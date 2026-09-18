use std::sync::Arc;

use futures::StreamExt;
use serde_json::json;
use zlogic_llm::anthropic::AnthropicClient;
use zlogic_llm::bedrock::BedrockClient;
use zlogic_llm::bedrock::sigv4::Credentials;
use zlogic_llm::chat::ChatClient;
use zlogic_llm::chat::generic::GenericOpenAi;
use zlogic_llm::chat::vendors::{
    DashScope, DeepSeek, Fireworks, Glm, OpenRouter, QwenLocal, VanillaOpenAi,
};
use zlogic_llm::gemini::GeminiClient;
use zlogic_llm::mock::{MockClient, MockScript};
use zlogic_llm::responses::ResponsesClient;
use zlogic_llm::transport::{
    FailingTransport, HttpTransport, RecordingTransport, ReplayTransport, TruncatingTransport,
};
use zlogic_llm::{Endpoint, LlmClient};
use zlogic_protocol::config::{
    GenericOpenAiDialect, ModelCapabilities, ThinkingCapability, UsageFieldMap,
};
use zlogic_protocol::llm::{
    Effort, FinishReason, LlmError, LlmErrorKind, LlmEvent, LlmRequest, PartKind, RequestMeta,
    ThinkingIntent, ThinkingMode, ToolDefinition,
};
use zlogic_protocol::message::{ContentPart, Message, TextPart};
use zlogic_protocol::usage::Purpose;

type Build = fn(Arc<dyn HttpTransport>) -> Arc<dyn LlmClient>;

struct Case {
    name: &'static str,
    build: Build,
    happy: Vec<u8>,
    truncated_text: Vec<u8>,
    truncated_tool: Vec<u8>,
    mid_stream_error: Vec<u8>,
    has_reasoning: bool,
    has_reasoning_raw: bool,
    args_verbatim: bool,
    args_wire_is_string: bool,
}

fn caps() -> ModelCapabilities {
    ModelCapabilities {
        vision: Some(true),
        thinking: ThinkingCapability {
            supported: true,
            can_disable: true,
            efforts: vec![Effort::Low, Effort::High],
            budget: true,
        },
    }
}

fn endpoint() -> Endpoint {
    Endpoint::new("https://api.test")
        .with_key(Some("k".into()))
        .with_capabilities(caps())
}

fn cases() -> Vec<Case> {
    let mut v = vec![
        Case {
            name: "openai_chat",
            build: |t| Arc::new(ChatClient::new(VanillaOpenAi, endpoint(), t)),
            happy: chat::happy("reasoning_content", None),
            truncated_text: chat::truncated_text(),
            truncated_tool: chat::truncated_tool(),
            mid_stream_error: chat::mid_stream_error(),
            has_reasoning: false,
            has_reasoning_raw: false,
            args_verbatim: true,
            args_wire_is_string: true,
        },
        Case {
            name: "deepseek",
            build: |t| Arc::new(ChatClient::new(DeepSeek, endpoint(), t)),
            happy: chat::happy("reasoning_content", None),
            truncated_text: chat::truncated_text(),
            truncated_tool: chat::truncated_tool(),
            mid_stream_error: chat::mid_stream_error(),
            has_reasoning: true,
            has_reasoning_raw: true,
            args_verbatim: true,
            args_wire_is_string: true,
        },
        Case {
            name: "glm",
            build: |t| Arc::new(ChatClient::new(Glm, endpoint(), t)),
            happy: chat::happy("reasoning_content", None),
            truncated_text: chat::truncated_text(),
            truncated_tool: chat::truncated_tool(),
            mid_stream_error: chat::mid_stream_error(),
            has_reasoning: true,
            has_reasoning_raw: true,
            args_verbatim: true,
            args_wire_is_string: true,
        },
        Case {
            name: "dashscope",
            build: |t| Arc::new(ChatClient::new(DashScope, endpoint(), t)),
            happy: chat::happy("reasoning_content", None),
            truncated_text: chat::truncated_text(),
            truncated_tool: chat::truncated_tool(),
            mid_stream_error: chat::mid_stream_error(),
            has_reasoning: true,
            has_reasoning_raw: true,
            args_verbatim: true,
            args_wire_is_string: true,
        },
        Case {
            name: "fireworks",
            build: |t| Arc::new(ChatClient::new(Fireworks, endpoint(), t)),
            happy: chat::happy("reasoning_content", None),
            truncated_text: chat::truncated_text(),
            truncated_tool: chat::truncated_tool(),
            mid_stream_error: chat::mid_stream_error(),
            has_reasoning: true,
            has_reasoning_raw: true,
            args_verbatim: true,
            args_wire_is_string: true,
        },
        Case {
            name: "openrouter",
            build: |t| Arc::new(ChatClient::new(OpenRouter, endpoint(), t)),
            happy: chat::happy(
                "reasoning",
                Some(json!([
                    { "type": "reasoning.text", "text": "Let me ", "index": 0 },
                    { "type": "reasoning.encrypted", "data": "enc", "index": 1 }
                ])),
            ),
            truncated_text: chat::truncated_text(),
            truncated_tool: chat::truncated_tool(),
            mid_stream_error: chat::mid_stream_error(),
            has_reasoning: true,
            has_reasoning_raw: true,
            args_verbatim: true,
            args_wire_is_string: true,
        },
        Case {
            name: "qwen_local(patched)",
            build: |t| Arc::new(ChatClient::new(QwenLocal, endpoint(), t)),
            happy: chat::happy("reasoning_content", None),
            truncated_text: chat::truncated_text(),
            truncated_tool: chat::truncated_tool(),
            mid_stream_error: chat::mid_stream_error(),
            has_reasoning: true,
            has_reasoning_raw: true,
            args_verbatim: true,
            args_wire_is_string: true,
        },
        Case {
            name: "qwen_local(think-tags)",
            build: |t| Arc::new(ChatClient::new(QwenLocal, endpoint(), t)),
            happy: chat::happy_think_tags(),
            truncated_text: chat::truncated_text(),
            truncated_tool: chat::truncated_tool(),
            mid_stream_error: chat::mid_stream_error(),
            has_reasoning: true,
            has_reasoning_raw: false,
            args_verbatim: true,
            args_wire_is_string: true,
        },
        Case {
            name: "openai_generic",
            build: |t| {
                Arc::new(ChatClient::new(
                    GenericOpenAi::new(GenericOpenAiDialect {
                        reasoning_carrier: Some("reasoning_content".into()),
                        usage_fields: UsageFieldMap::default(),
                        ..Default::default()
                    }),
                    endpoint(),
                    t,
                ))
            },
            happy: chat::happy("reasoning_content", None),
            truncated_text: chat::truncated_text(),
            truncated_tool: chat::truncated_tool(),
            mid_stream_error: chat::mid_stream_error(),
            has_reasoning: true,
            has_reasoning_raw: true,
            args_verbatim: true,
            args_wire_is_string: true,
        },
        Case {
            name: "anthropic",
            build: |t| Arc::new(AnthropicClient::new(endpoint(), t, Some(4096))),
            happy: anthropic_fx::happy(),
            truncated_text: anthropic_fx::truncated_text(),
            truncated_tool: anthropic_fx::truncated_tool(),
            mid_stream_error: anthropic_fx::mid_stream_error(),
            has_reasoning: true,
            has_reasoning_raw: true,
            args_verbatim: true,
            args_wire_is_string: false,
        },
        Case {
            name: "gemini",
            build: |t| Arc::new(GeminiClient::new(endpoint(), t, Some(4096))),
            happy: gemini_fx::happy(),
            truncated_text: gemini_fx::truncated_text(),
            truncated_tool: gemini_fx::truncated_tool(),
            mid_stream_error: gemini_fx::mid_stream_error(),
            has_reasoning: true,
            has_reasoning_raw: true,
            args_verbatim: false,
            args_wire_is_string: false,
        },
        Case {
            name: "openai_responses",
            build: |t| Arc::new(ResponsesClient::new(endpoint(), t, Some(4096))),
            happy: responses_fx::happy(),
            truncated_text: responses_fx::truncated_text(),
            truncated_tool: responses_fx::truncated_tool(),
            mid_stream_error: responses_fx::mid_stream_error(),
            has_reasoning: true,
            has_reasoning_raw: true,
            args_verbatim: true,
            args_wire_is_string: true,
        },
        Case {
            name: "bedrock",
            build: |t| {
                Arc::new(BedrockClient::new(
                    endpoint(),
                    t,
                    "us-east-1",
                    Credentials {
                        access_key_id: "AKID".into(),
                        secret_access_key: "SECRET".into(),
                        session_token: None,
                    },
                    Some(4096),
                ))
            },
            happy: bedrock_fx::happy(),
            truncated_text: bedrock_fx::truncated_text(),
            truncated_tool: bedrock_fx::truncated_tool(),
            mid_stream_error: bedrock_fx::mid_stream_error(),
            has_reasoning: true,
            has_reasoning_raw: true,
            args_verbatim: true,
            args_wire_is_string: false,
        },
    ];
    v.sort_by_key(|c| c.name);
    v
}

