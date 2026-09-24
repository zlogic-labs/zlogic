//! The model catalog a subscription may use.
//!
//! Which models a plan may call is the server's answer, not a client constant: the same backend
//! that answers a turn lists what is callable, per plan and per account. This is that call.

use serde::{Deserialize, Serialize};

use crate::oauth::Codex;
use crate::{Error, Tokens};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelInfo {
    pub slug: String,
    pub display_name: String,
    pub description: Option<String>,
    /// `list` shows in a picker; `hide` is callable but not offered.
    pub visibility: Option<String>,
    pub supported_in_api: Option<bool>,
    pub priority: Option<i64>,
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub supported_reasoning_levels: Vec<ReasoningLevel>,
    pub supports_image_detail_original: Option<bool>,
    pub supports_parallel_tool_calls: Option<bool>,
    pub supports_reasoning_summaries: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReasoningLevel {
    pub effort: String,
    pub description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Response {
    #[serde(default)]
    models: Vec<ModelInfo>,
}

impl ModelInfo {
    /// Whether a picker should offer this model.
    pub fn is_listed(&self) -> bool {
        !matches!(
            self.visibility.as_deref(),
            Some("hide") | Some("hidden") | Some("none")
        )
    }

    pub fn efforts(&self) -> Vec<String> {
        let mut efforts: Vec<String> = self
            .supported_reasoning_levels
            .iter()
            .map(|level| level.effort.clone())
            .filter(|effort| !effort.trim().is_empty())
            .collect();
        efforts.sort();
        efforts.dedup();
        efforts
    }
}

impl Codex {
    /// The models this subscription may call, most preferred first.
    pub async fn models(&self, tokens: &Tokens) -> Result<Vec<ModelInfo>, Error> {
        let config = self.config();
        let endpoint = "/models";
        let url = format!("{}{endpoint}", config.backend);
        let response = self
            .http_get(&url, tokens)
            .query(&[("client_version", config.client_version.as_str())])
            .send()
            .await
            .map_err(|e| Error::Network {
                url: url.clone(),
                reason: e.to_string(),
            })?;

        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        if !(200..300).contains(&status) {
            return Err(Error::Status {
                endpoint: url,
                status,
                body: body.chars().take(400).collect(),
            });
        }
        let parsed: Response = serde_json::from_str(&body).map_err(|e| {
            Error::Protocol(format!(
                "{url} did not answer with a model list: {e}; body: {}",
                body.chars().take(200).collect::<String>()
            ))
        })?;

        let mut models: Vec<ModelInfo> = parsed
            .models
            .into_iter()
            .filter(|m| !m.slug.trim().is_empty() && m.is_listed())
            .collect();
        models.sort_by_key(|m| (m.priority.unwrap_or(i64::MAX), m.slug.clone()));
        Ok(models)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fixture_shape_parses_without_the_fields_we_do_not_read() {
        let body = r#"{"models":[{
            "slug":"gpt-test",
            "display_name":"gpt-test",
            "description":"desc",
            "default_reasoning_level":"medium",
            "supported_reasoning_levels":[{"effort":"low","description":"low"},{"effort":"high","description":"high"}],
            "shell_type":"shell_command",
            "visibility":"list",
            "minimal_client_version":[0,99,0],
            "supported_in_api":true,
            "priority":1,
            "upgrade":null,
            "base_instructions":"base instructions",
            "supports_reasoning_summaries":false,
            "support_verbosity":false,
            "default_verbosity":null,
            "apply_patch_tool_type":null,
            "truncation_policy":{"mode":"bytes","limit":10000},
            "supports_parallel_tool_calls":false,
            "supports_image_detail_original":false,
            "context_window":272000,
            "experimental_supported_tools":[]
        }]}"#;
        let response: Response = serde_json::from_str(body).unwrap();
        let model = &response.models[0];
        assert_eq!(model.slug, "gpt-test");
        assert_eq!(model.context_window, Some(272_000));
        assert_eq!(model.efforts(), ["high", "low"], "sorted and deduped");
        assert!(model.is_listed());
    }

    #[test]
    fn a_hidden_model_is_not_offered_but_is_not_lost_here() {
        let mut model = ModelInfo {
            visibility: Some("hide".into()),
            ..Default::default()
        };
        assert!(!model.is_listed());
        model.visibility = None;
        assert!(model.is_listed(), "an unstated visibility is visible");
        model.visibility = Some("LIST".into());
        assert!(
            model.is_listed(),
            "only the exact hide values count as hidden"
        );
    }

    #[test]
    fn a_model_without_a_slug_is_dropped_by_the_caller() {
        let response: Response =
            serde_json::from_str(r#"{"models":[{"slug":""},{"slug":"ok"}]}"#).unwrap();
        let kept: Vec<&ModelInfo> = response
            .models
            .iter()
            .filter(|m| !m.slug.trim().is_empty())
            .collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].slug, "ok");
    }
}
