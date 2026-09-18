use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

use crate::message::{ContentPart, Message};
use crate::usage::{Purpose, UsageReport};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmRequest {
    pub model: String,
    pub system: Vec<SystemPart>,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDefinition>,
    pub thinking: ThinkingIntent,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,
    #[serde(default)]
    pub cache: CacheSpec,
    pub meta: RequestMeta,
}

/// ```text
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemPart {
    pub text: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub cache: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheSpec {
    #[serde(default = "default_true")]
    pub tools: bool,
    #[serde(default = "default_true")]
    pub system: bool,
    #[serde(default)]
    pub messages: MessageCache,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_key: Option<String>,
}

impl Default for CacheSpec {
    fn default() -> Self {
        Self {
            tools: true,
            system: true,
            messages: MessageCache::LatestUser,
            ttl_seconds: None,
            prompt_key: None,
        }
    }
}

impl CacheSpec {
    pub fn off() -> Self {
        Self {
            tools: false,
            system: false,
            messages: MessageCache::None,
            ttl_seconds: None,
            prompt_key: None,
        }
    }

    pub fn is_off(&self) -> bool {
        !self.tools && !self.system && self.messages == MessageCache::None
    }

    pub fn wants_long_ttl(&self) -> bool {
        !self.is_off() && self.ttl_seconds.is_some_and(|s| s >= 3600)
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageCache {
    None,
    #[default]
    LatestUser,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ThinkingIntent {
    pub mode: ThinkingMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u32>,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingMode {
    On,
    Off,
    #[default]
    Default,
}

/// | | minimal | low | medium | high | xhigh | max |
/// |---|---|---|---|---|---|---|
/// | Anthropic Opus 5 / 4.8 / 4.7 / Sonnet 5 | | ✓ | ✓ | ✓ | ✓ | ✓ |
/// | Anthropic Opus 4.6 / Sonnet 4.6 | | ✓ | ✓ | ✓ | | ✓ |
/// | Gemini 3 flash / pro | ✓ / | ✓ | ✓ / | ✓ | | |
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    Minimal,
    Low,
    Medium,
    High,
    #[serde(rename = "xhigh")]
    XHigh,
    Max,
}

impl Effort {
    pub const LADDER: [Effort; 6] = [
        Effort::Minimal,
        Effort::Low,
        Effort::Medium,
        Effort::High,
        Effort::XHigh,
        Effort::Max,
    ];

    pub fn rank(self) -> u8 {
        match self {
            Effort::Minimal => 0,
            Effort::Low => 1,
            Effort::Medium => 2,
            Effort::High => 3,
            Effort::XHigh => 4,
            Effort::Max => 5,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Minimal => "minimal",
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::XHigh => "xhigh",
            Effort::Max => "max",
        }
    }

    pub fn from_wire(s: &str) -> Option<Effort> {
        Effort::LADDER.into_iter().find(|e| e.as_str() == s)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseFormat {
    Json,
    JsonSchema {
        name: String,
        schema: Value,
        #[serde(default, skip_serializing_if = "is_false")]
        strict: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestMeta {
    pub session_id: String,
    pub turn_id: String,
    pub round_id: String,
    pub purpose: Purpose,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LlmEvent {
    PartStart {
        index: u32,
        kind: PartKind,
    },
    PartDelta {
        index: u32,
        delta: String,
    },
    ToolCallDetected {
        index: u32,
        call_index: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        name: String,
    },
    PartEnd {
        index: u32,
        part: ContentPart,
    },
    Usage(UsageReport),
    Notice {
        code: String,
        message: String,
    },
    ResponseEnd {
        finish_reason: FinishReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PartKind {
    Reasoning,
    Text,
    ToolCall,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    #[default]
    Stop,
    ToolCalls,
    Length,
    ContentFilter,
    Other,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmError {
    pub kind: LlmErrorKind,
    pub retryable: bool,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmErrorKind {
    Auth,
    RateLimit,
    Quota,
    Server,
    Network,
    BadRequest,
    ContentPolicy,
    Aborted,
    ContextLengthExceeded,
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for LlmError {}

fn is_false(b: &bool) -> bool {
    !*b
}

//     trait LlmClient {
//         fn stream(&self, req: LlmRequest, cancel: CancelToken)
//             -> impl Stream<Item = Result<LlmEvent, LlmError>>;
//     }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_name_matches_the_wire_name() {
        for e in Effort::LADDER {
            let via_serde = serde_json::to_string(&e).expect("serialize");
            let via_serde = via_serde.trim_matches('"');
            assert_eq!(
                via_serde,
                e.as_str(),
                "{e:?} has a serde name that disagrees with its wire name —— config will not load, or the value is rejected on the way out"
            );
            let back: Effort =
                serde_json::from_str(&format!("\"{}\"", e.as_str())).expect("deserialize");
            assert_eq!(back, e);
        }
    }

    #[test]
    fn the_ladder_is_strictly_ordered() {
        let ranks: Vec<u8> = Effort::LADDER.iter().map(|e| e.rank()).collect();
        assert!(ranks.windows(2).all(|w| w[0] < w[1]), "{ranks:?}");
    }
}
