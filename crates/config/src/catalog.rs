use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{Dirs, ModelSettings, PriceFile, ProviderSettings, Result, write_json_file};

pub const SOURCE: &str = "https://zlogic.run/catalog/models.yaml";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CatalogFile {
    #[serde(default)]
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<String>,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderSettings>,
}

impl CatalogFile {
    pub fn path(dirs: &Dirs) -> PathBuf {
        dirs.data.join("catalog.json")
    }

    pub fn read(dirs: &Dirs) -> Option<Self> {
        let path = Self::path(dirs);
        let text = std::fs::read_to_string(&path).ok()?;
        match serde_json::from_str::<Self>(&text) {
            Ok(file) => Some(file),
            Err(e) => {
                tracing::warn!(target: "zlogic::config", path = %path.display(), "catalog snapshot failed to parse; ignoring: {e}");
                None
            }
        }
    }

    pub fn write(&self, dirs: &Dirs) -> Result<()> {
        write_json_file(&Self::path(dirs), self)
    }

    pub fn model_count(&self) -> u32 {
        self.providers.values().map(|p| p.models.len() as u32).sum()
    }

    /// |---|---|
    pub fn merge_into(&self, base: &mut CatalogFile) -> MergeReport {
        let mut report = MergeReport::default();

        for (pid, incoming) in &self.providers {
            match base.providers.get_mut(pid) {
                Some(existing) => {
                    for (mid, model) in &incoming.models {
                        match existing.models.insert(mid.clone(), model.clone()) {
                            Some(_) => report.updated_models += 1,
                            None => report.added_models += 1,
                        }
                    }
                    let mut incoming = incoming.clone();
                    incoming.models = std::mem::take(&mut existing.models);
                    incoming.enabled = existing.enabled;
                    incoming.credential_ok = existing.credential_ok;
                    *existing = incoming;
                }
                None => {
                    base.providers.insert(pid.clone(), incoming.clone());
                    report.added_providers.push(pid.clone());
                }
            }
        }

        for (pid, p) in &base.providers {
            match self.providers.get(pid) {
                None => report.dropped_providers.push(pid.clone()),
                Some(incoming) => {
                    for mid in p.models.keys() {
                        if !incoming.models.contains_key(mid) {
                            report.dropped_models.push(format!("{pid}:{mid}"));
                        }
                    }
                }
            }
        }
        report
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergeReport {
    pub added_providers: Vec<String>,
    pub added_models: u32,
    pub updated_models: u32,
    pub dropped_providers: Vec<String>,
    pub dropped_models: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    Newer,
    Same,
    Older,
}

pub fn freshness(candidate: &str, current: &str) -> Freshness {
    if candidate == current {
        return Freshness::Same;
    }
    match (date_prefix(candidate), date_prefix(current)) {
        (Some(server), Some(mine)) if server < mine => Freshness::Older,
        _ => Freshness::Newer,
    }
}

fn date_prefix(version: &str) -> Option<&str> {
    let head = version.get(..10)?;
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    (head.as_bytes()[4] == b'-'
        && head.as_bytes()[7] == b'-'
        && digits(&head[..4])
        && digits(&head[5..7])
        && digits(&head[8..10]))
    .then_some(head)
}

pub fn effective_version(builtin: &CatalogFile, snapshot: Option<&CatalogFile>) -> String {
    match snapshot {
        Some(s) if freshness(&s.version, &builtin.version) != Freshness::Older => s.version.clone(),
        _ => builtin.version.clone(),
    }
}

#[derive(Debug, Clone)]
pub struct EffectiveCatalog {
    pub catalog: CatalogFile,
    pub version: String,
    pub warnings: Vec<String>,
}

pub fn apply_snapshots(
    mut builtin: CatalogFile,
    snapshot: Option<&CatalogFile>,
    prices: Option<&PriceFile>,
) -> EffectiveCatalog {
    let mut warnings = Vec::new();
    let mut version = builtin.version.clone();

    if let Some(snapshot) = snapshot {
        if freshness(&snapshot.version, &builtin.version) == Freshness::Older {
            warnings.push(format!(
                "catalog snapshot {} is older than the catalog built into this version ({}); \
                 ignoring it — check for updates again",
                snapshot.version, builtin.version
            ));
        } else {
            let report = snapshot.merge_into(&mut builtin);
            version = snapshot.version.clone();
            if let Some(note) = dropped_note(&report) {
                warnings.push(note);
            }
        }
    }

    if let Some(prices) = prices {
        prices.apply(&mut builtin);
    }

    EffectiveCatalog {
        catalog: builtin,
        version,
        warnings,
    }
}

fn dropped_note(report: &MergeReport) -> Option<String> {
    if report.dropped_providers.is_empty() && report.dropped_models.is_empty() {
        return None;
    }
    let mut names: Vec<&str> = report
        .dropped_providers
        .iter()
        .map(String::as_str)
        .chain(report.dropped_models.iter().map(String::as_str))
        .collect();
    let total = names.len();
    const SHOWN: usize = 8;
    let rest = if total > SHOWN {
        names.truncate(SHOWN);
        format!(" (and {} more)", total - SHOWN)
    } else {
        String::new()
    };
    Some(format!(
        "the catalog on {} no longer lists {}{rest}; they are kept locally so existing \
         references keep working",
        SOURCE,
        names.join(", ")
    ))
}

pub fn parse_snapshot(body: &str) -> std::result::Result<(CatalogFile, Vec<String>), String> {
    let mut top: BTreeMap<String, serde_yaml_ng::Value> =
        serde_yaml_ng::from_str(body).map_err(|e| format!("not a YAML mapping: {e}"))?;

    let mut take = |key: &str| {
        top.remove(key)
            .and_then(|v| match v {
                serde_yaml_ng::Value::String(s) => Some(s),
                _ => None,
            })
            .filter(|s: &String| !s.trim().is_empty())
    };
    let version = take("version").ok_or("the catalog has no version")?;
    let generated_at = take("generated_at");
    let source = take("source");

    let raw = top
        .remove("providers")
        .ok_or("the catalog has no providers section")?;
    let raw: BTreeMap<String, serde_yaml_ng::Value> =
        serde_yaml_ng::from_value(raw).map_err(|e| format!("providers is not a mapping: {e}"))?;

    let mut providers = BTreeMap::new();
    let mut rejected = Vec::new();
    for (id, value) in raw {
        match loose_provider(value) {
            Ok(provider) => {
                providers.insert(id, provider);
            }
            Err(e) => rejected.push(format!("{id}: {}", one_line(&e))),
        }
    }
    if providers.is_empty() {
        return Err(format!(
            "no provider could be read from the catalog ({} rejected) — its shape may have changed",
            rejected.len()
        ));
    }

    Ok((
        CatalogFile {
            version,
            generated_at,
            source,
            fetched_at: None,
            providers,
        },
        rejected,
    ))
}

fn loose_provider(value: serde_yaml_ng::Value) -> std::result::Result<ProviderSettings, String> {
    let serde_yaml_ng::Value::Mapping(mut map) = value else {
        return Err("not a mapping".to_string());
    };
    let raw_models = map.remove("models");
    let provider: ProviderSettings =
        serde_yaml_ng::from_value(serde_yaml_ng::Value::Mapping(map)).map_err(|e| e.to_string())?;

    let Some(raw_models) = raw_models else {
        return Ok(provider);
    };
    let models: BTreeMap<String, serde_yaml_ng::Value> =
        serde_yaml_ng::from_value(raw_models).map_err(|e| format!("models: {e}"))?;

    let mut provider = provider;
    for (mid, value) in models {
        match serde_yaml_ng::from_value::<ModelSettings>(value) {
            Ok(model) => {
                provider.models.insert(mid, model);
            }
            Err(e) => tracing::warn!(
                target: "zlogic::config",
                "skipping model {mid}: {e}"
            ),
        }
    }
    Ok(provider)
}

fn one_line(error: &str) -> String {
    error.lines().next().unwrap_or(error).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ModelSettings;

    fn catalog(version: &str, models: &[(&str, &str)]) -> CatalogFile {
        let mut providers: BTreeMap<String, ProviderSettings> = BTreeMap::new();
        for (pid, mid) in models {
            providers
                .entry((*pid).to_string())
                .or_insert_with(|| ProviderSettings {
                    sdk: Some(zlogic_protocol::config::Sdk::DeepSeek),
                    ..Default::default()
                })
                .models
                .insert((*mid).to_string(), ModelSettings::default());
        }
        CatalogFile {
            version: version.into(),
            providers,
            ..Default::default()
        }
    }

    #[test]
    fn the_date_prefix_decides_which_side_is_newer() {
        assert_eq!(
            freshness("2026-08-05.aaaa", "2026-07-30.bbbb"),
            Freshness::Newer
        );
        assert_eq!(
            freshness("2026-07-30.bbbb", "2026-08-05.aaaa"),
            Freshness::Older,
            "a snapshot was pulled and then the program upgraded —— this is exactly that scenario"
        );
    }

    #[test]
    fn the_same_date_with_a_different_body_counts_as_newer() {
        assert_eq!(
            freshness("2026-08-05.aaaa", "2026-08-05.bbbb"),
            Freshness::Newer
        );
        assert_eq!(
            freshness("2026-08-05.aaaa", "2026-08-05.aaaa"),
            Freshness::Same
        );
    }

    #[test]
    fn an_unrecognised_version_still_compares() {
        assert_eq!(freshness("v2", "v1"), Freshness::Newer);
        assert_eq!(freshness("v1", "v1"), Freshness::Same);
        assert_eq!(
            freshness("catalog-β", "catalog-α"),
            Freshness::Newer,
            "multibyte must not panic either"
        );
    }

    #[test]
    fn an_older_snapshot_is_ignored_rather_than_applied() {
        let builtin = catalog("2026-08-05.new", &[("deepseek", "v4.1-flash")]);
        let snapshot = catalog("2026-07-30.old", &[("deepseek", "v4-flash")]);

        let effective = apply_snapshots(builtin, Some(&snapshot), None);
        assert_eq!(effective.version, "2026-08-05.new");
        assert!(
            effective.catalog.providers["deepseek"]
                .models
                .contains_key("v4.1-flash"),
            "the built-in model must not be swapped out by an older snapshot"
        );
        assert_eq!(effective.warnings.len(), 1, "this must be surfaced");
    }

    #[test]
    fn newer_models_and_providers_are_added() {
        let builtin = catalog("2026-07-30.old", &[("deepseek", "v4-flash")]);
        let snapshot = catalog(
            "2026-08-05.new",
            &[("deepseek", "v4.1-flash"), ("zai", "glm-5.2")],
        );

        let effective = apply_snapshots(builtin, Some(&snapshot), None);
        let deepseek = &effective.catalog.providers["deepseek"];
        assert!(
            deepseek.models.contains_key("v4.1-flash"),
            "new models must come in"
        );
        assert!(
            deepseek.models.contains_key("v4-flash"),
            "a model the server no longer lists must be kept —— a ref that names it cannot vanish"
        );
        assert!(
            effective.catalog.providers.contains_key("zai"),
            "new providers must come in"
        );
        assert_eq!(effective.version, "2026-08-05.new");
        assert!(
            effective.warnings.iter().any(|w| w.contains("v4-flash")),
            "whatever is kept must be reported: {:#?}",
            effective.warnings
        );
    }

    #[test]
    fn prices_are_applied_on_top_of_the_catalog_snapshot() {
        let builtin = catalog("2026-07-30.old", &[("deepseek", "v4-flash")]);
        let mut snapshot = catalog("2026-08-05.new", &[("deepseek", "v4-flash")]);
        snapshot
            .providers
            .get_mut("deepseek")
            .unwrap()
            .models
            .insert(
                "v4-flash".into(),
                ModelSettings {
                    context_window: Some(123_456),
                    ..Default::default()
                },
            );

        let mut prices = BTreeMap::new();
        prices.insert(
            "deepseek:v4-flash".to_string(),
            zlogic_protocol::config::Pricing {
                input_per_m: 0.28,
                output_per_m: 0.42,
                cached_input_per_m: None,
                cache_write_per_m: None,
                currency: "USD".into(),
            },
        );
        let prices = PriceFile {
            source: crate::prices::SOURCE.into(),
            fetched_at: "2026-08-06T00:00:00Z".into(),
            prices,
        };

        let effective = apply_snapshots(builtin, Some(&snapshot), Some(&prices));
        let model = &effective.catalog.providers["deepseek"].models["v4-flash"];
        assert_eq!(
            model.context_window,
            Some(123_456),
            "the snapshot's window must take effect"
        );
        assert_eq!(model.pricing.as_ref().unwrap().input_per_m, 0.28);
    }

    #[test]
    fn one_unreadable_provider_does_not_take_the_whole_catalog_down() {
        let body = "\
version: 2026-08-05.aaaa
generated_at: 2026-08-05
source: https://models.dev/api.json
providers:
  deepseek:
    sdk: deepseek
    models:
      v4.1-flash: {context_window: 128000}
  future:
    sdk: deepseek
    models: {}
    a_field_from_the_future: true
";
        let (catalog, rejected) = parse_snapshot(body).unwrap();
        assert!(catalog.providers.contains_key("deepseek"));
        assert!(!catalog.providers.contains_key("future"));
        assert_eq!(rejected.len(), 1);
        assert!(rejected[0].starts_with("future:"), "{rejected:?}");
    }

    #[test]
    fn an_unreadable_model_only_costs_that_model() {
        let body = "\
version: 2026-08-05.aaaa
providers:
  deepseek:
    sdk: deepseek
    models:
      v4-flash: {context_window: 128000}
      v4.1-flash:
        context_window: 128000
        a_field_from_the_future: 1
";
        let (catalog, rejected) = parse_snapshot(body).unwrap();
        assert!(
            rejected.is_empty(),
            "the provider itself is not broken: {rejected:?}"
        );
        let models = &catalog.providers["deepseek"].models;
        assert!(models.contains_key("v4-flash"));
        assert!(!models.contains_key("v4.1-flash"));
    }

    #[test]
    fn a_catalog_without_a_version_is_refused() {
        let body = "providers:\n  deepseek:\n    sdk: deepseek\n";
        let err = parse_snapshot(body).unwrap_err();
        assert!(err.contains("version"), "{err}");
    }

    #[test]
    fn a_catalog_whose_shape_changed_is_refused() {
        let body = "version: 2026-08-05.aaaa\nproviders: []\n";
        assert!(parse_snapshot(body).is_err());
    }

    #[test]
    fn a_snapshot_round_trips_through_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        let mut file = catalog("2026-08-05.aaaa", &[("deepseek", "v4.1-flash")]);
        file.fetched_at = Some("2026-08-06T00:00:00Z".into());

        file.write(&dirs).unwrap();
        let back = CatalogFile::read(&dirs).expect("should read back");
        assert_eq!(back.version, "2026-08-05.aaaa");
        assert_eq!(back.fetched_at.as_deref(), Some("2026-08-06T00:00:00Z"));
        assert_eq!(back.model_count(), 1);
    }

    #[test]
    fn a_corrupt_snapshot_is_ignored_rather_than_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        std::fs::create_dir_all(&dirs.data).unwrap();
        std::fs::write(CatalogFile::path(&dirs), "{ not json").unwrap();
        assert!(CatalogFile::read(&dirs).is_none());
    }
}
