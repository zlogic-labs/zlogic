use serde_json::json;
use zlogic_protocol::config::{ClientSpec, GenericOpenAiDialect, Sdk, resolve_client};
use zlogic_protocol::input::MessagePart;
use zlogic_protocol::llm::{FinishReason, LlmEvent, PartKind};
use zlogic_protocol::message::{ContentPart, ReasoningPart, ToolCall, ToolCallPart};
use zlogic_protocol::stream::{AgentRef, BlockFinal, StreamEvent, StreamPayload, UiToolCall};
use zlogic_protocol::usage::{TokenUsage, UsageReport};

fn round_trip<T>(value: &T, expected: serde_json::Value)
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let encoded = serde_json::to_value(value).expect("serialize");
    assert_eq!(encoded, expected, "wire shape drifted");
    let decoded: T = serde_json::from_value(encoded).expect("deserialize");
    assert_eq!(&decoded, value, "round-trip lost information");
}

#[test]
fn llm_part_end_tool_call_keeps_args_verbatim() {
    let event = LlmEvent::PartEnd {
        index: 2,
        part: ContentPart::ToolCall(ToolCallPart {
            calls: vec![ToolCall {
                id: "call_1".into(),
                name: "read_file".into(),
                args: "{\"path\": \"a.rs\", \"start_line\": 1}".into(),
                raw: Some(json!({ "thoughtSignature": "Cs8B" })),
            }],
        }),
    };

    round_trip(
        &event,
        json!({
            "type": "part_end",
            "index": 2,
            "part": {
                "type": "tool_call",
                "calls": [{
                    "id": "call_1",
                    "name": "read_file",
                    "args": "{\"path\": \"a.rs\", \"start_line\": 1}",
                    "raw": { "thoughtSignature": "Cs8B" }
                }]
            }
        }),
    );
}

#[test]
fn reasoning_without_raw_omits_the_field() {
    let part = ContentPart::Reasoning(ReasoningPart {
        text: "analysis…".into(),
        raw: None,
        truncated: false,
    });
    round_trip(&part, json!({ "type": "reasoning", "text": "analysis…" }));
}

#[test]
fn llm_lifecycle_events() {
    round_trip(
        &LlmEvent::PartStart {
            index: 0,
            kind: PartKind::Reasoning,
        },
        json!({ "type": "part_start", "index": 0, "kind": "reasoning" }),
    );
    round_trip(
        &LlmEvent::ToolCallDetected {
            index: 1,
            call_index: 0,
            id: None,
            name: "shell".into(),
        },
        json!({ "type": "tool_call_detected", "index": 1, "call_index": 0, "name": "shell" }),
    );
    round_trip(
        &LlmEvent::ResponseEnd {
            finish_reason: FinishReason::ToolCalls,
        },
        json!({ "type": "response_end", "finish_reason": "tool_calls" }),
    );
}

#[test]
fn usage_report_flattens_into_the_event() {
    round_trip(
        &LlmEvent::Usage(UsageReport {
            tokens: TokenUsage {
                input: 1200,
                output: 300,
                cache_read: Some(1000),
                ..Default::default()
            },
            cost: None,
            raw: None,
        }),
        json!({
            "type": "usage",
            "tokens": { "input": 1200, "output": 300, "cache_read": 1000 }
        }),
    );
}

#[test]
fn stream_block_end_has_no_raw_channel() {
    let event = StreamEvent {
        seq: 7,
        session_id: "s1".into(),
        turn_id: "t1".into(),
        agent: AgentRef::root(),
        payload: StreamPayload::BlockEnd {
            block_id: "b1".into(),
            block: BlockFinal::ToolCall {
                calls: vec![UiToolCall {
                    call_id: "call_1".into(),
                    name: "shell".into(),
                    args: "{\"cmd\":\"ls\"}".into(),
                }],
            },
        },
    };

    let encoded = serde_json::to_string(&event).unwrap();
    assert!(
        !encoded.contains("\"raw\""),
        "raw must never appear in the UI stream: {encoded}"
    );

    round_trip(
        &event,
        json!({
            "seq": 7,
            "session_id": "s1",
            "turn_id": "t1",
            "agent": { "name": "main" },
            "payload": {
                "type": "block_end",
                "block_id": "b1",
                "block": {
                    "type": "tool_call",
                    "calls": [{ "call_id": "call_1", "name": "shell", "args": "{\"cmd\":\"ls\"}" }]
                }
            }
        }),
    );
}

#[test]
fn client_spec_scopes_the_escape_hatch() {
    let dialect = GenericOpenAiDialect {
        reasoning_carrier: Some("reasoning_content".into()),
        think_tags: true,
        ..Default::default()
    };
    let spec = resolve_client(Sdk::OpenAiGeneric, Some(&dialect)).unwrap();
    round_trip(
        &spec,
        json!({
            "client": "openai_generic",
            "reasoning_carrier": "reasoning_content",
            "think_tags": true,
            "usage_fields": {}
        }),
    );

    round_trip(
        &ClientSpec::Builtin { sdk: Sdk::DeepSeek },
        json!({ "client": "builtin", "sdk": "deepseek" }),
    );
}

#[test]
fn uploaded_attachment_submission_uses_an_object_id() {
    round_trip(
        &MessagePart::Attachment {
            object_id: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .into(),
            name: "photo.png".into(),
            mime_type: "image/png".into(),
            bytes: 42,
        },
        json!({
            "type": "attachment",
            "object_id": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "name": "photo.png",
            "mime_type": "image/png",
            "bytes": 42
        }),
    );
}

#[test]
fn skill_reference_load_and_invocation_have_distinct_wire_shapes() {
    round_trip(
        &MessagePart::Skill {
            name: "acme:review".into(),
            args: None,
        },
        json!({ "type": "skill", "name": "acme:review" }),
    );
    round_trip(
        &MessagePart::SkillLoad {
            name: "acme:review".into(),
            revision: "abc123".into(),
            body_object: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .into(),
            path: "/skills/review/SKILL.md".into(),
            loaded_by: zlogic_protocol::SkillLoadSource::User,
            unsupported: Vec::new(),
        },
        json!({
            "type": "skill_load",
            "name": "acme:review",
            "revision": "abc123",
            "body_object": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "path": "/skills/review/SKILL.md",
            "loaded_by": "user"
        }),
    );
    round_trip(
        &MessagePart::SkillInvocation {
            name: "acme:review".into(),
            args: Some("strict".into()),
        },
        json!({
            "type": "skill_invocation",
            "name": "acme:review",
            "args": "strict"
        }),
    );
    round_trip(
        &MessagePart::SkillUnload {
            name: "acme:review".into(),
        },
        json!({
            "type": "skill_unload",
            "name": "acme:review"
        }),
    );
}