fn assert_lifecycle(events: &[LlmEvent], case: &str) {
    let mut open: Option<u32> = None;
    let mut open_kind: Option<PartKind> = None;
    let mut last_start: Option<u32> = None;
    let mut ended = std::collections::HashSet::new();
    let mut detected = std::collections::HashSet::new();
    let mut response_end_at: Option<usize> = None;

    for (i, ev) in events.iter().enumerate() {
        assert!(
            response_end_at.is_none(),
            "[{case}] events after ResponseEnd: {ev:?}"
        );
        match ev {
            LlmEvent::PartStart { index, kind } => {
                assert!(
                    open.is_none(),
                    "[{case}] only one open part is allowed at a time"
                );
                if let Some(prev) = last_start {
                    assert!(*index > prev, "[{case}] index must increase monotonically");
                }
                last_start = Some(*index);
                open = Some(*index);
                open_kind = Some(*kind);
            }
            LlmEvent::PartDelta { index, .. } => {
                assert_eq!(
                    open,
                    Some(*index),
                    "[{case}] a delta must land on the open part"
                );
                assert!(
                    !ended.contains(index),
                    "[{case}] no delta may follow PartEnd"
                );
            }
            LlmEvent::PartEnd { index, part } => {
                assert_eq!(
                    open,
                    Some(*index),
                    "[{case}] PartEnd must close the open part"
                );
                assert!(
                    ended.insert(*index),
                    "[{case}] an index can only PartEnd once"
                );
                open = None;
                open_kind = None;
                if let ContentPart::Reasoning(p) = part
                    && p.truncated
                {
                    assert!(
                        p.raw.is_none(),
                        "[{case}] a truncated reasoning part must not carry raw"
                    );
                }
                if let ContentPart::Text(p) = part
                    && p.truncated
                {
                    assert!(
                        p.raw.is_none(),
                        "[{case}] a truncated text part must not carry raw"
                    );
                }
            }
            LlmEvent::ToolCallDetected {
                index, call_index, ..
            } => {
                assert_eq!(
                    open,
                    Some(*index),
                    "[{case}] detected must fall inside the tool-call lifecycle"
                );
                assert!(
                    detected.insert((*index, *call_index)),
                    "[{case}] a call_index can only be detected once"
                );
            }
            LlmEvent::Usage(_) | LlmEvent::Notice { .. } => {}
            LlmEvent::ResponseEnd { finish_reason } => {
                if open.is_some() {
                    let truncated = matches!(
                        finish_reason,
                        FinishReason::Length | FinishReason::ContentFilter
                    );
                    assert!(
                        truncated && open_kind == Some(PartKind::ToolCall),
                        "[{case}] all parts must be closed before ResponseEnd; \
                         only a truncated tool-call group may go without PartEnd (open_kind={open_kind:?}, \
                         finish={finish_reason:?})"
                    );
                }
                response_end_at = Some(i);
            }
        }
    }

    assert_eq!(
        response_end_at,
        Some(events.len() - 1),
        "[{case}] ResponseEnd must be unique and last"
    );
}

fn tool_call_part(events: &[LlmEvent]) -> Option<&zlogic_protocol::message::ToolCallPart> {
    events.iter().find_map(|e| match e {
        LlmEvent::PartEnd {
            part: ContentPart::ToolCall(p),
            ..
        } => Some(p),
        _ => None,
    })
}

fn reasoning_part(events: &[LlmEvent]) -> Option<&zlogic_protocol::message::ReasoningPart> {
    events.iter().find_map(|e| match e {
        LlmEvent::PartEnd {
            part: ContentPart::Reasoning(p),
            ..
        } => Some(p),
        _ => None,
    })
}

fn text_part(events: &[LlmEvent]) -> Option<&zlogic_protocol::message::TextPart> {
    events.iter().find_map(|e| match e {
        LlmEvent::PartEnd {
            part: ContentPart::Text(p),
            ..
        } => Some(p),
        _ => None,
    })
}

