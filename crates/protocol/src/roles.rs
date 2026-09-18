//! ```yaml
//! llm_roles:
//!   title:
//!     thinking: off
//!   compaction:
//!     models: ["deepseek:deepseek-v4", main, session]
//!     params: { temperature: 0.3 }
//! ```

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SESSION: &str = "session";

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleSettings {
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub params: BTreeMap<String, Value>,
    pub thinking: Option<RoleThinking>,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleThinking {
    On,
    Off,
}

impl RoleThinking {
    pub fn is_on(self) -> bool {
        matches!(self, RoleThinking::On)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_role_reads_from_yaml_in_the_documented_shape() {
        let y = r#"
            models: [session, light]
            thinking: off
            params:
              temperature: 0.3
        "#;
        let r: RoleSettings = serde_yaml_ng::from_str(y).unwrap();
        assert_eq!(r.models, ["session", "light"]);
        assert_eq!(r.thinking, Some(RoleThinking::Off));
        assert_eq!(r.params["temperature"], 0.3);
    }

    #[test]
    fn yaml_off_parses_as_the_off_variant() {
        #[derive(Deserialize)]
        struct Wrap {
            thinking: RoleThinking,
        }
        assert_eq!(
            serde_yaml_ng::from_str::<Wrap>("thinking: off")
                .unwrap()
                .thinking,
            RoleThinking::Off
        );
        assert_eq!(
            serde_yaml_ng::from_str::<Wrap>("thinking: \"off\"")
                .unwrap()
                .thinking,
            RoleThinking::Off
        );
    }

    #[test]
    fn an_empty_role_is_valid_and_means_the_builtin_chain() {
        let r: RoleSettings = serde_yaml_ng::from_str("{}").unwrap();
        assert!(r.models.is_empty());
        assert!(r.thinking.is_none());
    }

    #[test]
    fn a_typo_in_a_role_is_rejected_rather_than_ignored() {
        assert!(serde_yaml_ng::from_str::<RoleSettings>("modles: [light]").is_err());
    }
}
