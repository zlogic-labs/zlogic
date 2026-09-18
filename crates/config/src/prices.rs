use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use zlogic_protocol::config::Pricing;

use crate::{CatalogFile, Dirs, Result, write_json_file};

pub const SOURCE: &str = "https://models.dev/api.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceFile {
    pub source: String,
    pub fetched_at: String,
    pub prices: BTreeMap<String, Pricing>,
}

impl PriceFile {
    pub fn path(dirs: &Dirs) -> PathBuf {
        dirs.data.join("prices.json")
    }

    pub fn read(dirs: &Dirs) -> Option<Self> {
        let path = Self::path(dirs);
        let text = std::fs::read_to_string(&path).ok()?;
        match serde_json::from_str::<Self>(&text) {
            Ok(file) => Some(file),
            Err(e) => {
                tracing::warn!(target: "zlogic::config", path = %path.display(), "price snapshot failed to parse; ignoring: {e}");
                None
            }
        }
    }

    pub fn write(&self, dirs: &Dirs) -> Result<()> {
        write_json_file(&Self::path(dirs), self)
    }

    pub fn apply(&self, catalog: &mut CatalogFile) -> u32 {
        let mut applied = 0;
        for (provider_id, provider) in catalog.providers.iter_mut() {
            for (model_id, model) in provider.models.iter_mut() {
                if let Some(pricing) = self.prices.get(&format!("{provider_id}:{model_id}")) {
                    model.pricing = Some(pricing.clone());
                    applied += 1;
                }
            }
        }
        applied
    }
}

pub fn source_provider_id(our_id: &str) -> Option<&'static str> {
    Some(match our_id {
        "anthropic" => "anthropic",
        "openai" => "openai",
        "gemini" => "google",
        "deepseek" => "deepseek",
        "glm" => "zhipuai",
        "dashscope" => "alibaba",
        "openrouter" => "openrouter",
        "xai" => "xai",
        "groq" => "groq",
        _ => return None,
    })
}