fn usage(events: &[LlmEvent]) -> Option<&zlogic_protocol::usage::UsageReport> {
    events.iter().find_map(|e| match e {
        LlmEvent::Usage(u) => Some(u),
        _ => None,
    })
}

fn accumulated_deltas(events: &[LlmEvent], kind: PartKind) -> String {
    let mut target: Option<u32> = None;
    let mut out = String::new();
    for ev in events {
        match ev {
            LlmEvent::PartStart { index, kind: k } if *k == kind => target = Some(*index),
            LlmEvent::PartDelta { index, delta } if Some(*index) == target => out.push_str(delta),
            LlmEvent::PartEnd { index, .. } if Some(*index) == target => target = None,
            _ => {}
        }
    }
    out
}

async fn run(case: &Case, body: &[u8], chunk: usize) -> Result<Vec<LlmEvent>, LlmError> {
    let transport: Arc<dyn HttpTransport> = Arc::new(ReplayTransport::chunked(body, chunk));
    let client = (case.build)(transport);
    let stream = client.stream(request()).await?;
    let mut out = Vec::new();
    let mut s = stream;
    while let Some(item) = s.next().await {
        out.push(item?);
    }
    Ok(out)
}

fn request() -> LlmRequest {
    LlmRequest {
        model: "test-model".into(),
        system: vec![],
        messages: vec![Message::user(vec![ContentPart::Text(TextPart {
            text: "hi".into(),
            raw: None,
            truncated: false,
        })])],
        tools: vec![ToolDefinition {
            name: "alpha".into(),
            description: "d".into(),
            parameters: json!({ "type": "object" }),
        }],
        cache: Default::default(),
        thinking: ThinkingIntent {
            mode: ThinkingMode::On,
            effort: Some(Effort::Low),
            budget_tokens: None,
        },
        params: Default::default(),
        response_format: None,
        meta: RequestMeta {
            session_id: "s".into(),
            turn_id: "t".into(),
            round_id: "r".into(),
            purpose: Purpose::Main,
        },
    }
}

#[tokio::test]
async fn lifecycle_holds_for_every_client_at_every_chunk_size() {
    for case in cases() {
        for chunk in [1, 2, 3, 7, 64, usize::MAX / 2] {
            let events = run(&case, &case.happy, chunk).await.expect(case.name);
            assert_lifecycle(&events, case.name);
        }
    }
}

#[tokio::test]
async fn chunk_size_does_not_change_the_outcome() {
    for case in cases() {
        let reference = run(&case, &case.happy, usize::MAX / 2)
            .await
            .expect(case.name);
        for chunk in [1, 2, 3, 5, 11, 64] {
            let got = run(&case, &case.happy, chunk).await.expect(case.name);
            assert_eq!(
                accumulated_deltas(&got, PartKind::Text),
                accumulated_deltas(&reference, PartKind::Text),
                "[{}] chunk {chunk}: accumulated text differs",
                case.name
            );
            assert_eq!(
                text_part(&got).map(|p| &p.text),
                text_part(&reference).map(|p| &p.text),
                "[{}] chunk {chunk}: text PartEnd differs",
                case.name
            );
            assert_eq!(
                tool_call_part(&got),
                tool_call_part(&reference),
                "[{}] chunk {chunk}: tool-call group differs",
                case.name
            );
            assert_eq!(
                reasoning_part(&got),
                reasoning_part(&reference),
                "[{}] chunk {chunk}: reasoning differs",
                case.name
            );
        }
    }
}

#[tokio::test]
async fn delta_accumulation_matches_the_authoritative_part_end() {
    for case in cases() {
        let events = run(&case, &case.happy, 3).await.expect(case.name);
        let acc = accumulated_deltas(&events, PartKind::Text);
        let authoritative = text_part(&events).expect(case.name).text.clone();
        assert_eq!(
            acc, authoritative,
            "[{}] accumulated deltas should match PartEnd",
            case.name
        );
    }
}

// ═══════════════════════════ B. tool-call ═══════════════════════════

#[tokio::test]
async fn tool_call_group_is_single_ordered_and_verbatim() {
    for case in cases() {
        let events = run(&case, &case.happy, 3).await.expect(case.name);

        let groups = events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    LlmEvent::PartEnd {
                        part: ContentPart::ToolCall(_),
                        ..
                    }
                )
            })
            .count();
        assert_eq!(
            groups, 1,
            "[{}] at most one tool-call part per response",
            case.name
        );

        let part = tool_call_part(&events).expect(case.name);
        assert_eq!(part.calls.len(), 2, "[{}] two parallel calls", case.name);
        assert_eq!(
            part.calls[0].name, "alpha",
            "[{}] must be ordered by wire callIndex",
            case.name
        );
        assert_eq!(part.calls[1].name, "beta", "[{}]", case.name);
        if case.args_verbatim {
            assert_eq!(
                part.calls[0].args, "{\"b\": 1, \"a\": 2}",
                "[{}] args must be verbatim",
                case.name
            );
            assert_eq!(part.calls[1].args, "{\"y\": 2}", "[{}]", case.name);
        } else {
            assert_eq!(part.calls[0].args, "{\"b\":1,\"a\":2}", "[{}]", case.name);
        }
        let keys: Vec<String> =
            serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&part.calls[0].args)
                .unwrap()
                .keys()
                .cloned()
                .collect();
        assert_eq!(
            keys,
            ["b", "a"],
            "[{}] key order must be preserved",
            case.name
        );
        assert!(
            !part.calls[0].id.is_empty(),
            "[{}] a missing id must be synthesized",
            case.name
        );
    }
}

#[tokio::test]
async fn tool_call_part_has_zero_deltas() {
    for case in cases() {
        let events = run(&case, &case.happy, 1).await.expect(case.name);
        let tool_index = events.iter().find_map(|e| match e {
            LlmEvent::PartStart {
                index,
                kind: PartKind::ToolCall,
            } => Some(*index),
            _ => None,
        });
        let Some(idx) = tool_index else { continue };
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, LlmEvent::PartDelta { index, .. } if *index == idx)),
            "[{}] a tool-call part stays at zero deltas throughout",
            case.name
        );
    }
}

