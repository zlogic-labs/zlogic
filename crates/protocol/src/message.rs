use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Source {
    pub provider_id: String,
    pub model_id: String,
}

impl Source {
    pub fn new(provider_id: impl Into<String>, model_id: impl Into<String>) -> Self {
        Self {
            provider_id: provider_id.into(),
            model_id: model_id.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RawPolicy {
    Keep,
    Strip,
}

pub fn raw_policy(entry: Option<&Source>, target: &Source) -> RawPolicy {
    match entry {
        Some(src) if src == target => RawPolicy::Keep,
        _ => RawPolicy::Strip,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    pub content: Vec<ContentPart>,
}

impl Message {
    pub fn user(content: Vec<ContentPart>) -> Self {
        Self {
            role: Role::User,
            source: None,
            content,
        }
    }

    pub fn assistant(source: Source, content: Vec<ContentPart>) -> Self {
        Self {
            role: Role::Assistant,
            source: Some(source),
            content,
        }
    }

    pub fn tool(content: Vec<ContentPart>) -> Self {
        Self {
            role: Role::Tool,
            source: None,
            content,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Reasoning(ReasoningPart),
    Text(TextPart),
    ToolCall(ToolCallPart),
    ToolResult(ToolResultPart),
    Image(ImagePart),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReasoningPart {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<Value>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextPart {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<Value>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallPart {
    pub calls: Vec<ToolCall>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResultPart {
    pub call_id: String,
    pub name: String,
    pub content: String,
    /// Files produced by the tool. Their bytes live in the object store; this is the durable,
    /// MIME-aware description used to reconstruct model context after reopening a session.
    /// Keeping this beside `content` rather than flattening it into prose lets the context
    /// projector send supported images as media, inline bounded text files, and degrade every
    /// other MIME to an explicit placeholder without losing the underlying object.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<ToolResultFile>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_error: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResultFile {
    /// Human-readable file name or logical resource name. It is display metadata, never a path
    /// used for filesystem access.
    pub name: String,
    /// Normalized lowercase media type. Unknown data uses `application/octet-stream`.
    pub mime_type: String,
    /// Content-addressed object id, represented as a string to keep protocol independent from the
    /// object-store implementation.
    pub object_id: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImagePart {
    pub mime_type: String,
    pub source: ImageSource,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageSource {
    Path { path: String },
    Base64 { data: String },
}

impl ContentPart {
    pub fn strip_raw(&mut self) {
        match self {
            ContentPart::Reasoning(p) => p.raw = None,
            ContentPart::Text(p) => p.raw = None,
            ContentPart::ToolCall(p) => {
                for call in &mut p.calls {
                    call.raw = None;
                }
            }
            ContentPart::ToolResult(_) | ContentPart::Image(_) => {}
        }
    }
}

pub fn gate_message_for_target(msg: &mut Message, target: &Source) {
    if msg.role != Role::Assistant {
        return;
    }
    match raw_policy(msg.source.as_ref(), target) {
        RawPolicy::Keep => {}
        RawPolicy::Strip => {
            msg.content
                .retain(|p| !matches!(p, ContentPart::Reasoning(_)));
            for part in &mut msg.content {
                part.strip_raw();
            }
        }
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn assistant_with_raw(source: Source) -> Message {
        Message::assistant(
            source,
            vec![
                ContentPart::Reasoning(ReasoningPart {
                    text: "think".into(),
                    raw: Some(json!({ "signature": "sig" })),
                    truncated: false,
                }),
                ContentPart::Text(TextPart {
                    text: "answer".into(),
                    raw: Some(json!({ "thoughtSignature": "ts" })),
                    truncated: false,
                }),
                ContentPart::ToolCall(ToolCallPart {
                    calls: vec![ToolCall {
                        id: "call_1".into(),
                        name: "read_file".into(),
                        args: "{\"path\":\"a\"}".into(),
                        raw: Some(json!({ "itemId": "fc_1" })),
                    }],
                }),
            ],
        )
    }

    #[test]
    fn same_source_keeps_everything_byte_for_byte() {
        let target = Source::new("anthropic", "claude-opus-5");
        let mut msg = assistant_with_raw(target.clone());
        let before = msg.clone();
        gate_message_for_target(&mut msg, &target);
        assert_eq!(msg, before);
    }

    #[test]
    fn same_client_different_model_strips() {
        let mut msg = assistant_with_raw(Source::new("anthropic", "claude-opus-5"));
        gate_message_for_target(&mut msg, &Source::new("anthropic", "claude-sonnet-5"));

        assert!(
            !msg.content
                .iter()
                .any(|p| matches!(p, ContentPart::Reasoning(_)))
        );
        match &msg.content[0] {
            ContentPart::Text(t) => {
                assert_eq!(t.text, "answer");
                assert!(t.raw.is_none());
            }
            other => panic!("expected text part, got {other:?}"),
        }
        match &msg.content[1] {
            ContentPart::ToolCall(tc) => {
                assert_eq!(tc.calls[0].args, "{\"path\":\"a\"}");
                assert!(tc.calls[0].raw.is_none());
            }
            other => panic!("expected tool-call part, got {other:?}"),
        }
    }

    #[test]
    fn unknown_source_strips() {
        let mut msg = assistant_with_raw(Source::new("p", "m"));
        msg.source = None;
        gate_message_for_target(&mut msg, &Source::new("p", "m"));
        assert!(
            !msg.content
                .iter()
                .any(|p| matches!(p, ContentPart::Reasoning(_)))
        );
    }

    #[test]
    fn non_assistant_untouched() {
        let mut msg = Message::tool(vec![ContentPart::ToolResult(ToolResultPart {
            call_id: "call_1".into(),
            name: "read_file".into(),
            content: "ok".into(),
            is_error: false,
            files: Vec::new(),
        })]);
        let before = msg.clone();
        gate_message_for_target(&mut msg, &Source::new("p", "m"));
        assert_eq!(msg, before);
    }
}
