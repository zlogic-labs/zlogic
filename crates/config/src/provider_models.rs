//! The snapshot of what a subscription backend said this account may call.
//!
//! Which models a plan may use is the server's answer, so it is fetched rather than guessed. The
//! answer is not user configuration — it lives in the data directory next to the catalog and price
//! snapshots, and it is applied on top of the built-in provider instead of rewriting `models.yaml`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{Dirs, ModelSettings, ProviderSettings, Result, read_optional, write_json_file};

pub const FILE: &str = "provider-models.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderModelsFile {
    pub provider_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<String>,
    #[serde(default)]
    pub models: BTreeMap<String, ModelSettings>,
}

impl ProviderModelsFile {
    pub fn path(dirs: &Dirs) -> PathBuf {
        dirs.data.join(FILE)
    }

    pub fn read(dirs: &Dirs) -> Option<Self> {
        let file: Option<Self> = read_optional(&Self::path(dirs), &mut Vec::new())
            .ok()
            .flatten();
        file.filter(|f| !f.provider_id.trim().is_empty())
    }

    pub fn write(&self, dirs: &Dirs) -> Result<()> {
        write_json_file(&Self::path(dirs), self)
    }

    /// Replace what the built-in provider lists with what the backend answered.
    ///
    /// Models the user wrote down by hand survive: they are the one thing here that is an
    /// intention rather than an observation.
    pub fn apply(&self, provider: &mut ProviderSettings, declared: &BTreeSet<String>) {
        let mut models = self.models.clone();
        for (id, settings) in &provider.models {
            if declared.contains(id) {
                models.insert(id.clone(), settings.clone());
            }
        }
        provider.models = models;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(context: u64) -> ModelSettings {
        ModelSettings {
            context_window: Some(context),
            ..Default::default()
        }
    }

    fn provider(ids: &[&str]) -> ProviderSettings {
        ProviderSettings {
            models: ids.iter().map(|id| (id.to_string(), model(1))).collect(),
            ..Default::default()
        }
    }

    fn snapshot(ids: &[&str]) -> ProviderModelsFile {
        ProviderModelsFile {
            provider_id: "codex".into(),
            fetched_at: Some("2026-09-22T00:00:00Z".into()),
            models: ids.iter().map(|id| (id.to_string(), model(9))).collect(),
        }
    }

    #[test]
    fn the_fetched_list_replaces_the_built_in_floor() {
        let mut p = provider(&["gpt-5.5", "gone"]);
        snapshot(&["gpt-5.5", "new"]).apply(&mut p, &BTreeSet::new());
        let ids: Vec<&String> = p.models.keys().collect();
        assert_eq!(ids, ["gpt-5.5", "new"]);
        assert_eq!(
            p.models["gpt-5.5"].context_window,
            Some(9),
            "a model both sides know is described by the backend"
        );
    }

    #[test]
    fn a_model_the_user_declared_is_never_dropped() {
        let mut p = provider(&["gpt-5.5", "mine"]);
        snapshot(&["gpt-5.5"]).apply(&mut p, &BTreeSet::from(["mine".to_string()]));
        let ids: Vec<&String> = p.models.keys().collect();
        assert_eq!(ids, ["gpt-5.5", "mine"]);
        assert_eq!(p.models["mine"].context_window, Some(1));
    }

    #[test]
    fn a_snapshot_without_a_provider_is_ignored_rather_than_applied_to_nothing() {
        let dirs = Dirs::under(std::env::temp_dir().join("zlogic-provider-models-test"));
        assert!(
            ProviderModelsFile::read(&dirs).is_none(),
            "an absent snapshot must read as absent"
        );
    }
}