#[tokio::test]
async fn detected_fires_early_and_is_not_authoritative() {
    for case in cases() {
        let events = run(&case, &case.happy, 1).await.expect(case.name);
        let first_detected = events
            .iter()
            .position(|e| matches!(e, LlmEvent::ToolCallDetected { .. }));
        let tool_end = events.iter().position(|e| {
            matches!(
                e,
                LlmEvent::PartEnd {
                    part: ContentPart::ToolCall(_),
                    ..
                }
            )
        });
        let (Some(d), Some(end)) = (first_detected, tool_end) else {
            panic!(
                "[{}] expected both detected and a tool-call PartEnd",
                case.name
            );
        };
        assert!(d < end, "[{}] detected must come before PartEnd", case.name);

        let without: Vec<LlmEvent> = events
            .iter()
            .filter(|e| !matches!(e, LlmEvent::ToolCallDetected { .. }))
            .cloned()
            .collect();
        assert_eq!(
            tool_call_part(&without),
            tool_call_part(&events),
            "[{}] detected is not authoritative state",
            case.name
        );
    }
}

// ═══════════════════════════ C. raw ═══════════════════════════

#[tokio::test]
async fn reasoning_raw_presence_matches_the_declared_capability() {
    for case in cases() {
        let events = run(&case, &case.happy, 3).await.expect(case.name);
        if !case.has_reasoning {
            assert!(
                reasoning_part(&events).is_none(),
                "[{}] this client has no reasoning channel, so no reasoning part should be parsed",
                case.name
            );
            continue;
        }
        let r = reasoning_part(&events)
            .unwrap_or_else(|| panic!("[{}] expected a reasoning part", case.name));
        assert!(
            !r.text.is_empty(),
            "[{}] the reasoning display text must not be empty",
            case.name
        );
        assert_eq!(
            r.raw.is_some(),
            case.has_reasoning_raw,
            "[{}] whether reasoning raw is present must match the declaration (an inline <think> has no replayable payload)",
            case.name
        );
        if let Some(raw) = &r.raw {
            assert!(!raw.is_null(), "[{}] raw must not be null", case.name);
            if let Some(o) = raw.as_object() {
                assert!(
                    !o.is_empty(),
                    "[{}] raw must not be an empty object",
                    case.name
                );
            }
        }
    }
}

#[tokio::test]
async fn interrupted_stream_produces_no_raw_and_no_response_end() {
    for case in cases() {
        let prefix: Vec<u8> = case
            .happy
            .iter()
            .copied()
            .take(case.happy.len() / 2)
            .collect();
        let transport: Arc<dyn HttpTransport> = Arc::new(TruncatingTransport { prefix });
        let client = (case.build)(transport);
        let mut stream = client.stream(request()).await.expect(case.name);

        let mut events = Vec::new();
        let mut err = None;
        while let Some(item) = stream.next().await {
            match item {
                Ok(e) => events.push(e),
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }

        let err =
            err.unwrap_or_else(|| panic!("[{}] a mid-stream cut must surface as Err", case.name));
        assert!(
            !err.retryable,
            "[{}] once output has been produced it must never be resent: rows would be written twice and the UI would render twice",
            case.name
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, LlmEvent::ResponseEnd { .. })),
            "[{}] the error path must not emit ResponseEnd",
            case.name
        );
        let complete = run(&case, &case.happy, 64).await.expect(case.name);
        let ends = |v: &[LlmEvent]| {
            v.iter()
                .filter(|e| matches!(e, LlmEvent::PartEnd { .. }))
                .count()
        };
        assert!(
            ends(&events) < ends(&complete),
            "[{}] an interruption must have at least one fewer PartEnd than the complete stream — an in-flight part must not be faked into a finished state",
            case.name
        );
    }
}

#[tokio::test]
async fn raw_round_trips_byte_for_byte_into_the_next_request() {
    for case in cases() {
        if !case.has_reasoning_raw {
            continue;
        }
        let events = run(&case, &case.happy, 5).await.expect(case.name);
        let reasoning = reasoning_part(&events).expect(case.name).clone();
        let calls = tool_call_part(&events).expect(case.name).clone();
        let raw = reasoning
            .raw
            .clone()
            .unwrap_or_else(|| panic!("[{}] expected raw", case.name));

        let recorder = RecordingTransport::new();
        let client = (case.build)(Arc::new(recorder.clone()));
        let mut req = request();
        req.messages.push(Message::assistant(
            zlogic_protocol::message::Source::new("p", "m"),
            vec![
                ContentPart::Reasoning(reasoning),
                ContentPart::Text(TextPart {
                    text: "answer".into(),
                    raw: None,
                    truncated: false,
                }),
                ContentPart::ToolCall(calls.clone()),
            ],
        ));
        let mut s = client.stream(req).await.expect(case.name);
        while s.next().await.is_some() {}

        let body = recorder
            .last_body()
            .unwrap_or_else(|| panic!("[{}] no request was recorded", case.name));

        let mut leaves = Vec::new();
        collect_string_leaves(&raw, &mut leaves);
        assert!(
            !leaves.is_empty(),
            "[{}] raw should carry an assertable payload",
            case.name
        );
        for leaf in &leaves {
            assert!(
                body.contains(leaf.as_str()) || body.contains(&escape_json(leaf)),
                "[{}] raw payload {leaf:?} did not make it back to the wire verbatim — that is where the 400 comes from\nbody: {body}",
                case.name
            );
        }

        if case.args_wire_is_string {
            let escaped = escape_json("{\"b\": 1, \"a\": 2}");
            assert!(
                body.contains(&escaped),
                "[{}] string-typed wire args must go back verbatim (expected {escaped})\nbody: {body}",
                case.name
            );
        } else {
            assert!(
                body.contains("\"b\":1,\"a\":2"),
                "[{}] object-typed wire must at least keep key order (the default BTreeMap would sort them a,b)\nbody: {body}",
                case.name
            );
        }
    }
}

fn collect_string_leaves(v: &serde_json::Value, out: &mut Vec<String>) {
    match v {
        serde_json::Value::String(s) if !s.is_empty() => out.push(s.clone()),
        serde_json::Value::Array(a) => a.iter().for_each(|x| collect_string_leaves(x, out)),
        serde_json::Value::Object(o) => o.values().for_each(|x| collect_string_leaves(x, out)),
        _ => {}
    }
}

fn escape_json(s: &str) -> String {
    let encoded = serde_json::to_string(s).unwrap_or_default();
    encoded.trim_matches('"').to_string()
}