pub fn prices_from_models_dev(api: &Value, known: &CatalogFile) -> BTreeMap<String, Pricing> {
    let mut out = BTreeMap::new();
    for (our_id, provider) in &known.providers {
        let Some(source_id) = source_provider_id(our_id) else {
            continue;
        };
        let Some(models) = api.get(source_id).and_then(|p| p.get("models")) else {
            continue;
        };
        for model_id in provider.models.keys() {
            let Some(cost) = models.get(model_id).and_then(|m| m.get("cost")) else {
                continue;
            };
            let num = |key: &str| cost.get(key).and_then(Value::as_f64);
            let (Some(input_per_m), Some(output_per_m)) = (num("input"), num("output")) else {
                continue;
            };
            out.insert(
                format!("{our_id}:{model_id}"),
                Pricing {
                    input_per_m,
                    output_per_m,
                    cached_input_per_m: num("cache_read"),
                    cache_write_per_m: num("cache_write"),
                    currency: "USD".into(),
                },
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> CatalogFile {
        serde_yaml_ng::from_str(crate::CATALOG).unwrap()
    }

    #[test]
    fn every_catalog_provider_has_a_source_mapping() {
        for provider_id in catalog().providers.keys() {
            assert!(
                source_provider_id(provider_id).is_some(),
                "{provider_id} is in the built-in catalog but source_provider_id does not know it —— \
                 add a mapping (the models.dev provider key, see scripts/gen-model-catalog.py)"
            );
        }
    }

    #[test]
    fn prices_are_read_for_the_models_we_actually_have() {
        let api = serde_json::json!({
            "anthropic": {
                "models": {
                    "claude-opus-5": {
                        "cost": { "input": 4.0, "output": 20.0, "cache_read": 0.4 }
                    },
                    "claude-experimental-9": { "cost": { "input": 1.0, "output": 2.0 } }
                }
            }
        });
        let mut known = CatalogFile::default();
        let mut anthropic = crate::ProviderSettings::default();
        anthropic
            .models
            .insert("claude-opus-5".into(), Default::default());
        known.providers.insert("anthropic".into(), anthropic);

        let prices = prices_from_models_dev(&api, &known);
        assert_eq!(prices.len(), 1);
        let p = &prices["anthropic:claude-opus-5"];
        assert_eq!(p.input_per_m, 4.0);
        assert_eq!(p.output_per_m, 20.0);
        assert_eq!(p.cached_input_per_m, Some(0.4));
        assert_eq!(p.cache_write_per_m, None);
        assert_eq!(p.currency, "USD");
    }

    #[test]
    fn a_renamed_provider_is_matched_through_the_mapping() {
        let api = serde_json::json!({
            "google": { "models": { "gemini-3-pro": { "cost": { "input": 1.0, "output": 8.0 } } } }
        });
        let mut known = CatalogFile::default();
        let mut gemini = crate::ProviderSettings::default();
        gemini
            .models
            .insert("gemini-3-pro".into(), Default::default());
        known.providers.insert("gemini".into(), gemini);

        assert!(
            prices_from_models_dev(&api, &known).contains_key("gemini:gemini-3-pro"),
            "gemini's prices hang off google in models.dev"
        );
    }

    #[test]
    fn a_half_priced_entry_is_skipped_rather_than_half_filled() {
        let api = serde_json::json!({
            "deepseek": { "models": { "deepseek-v4-pro": { "cost": { "input": 0.4 } } } }
        });
        let mut known = CatalogFile::default();
        let mut ds = crate::ProviderSettings::default();
        ds.models
            .insert("deepseek-v4-pro".into(), Default::default());
        known.providers.insert("deepseek".into(), ds);

        assert!(prices_from_models_dev(&api, &known).is_empty());
    }

    #[test]
    fn applying_a_snapshot_replaces_the_catalog_price() {
        let mut cat = catalog();
        let (provider_id, model_id) = {
            let (pid, provider) = cat.providers.iter().next().unwrap();
            (pid.clone(), provider.models.keys().next().unwrap().clone())
        };
        let file = PriceFile {
            source: SOURCE.into(),
            fetched_at: "2026-07-30T00:00:00Z".into(),
            prices: BTreeMap::from([(
                format!("{provider_id}:{model_id}"),
                Pricing {
                    input_per_m: 123.0,
                    output_per_m: 456.0,
                    cached_input_per_m: None,
                    cache_write_per_m: None,
                    currency: "USD".into(),
                },
            )]),
        };

        assert_eq!(file.apply(&mut cat), 1);
        let applied = cat.providers[&provider_id].models[&model_id]
            .pricing
            .as_ref()
            .unwrap();
        assert_eq!(applied.input_per_m, 123.0);
    }

    #[test]
    fn a_snapshot_entry_for_an_unknown_model_is_ignored() {
        let mut cat = catalog();
        let before = cat.providers.len();
        let file = PriceFile {
            source: SOURCE.into(),
            fetched_at: "2026-07-30T00:00:00Z".into(),
            prices: BTreeMap::from([(
                "nobody:nothing".into(),
                Pricing {
                    input_per_m: 1.0,
                    output_per_m: 1.0,
                    cached_input_per_m: None,
                    cache_write_per_m: None,
                    currency: "USD".into(),
                },
            )]),
        };
        assert_eq!(file.apply(&mut cat), 0);
        assert_eq!(cat.providers.len(), before);
    }

    #[test]
    fn a_corrupt_snapshot_is_ignored_rather_than_fatal() {
        let home = tempfile::tempdir().unwrap();
        let dirs = Dirs {
            config: home.path().join("config"),
            data: home.path().join("data"),
            state: home.path().join("state"),
            cache: home.path().join("cache"),
        };
        std::fs::create_dir_all(&dirs.data).unwrap();
        std::fs::write(PriceFile::path(&dirs), "{ not json").unwrap();
        assert!(PriceFile::read(&dirs).is_none());
    }

    #[test]
    fn a_snapshot_round_trips_through_disk() {
        let home = tempfile::tempdir().unwrap();
        let dirs = Dirs {
            config: home.path().join("config"),
            data: home.path().join("data"),
            state: home.path().join("state"),
            cache: home.path().join("cache"),
        };
        let file = PriceFile {
            source: SOURCE.into(),
            fetched_at: "2026-07-30T00:00:00Z".into(),
            prices: BTreeMap::from([(
                "anthropic:opus".into(),
                Pricing {
                    input_per_m: 3.0,
                    output_per_m: 15.0,
                    cached_input_per_m: Some(0.3),
                    cache_write_per_m: Some(3.75),
                    currency: "USD".into(),
                },
            )]),
        };
        file.write(&dirs).unwrap();
        let back = PriceFile::read(&dirs).unwrap();
        assert_eq!(back.prices["anthropic:opus"].input_per_m, 3.0);
        assert_eq!(back.fetched_at, file.fetched_at);
        let leftovers: Vec<_> = std::fs::read_dir(&dirs.data)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }
}