#[tokio::test]
async fn truncated_text_is_persisted_without_raw() {
    for case in cases() {
        let events = run(&case, &case.truncated_text, 3).await.expect(case.name);
        assert_lifecycle(&events, case.name);

        let t = text_part(&events).unwrap_or_else(|| {
            panic!(
                "[{}] content the user has seen must be persisted",
                case.name
            )
        });
        assert!(
            !t.text.is_empty(),
            "[{}] a truncated text part must not be empty",
            case.name
        );
        assert!(t.truncated, "[{}] must be marked truncated", case.name);
        assert!(
            t.raw.is_none(),
            "[{}] a truncated part must not carry raw",
            case.name
        );

        assert!(
            matches!(
                events.last(),
                Some(LlmEvent::ResponseEnd {
                    finish_reason: FinishReason::Length
                })
            ),
            "[{}] ResponseEnd is still emitted and carries length: {:?}",
            case.name,
            events.last()
        );
    }
}

#[tokio::test]
async fn truncated_tool_call_is_dropped_entirely() {
    for case in cases() {
        let events = run(&case, &case.truncated_tool, 3).await.expect(case.name);
        assert_lifecycle(&events, case.name);
        assert!(
            tool_call_part(&events).is_none(),
            "[{}] a half-written args blob is always a 400 on replay, so the whole group must be dropped: {events:?}",
            case.name
        );
        assert!(
            matches!(
                events.last(),
                Some(LlmEvent::ResponseEnd {
                    finish_reason: FinishReason::Length
                })
            ),
            "[{}] ResponseEnd is still emitted",
            case.name
        );
    }
}

// ═══════════════════════════ E. usage ═══════════════════════════

#[tokio::test]
async fn usage_arrives_normalized() {
    for case in cases() {
        let events = run(&case, &case.happy, 3).await.expect(case.name);
        let u = usage(&events).unwrap_or_else(|| panic!("[{}] expected a usage event", case.name));
        assert_eq!(
            u.tokens.input, 100,
            "[{}] input must be the total billable input figure",
            case.name
        );
        assert_eq!(u.tokens.output, 20, "[{}]", case.name);
        assert_eq!(u.tokens.cache_read, Some(60), "[{}]", case.name);
        assert!(
            u.raw.is_some(),
            "[{}] the raw blob is kept for diagnostics",
            case.name
        );
    }
}

#[tokio::test]
async fn cache_inclusive_and_exclusive_semantics_normalize_to_the_same_total() {
    let inclusive = cases().into_iter().find(|c| c.name == "deepseek").unwrap();
    let a = run(&inclusive, &inclusive.happy, 7).await.unwrap();
    let exclusive = cases().into_iter().find(|c| c.name == "anthropic").unwrap();
    let b = run(&exclusive, &exclusive.happy, 7).await.unwrap();

    assert_eq!(
        usage(&a).unwrap().tokens.input,
        usage(&b).unwrap().tokens.input,
        "the two reporting conventions must normalize to the same total billable input, or context accounting drifts systematically"
    );
}

// ═══════════════════════════ media ═══════════════════════════

#[tokio::test]
async fn broken_images_degrade_to_an_explanatory_placeholder_everywhere() {
    use zlogic_protocol::message::{ImagePart, ImageSource};

    for case in cases() {
        for (label, part) in [
            (
                "empty base64",
                ImagePart {
                    mime_type: "image/png".into(),
                    source: ImageSource::Base64 {
                        data: String::new(),
                    },
                },
            ),
            (
                "non-vision mime",
                ImagePart {
                    mime_type: "image/svg+xml".into(),
                    source: ImageSource::Base64 {
                        data: "QUFBQQ==".into(),
                    },
                },
            ),
            (
                "unresolved path",
                ImagePart {
                    mime_type: "image/png".into(),
                    source: ImageSource::Path {
                        path: "/tmp/a.png".into(),
                    },
                },
            ),
        ] {
            let body = sent_body(&case, |r| {
                r.messages = vec![Message::user(vec![ContentPart::Image(part.clone())])];
            })
            .await;
            assert!(
                body.contains("image omitted"),
                "[{}] {label}: a sentence for the model to read must be left behind\nbody: {body}",
                case.name
            );
        }
    }
}

async fn sent_body(case: &Case, mut mutate: impl FnMut(&mut LlmRequest)) -> String {
    let recorder = RecordingTransport::new();
    let client = (case.build)(Arc::new(recorder.clone()));
    let mut req = request();
    mutate(&mut req);
    let mut s = client.stream(req).await.expect(case.name);
    while s.next().await.is_some() {}
    recorder
        .last_body()
        .unwrap_or_else(|| panic!("[{}] no request was recorded", case.name))
}

#[tokio::test]
async fn response_format_reaches_the_wire_for_every_client_that_has_the_field() {
    for case in cases()
        .into_iter()
        .filter(|c| c.name != "bedrock" && c.name != "deepseek")
    {
        let body = sent_body(&case, |r| {
            r.response_format = Some(zlogic_protocol::llm::ResponseFormat::JsonSchema {
                name: "verdict".into(),
                schema: serde_json::json!({
                    "type": "object",
                    "properties": { "distinctive_marker": { "type": "string" } }
                }),
                strict: true,
            });
        })
        .await;

        assert!(
            body.contains("distinctive_marker"),
            "[{}] the caller's schema must reach the wire, or structured output is an empty promise\nbody: {body}",
            case.name
        );
    }
}

#[tokio::test]
async fn deepseek_collapses_json_schema_to_json_object_on_the_wire() {
    let deepseek = cases().into_iter().find(|c| c.name == "deepseek").unwrap();
    let body = sent_body(&deepseek, |r| {
        r.response_format = Some(zlogic_protocol::llm::ResponseFormat::JsonSchema {
            name: "verdict".into(),
            schema: serde_json::json!({
                "type": "object",
                "properties": { "distinctive_marker": { "type": "string" } }
            }),
            strict: true,
        });
    })
    .await;

    assert!(body.contains("\"json_object\""), "body: {body}");
    assert!(
        !body.contains("distinctive_marker"),
        "DeepSeek takes no schema; the field shape has to travel in the prompt, so json_schema must not go on the wire\nbody: {body}"
    );
}

#[tokio::test]
async fn handshake_failures_are_retryable_or_not_by_status() {
    for (status, retryable, kind) in [
        (429u16, true, LlmErrorKind::RateLimit),
        (503, true, LlmErrorKind::Server),
        (401, false, LlmErrorKind::Auth),
        (400, false, LlmErrorKind::BadRequest),
    ] {
        for case in cases() {
            let transport: Arc<dyn HttpTransport> = Arc::new(FailingTransport::new(status, "boom"));
            let client = (case.build)(transport);
            let Err(e) = client.stream(request()).await else {
                panic!(
                    "[{}] HTTP {status} should fail during the handshake",
                    case.name
                );
            };
            assert_eq!(e.kind, kind, "[{}] {status}", case.name);
            assert_eq!(
                e.retryable, retryable,
                "[{}] {status} was classified wrongly for retryability",
                case.name
            );
        }
    }
}

#[tokio::test]
async fn context_overflow_gets_its_own_kind_and_is_not_retryable() {
    for case in cases() {
        let transport: Arc<dyn HttpTransport> = Arc::new(FailingTransport::new(
            400,
            "{\"error\":{\"message\":\"This model's maximum context length is 128000\"}}",
        ));
        let client = (case.build)(transport);
        let Err(e) = client.stream(request()).await else {
            panic!("[{}] should fail", case.name);
        };
        assert_eq!(
            e.kind,
            LlmErrorKind::ContextLengthExceeded,
            "[{}] it is the sole trigger for compress-and-retry-once and must be told apart from an ordinary retryable",
            case.name
        );
        assert!(
            !e.retryable,
            "[{}] resending the same request would overflow again",
            case.name
        );
    }
}

#[tokio::test]
async fn quota_exhaustion_is_not_retried() {
    for case in cases() {
        let transport: Arc<dyn HttpTransport> = Arc::new(FailingTransport::new(
            429,
            "{\"error\":{\"code\":\"insufficient_quota\",\"message\":\"You exceeded your current quota\"}}",
        ));
        let client = (case.build)(transport);
        let Err(e) = client.stream(request()).await else {
            panic!("[{}] should fail", case.name);
        };
        assert_eq!(e.kind, LlmErrorKind::Quota, "[{}]", case.name);
        assert!(
            !e.retryable,
            "[{}] retrying against an exhausted key is pure waste",
            case.name
        );
    }
}

#[tokio::test]
async fn request_id_reaches_the_error() {
    for case in cases() {
        let transport: Arc<dyn HttpTransport> = Arc::new(
            FailingTransport::new(429, "Rate limit reached")
                .header("Retry-After", "7")
                .header("x-request-id", "req_abc"),
        );
        let client = (case.build)(transport);
        let Err(e) = client.stream(request()).await else {
            panic!("[{}] should fail", case.name);
        };
        assert_eq!(e.request_id.as_deref(), Some("req_abc"), "[{}]", case.name);
    }
}

#[tokio::test]
async fn mid_stream_error_frames_surface_as_non_retryable_errors() {
    for case in cases() {
        let transport: Arc<dyn HttpTransport> =
            Arc::new(ReplayTransport::whole(case.mid_stream_error.clone()));
        let client = (case.build)(transport);
        let mut stream = client.stream(request()).await.expect(case.name);

        let mut saw_response_end = false;
        let mut err = None;
        while let Some(item) = stream.next().await {
            match item {
                Ok(LlmEvent::ResponseEnd { .. }) => saw_response_end = true,
                Ok(_) => {}
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        assert!(
            err.is_some(),
            "[{}] an in-stream error frame must surface as Err",
            case.name
        );
        assert!(!err.unwrap().retryable, "[{}]", case.name);
        assert!(
            !saw_response_end,
            "[{}] the error path emits no ResponseEnd",
            case.name
        );
    }
}

#[tokio::test]
async fn mock_client_satisfies_the_same_lifecycle_invariants() {
    let client = MockClient::new(MockScript {
        reasoning: Some("thinking".into()),
        text: Some("answer".into()),
        tool_calls: vec![(
            0,
            "call_1".into(),
            "alpha".into(),
            "{\"b\": 1, \"a\": 2}".into(),
        )],
        usage: None,
        notice: None,
        finish: None,
        fail_with: None,
    });
    let mut stream = client.stream(request()).await.unwrap();
    let mut events = Vec::new();
    while let Some(e) = stream.next().await {
        events.push(e.unwrap());
    }
    assert_lifecycle(&events, "mock");
    assert_eq!(
        tool_call_part(&events).unwrap().calls[0].args,
        "{\"b\": 1, \"a\": 2}"
    );
    assert!(matches!(
        events.last(),
        Some(LlmEvent::ResponseEnd {
            finish_reason: FinishReason::ToolCalls
        })
    ));
}

// ═══════════════════════════ fixture ═══════════════════════════

const USAGE_ALL: &str = r#"{"prompt_tokens":100,"completion_tokens":20,"prompt_cache_hit_tokens":60,"prompt_tokens_details":{"cached_tokens":60},"input_tokens":40,"output_tokens":20,"cache_read_input_tokens":60,"input_tokens_details":{"cached_tokens":60},"cost":0.0012}"#;

mod chat {
    use super::USAGE_ALL;
    use serde_json::Value;

    fn sse(lines: &[String]) -> Vec<u8> {
        let mut s = String::new();
        for l in lines {
            s.push_str("data: ");
            s.push_str(l);
            s.push_str("\r\n\r\n");
        }
        s.push_str("data: [DONE]\r\n\r\n");
        s.into_bytes()
    }

    fn delta(inner: &str) -> String {
        format!(r#"{{"choices":[{{"index":0,"delta":{inner}}}]}}"#)
    }

    pub fn happy(reasoning_field: &str, details: Option<Value>) -> Vec<u8> {
        let mut lines = vec![
            delta(&format!(r#"{{"{reasoning_field}":"Let me "}}"#)),
            delta(&format!(r#"{{"{reasoning_field}":"work…"}}"#)),
        ];
        if let Some(d) = details {
            lines.push(delta(&format!(r#"{{"reasoning_details":{d}}}"#)));
        }
        lines.extend([
            delta(r#"{"content":"ans"}"#),
            delta(r#"{"content":"wer"}"#),
            delta(
                r#"{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"alpha","arguments":""}}]}"#,
            ),
            delta(r#"{"tool_calls":[{"index":0,"function":{"arguments":"{\"b\": 1, \"a\": 2}"}}]}"#),
            delta(
                r#"{"tool_calls":[{"index":1,"id":"call_2","type":"function","function":{"name":"beta","arguments":"{\"y\": 2}"}}]}"#,
            ),
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#.to_string(),
            format!(r#"{{"choices":[],"usage":{USAGE_ALL}}}"#),
        ]);
        sse(&lines)
    }

    pub fn happy_think_tags() -> Vec<u8> {
        sse(&[
            delta(r#"{"content":"<think>Let me "}"#),
            delta(r#"{"content":"work…</thi"}"#),
            delta(r#"{"content":"nk>ans"}"#),
            delta(r#"{"content":"wer"}"#),
            delta(
                r#"{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"alpha","arguments":"{\"b\": 1, \"a\": 2}"}}]}"#,
            ),
            delta(
                r#"{"tool_calls":[{"index":1,"id":"call_2","type":"function","function":{"name":"beta","arguments":"{\"y\": 2}"}}]}"#,
            ),
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#.to_string(),
            format!(r#"{{"choices":[],"usage":{USAGE_ALL}}}"#),
        ])
    }

    pub fn truncated_text() -> Vec<u8> {
        sse(&[
            delta(r#"{"content":"truncated"}"#),
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"length"}]}"#.to_string(),
        ])
    }

    pub fn truncated_tool() -> Vec<u8> {
        sse(&[
            delta(
                r#"{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"alpha","arguments":"{\"cmd\": \"ec"}}]}"#,
            ),
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"length"}]}"#.to_string(),
        ])
    }

    pub fn mid_stream_error() -> Vec<u8> {
        sse(&[
            delta(r#"{"content":"start"}"#),
            r#"{"error":{"message":"upstream exploded","code":500}}"#.to_string(),
        ])
    }
}

mod anthropic_fx {
    fn sse(frames: &[(&str, String)]) -> Vec<u8> {
        let mut s = String::new();
        for (event, data) in frames {
            s.push_str(&format!("event: {event}\r\ndata: {data}\r\n\r\n"));
        }
        s.into_bytes()
    }

    const USAGE: &str = r#"{"input_tokens":40,"output_tokens":20,"cache_read_input_tokens":60}"#;

    pub fn happy() -> Vec<u8> {
        sse(&[
            ("message_start", format!(r#"{{"type":"message_start","message":{{"usage":{USAGE}}}}}"#)),
            ("content_block_start", r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#.into()),
            ("content_block_delta", r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me "}}"#.into()),
            ("content_block_delta", r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"work…"}}"#.into()),
            ("content_block_delta", r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"EqoBCk"}}"#.into()),
            ("content_block_stop", r#"{"type":"content_block_stop","index":0}"#.into()),
            ("content_block_start", r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#.into()),
            ("content_block_delta", r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"ans"}}"#.into()),
            ("content_block_delta", r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"wer"}}"#.into()),
            ("content_block_stop", r#"{"type":"content_block_stop","index":1}"#.into()),
            ("content_block_start", r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"call_1","name":"alpha"}}"#.into()),
            ("content_block_delta", r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"b\": 1, \"a\": 2}"}}"#.into()),
            ("content_block_stop", r#"{"type":"content_block_stop","index":2}"#.into()),
            ("content_block_start", r#"{"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"call_2","name":"beta"}}"#.into()),
            ("content_block_delta", r#"{"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"{\"y\": 2}"}}"#.into()),
            ("content_block_stop", r#"{"type":"content_block_stop","index":3}"#.into()),
            ("message_delta", format!(r#"{{"type":"message_delta","delta":{{"stop_reason":"tool_use"}},"usage":{USAGE}}}"#)),
            ("message_stop", r#"{"type":"message_stop"}"#.into()),
        ])
    }

    pub fn truncated_text() -> Vec<u8> {
        sse(&[
            ("content_block_start", r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#.into()),
            ("content_block_delta", r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"truncated"}}"#.into()),
            ("message_delta", r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"}}"#.into()),
        ])
    }

    pub fn truncated_tool() -> Vec<u8> {
        sse(&[
            ("content_block_start", r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_1","name":"alpha"}}"#.into()),
            ("content_block_delta", r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\": \"ec"}}"#.into()),
            ("message_delta", r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"}}"#.into()),
        ])
    }

    pub fn mid_stream_error() -> Vec<u8> {
        sse(&[
            ("content_block_start", r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#.into()),
            ("content_block_delta", r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"start"}}"#.into()),
            ("error", r#"{"type":"error","error":{"type":"overloaded_error","message":"upstream exploded"}}"#.into()),
        ])
    }
}

mod gemini_fx {
    const USAGE: &str = r#"{"promptTokenCount":100,"candidatesTokenCount":8,"cachedContentTokenCount":60,"thoughtsTokenCount":12,"totalTokenCount":120}"#;

    fn sse(lines: &[String]) -> Vec<u8> {
        let mut s = String::new();
        for l in lines {
            s.push_str(&format!("data: {l}\r\n\r\n"));
        }
        s.into_bytes()
    }

    fn chunk(parts: &str, finish: Option<&str>, usage: bool) -> String {
        let fr = finish
            .map(|f| format!(r#","finishReason":"{f}""#))
            .unwrap_or_default();
        let um = if usage {
            format!(r#","usageMetadata":{USAGE}"#)
        } else {
            String::new()
        };
        format!(r#"{{"candidates":[{{"content":{{"role":"model","parts":[{parts}]}}{fr}}}]{um}}}"#)
    }

    pub fn happy() -> Vec<u8> {
        sse(&[
            chunk(r#"{"text":"Let me ","thought":true}"#, None, false),
            chunk(
                r#"{"text":"work…","thought":true,"thoughtSignature":"Cs8B"}"#,
                None,
                false,
            ),
            chunk(r#"{"text":"ans"}"#, None, false),
            chunk(r#"{"text":"wer"}"#, None, false),
            chunk(
                r#"{"functionCall":{"name":"alpha","args":{"b":1,"a":2}},"thoughtSignature":"Ct2A"},{"functionCall":{"name":"beta","args":{"y":2}}}"#,
                Some("STOP"),
                true,
            ),
        ])
    }

    pub fn truncated_text() -> Vec<u8> {
        sse(&[chunk(r#"{"text":"truncated"}"#, Some("MAX_TOKENS"), false)])
    }

    pub fn truncated_tool() -> Vec<u8> {
        sse(&[chunk(
            r#"{"functionCall":{"name":"alpha","args":{"cmd":"ec"}}}"#,
            Some("MAX_TOKENS"),
            false,
        )])
    }

    pub fn mid_stream_error() -> Vec<u8> {
        sse(&[
            chunk(r#"{"text":"start"}"#, None, false),
            r#"{"error":{"code":500,"message":"upstream exploded"}}"#.to_string(),
        ])
    }
}

mod responses_fx {
    const USAGE: &str = r#"{"input_tokens":100,"output_tokens":20,"input_tokens_details":{"cached_tokens":60},"output_tokens_details":{"reasoning_tokens":12}}"#;

    fn sse(frames: &[String]) -> Vec<u8> {
        let mut s = String::new();
        for data in frames {
            s.push_str(&format!("data: {data}\r\n\r\n"));
        }
        s.into_bytes()
    }

    pub fn happy() -> Vec<u8> {
        sse(&[
            r#"{"type":"response.output_item.added","item":{"type":"reasoning","id":"rs_1"}}"#.into(),
            r#"{"type":"response.reasoning_summary_text.delta","delta":"Let me "}"#.into(),
            r#"{"type":"response.reasoning_summary_text.delta","delta":"work…"}"#.into(),
            r#"{"type":"response.output_item.done","item":{"type":"reasoning","id":"rs_1","encrypted_content":"gAAAA"}}"#.into(),
            r#"{"type":"response.output_item.added","item":{"type":"message","role":"assistant"}}"#.into(),
            r#"{"type":"response.output_text.delta","delta":"ans"}"#.into(),
            r#"{"type":"response.output_text.delta","delta":"wer"}"#.into(),
            r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"alpha"}}"#.into(),
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"b\": 1, \"a\": 2}"}"#.into(),
            r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"fc_2","call_id":"call_2","name":"beta"}}"#.into(),
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_2","delta":"{\"y\": 2}"}"#.into(),
            format!(r#"{{"type":"response.completed","response":{{"usage":{USAGE}}}}}"#),
        ])
    }

    pub fn truncated_text() -> Vec<u8> {
        sse(&[
            r#"{"type":"response.output_item.added","item":{"type":"message","role":"assistant"}}"#.into(),
            r#"{"type":"response.output_text.delta","delta":"truncated"}"#.into(),
            r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}}"#.into(),
        ])
    }

    pub fn truncated_tool() -> Vec<u8> {
        sse(&[
            r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"alpha"}}"#.into(),
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"cmd\": \"ec"}"#.into(),
            r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}}"#.into(),
        ])
    }

    pub fn mid_stream_error() -> Vec<u8> {
        sse(&[
            r#"{"type":"response.output_item.added","item":{"type":"message","role":"assistant"}}"#
                .into(),
            r#"{"type":"response.output_text.delta","delta":"start"}"#.into(),
            r#"{"type":"response.failed","response":{"error":{"message":"upstream exploded"}}}"#
                .into(),
        ])
    }
}

mod bedrock_fx {
    const PRELUDE: usize = 12;
    const TRAILER: usize = 4;

    fn frame(event_type: &str, message_type: &str, payload: &str) -> Vec<u8> {
        let mut headers = Vec::new();
        for (name, value) in [(":event-type", event_type), (":message-type", message_type)] {
            headers.push(name.len() as u8);
            headers.extend_from_slice(name.as_bytes());
            headers.push(7);
            headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
            headers.extend_from_slice(value.as_bytes());
        }
        let total = (PRELUDE + headers.len() + payload.len() + TRAILER) as u32;
        let mut out = Vec::new();
        out.extend_from_slice(&total.to_be_bytes());
        out.extend_from_slice(&(headers.len() as u32).to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&headers);
        out.extend_from_slice(payload.as_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());
        out
    }

    fn ev(event_type: &str, payload: &str) -> Vec<u8> {
        frame(event_type, "event", payload)
    }

    fn join(frames: Vec<Vec<u8>>) -> Vec<u8> {
        frames.into_iter().flatten().collect()
    }

    const USAGE: &str =
        r#"{"usage":{"inputTokens":40,"outputTokens":20,"cacheReadInputTokens":60}}"#;

    pub fn happy() -> Vec<u8> {
        join(vec![
            ev("messageStart", r#"{"role":"assistant"}"#),
            ev(
                "contentBlockDelta",
                r#"{"contentBlockIndex":0,"delta":{"reasoningContent":{"text":"Let me "}}}"#,
            ),
            ev(
                "contentBlockDelta",
                r#"{"contentBlockIndex":0,"delta":{"reasoningContent":{"text":"work…"}}}"#,
            ),
            ev(
                "contentBlockDelta",
                r#"{"contentBlockIndex":0,"delta":{"reasoningContent":{"signature":"sig_1"}}}"#,
            ),
            ev("contentBlockStop", r#"{"contentBlockIndex":0}"#),
            ev(
                "contentBlockDelta",
                r#"{"contentBlockIndex":1,"delta":{"text":"ans"}}"#,
            ),
            ev(
                "contentBlockDelta",
                r#"{"contentBlockIndex":1,"delta":{"text":"wer"}}"#,
            ),
            ev("contentBlockStop", r#"{"contentBlockIndex":1}"#),
            ev(
                "contentBlockStart",
                r#"{"contentBlockIndex":2,"start":{"toolUse":{"toolUseId":"call_1","name":"alpha"}}}"#,
            ),
            ev(
                "contentBlockDelta",
                r#"{"contentBlockIndex":2,"delta":{"toolUse":{"input":"{\"b\": 1, \"a\": 2}"}}}"#,
            ),
            ev("contentBlockStop", r#"{"contentBlockIndex":2}"#),
            ev(
                "contentBlockStart",
                r#"{"contentBlockIndex":3,"start":{"toolUse":{"toolUseId":"call_2","name":"beta"}}}"#,
            ),
            ev(
                "contentBlockDelta",
                r#"{"contentBlockIndex":3,"delta":{"toolUse":{"input":"{\"y\": 2}"}}}"#,
            ),
            ev("contentBlockStop", r#"{"contentBlockIndex":3}"#),
            ev("messageStop", r#"{"stopReason":"tool_use"}"#),
            ev("metadata", USAGE),
        ])
    }

    pub fn truncated_text() -> Vec<u8> {
        join(vec![
            ev(
                "contentBlockDelta",
                r#"{"contentBlockIndex":0,"delta":{"text":"truncated"}}"#,
            ),
            ev("messageStop", r#"{"stopReason":"max_tokens"}"#),
        ])
    }

    pub fn truncated_tool() -> Vec<u8> {
        join(vec![
            ev(
                "contentBlockStart",
                r#"{"contentBlockIndex":0,"start":{"toolUse":{"toolUseId":"call_1","name":"alpha"}}}"#,
            ),
            ev(
                "contentBlockDelta",
                r#"{"contentBlockIndex":0,"delta":{"toolUse":{"input":"{\"cmd\": \"ec"}}}"#,
            ),
            ev("messageStop", r#"{"stopReason":"max_tokens"}"#),
        ])
    }

    pub fn mid_stream_error() -> Vec<u8> {
        join(vec![
            ev(
                "contentBlockDelta",
                r#"{"contentBlockIndex":0,"delta":{"text":"start"}}"#,
            ),
            frame(
                "internalServerException",
                "exception",
                r#"{"message":"upstream exploded"}"#,
            ),
        ])
    }
}
