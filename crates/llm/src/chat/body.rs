use serde_json::{Map, Value, json};
use zlogic_protocol::llm::{LlmRequest, ResponseFormat};
use zlogic_protocol::message::{ContentPart, Message, Role};

use super::ChatVendor;
use crate::emitter::ReasoningRawSpec;

pub fn build_body<V: ChatVendor + ?Sized>(
    req: &LlmRequest,
    vendor: &V,
    capabilities: &zlogic_protocol::config::ModelCapabilities,
    warnings: &mut Vec<String>,
) -> Map<String, Value> {
    let mut body = Map::new();

    for (k, v) in &req.params {
        body.insert(k.clone(), v.clone());
    }

    for (k, v) in vendor.map_thinking(&req.thinking, &capabilities.thinking, warnings) {
        body.insert(k, v);
    }

    if vendor.supports_prompt_cache_key()
        && let Some(k) = &req.cache.prompt_key
    {
        body.insert("prompt_cache_key".into(), json!(k));
    }

    vendor.transform_body(&mut body);

    body.insert("model".into(), json!(req.model));
    body.insert(
        "messages".into(),
        json!(build_messages(req, vendor, capabilities, warnings)),
    );
    body.insert("stream".into(), json!(true));
    if vendor.wants_stream_options() {
        body.insert("stream_options".into(), json!({ "include_usage": true }));
    }
    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                })
            })
            .collect();
        body.insert("tools".into(), json!(tools));
    }
    if let Some(rf) = &req.response_format {
        body.insert(
            "response_format".into(),
            vendor.response_format(rf, warnings),
        );
    }

    body
}

pub(crate) fn response_format(rf: &ResponseFormat) -> Value {
    match rf {
        ResponseFormat::Json => json!({ "type": "json_object" }),
        ResponseFormat::JsonSchema {
            name,
            schema,
            strict,
        } => json!({
            "type": "json_schema",
            "json_schema": { "name": name, "schema": schema, "strict": strict }
        }),
    }
}

fn build_messages<V: ChatVendor + ?Sized>(
    req: &LlmRequest,
    vendor: &V,
    caps: &zlogic_protocol::config::ModelCapabilities,
    warnings: &mut Vec<String>,
) -> Vec<Value> {
    let mut out = Vec::new();

    if !req.system.is_empty() {
        let text = req
            .system
            .iter()
            .map(|p| p.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        out.push(json!({ "role": "system", "content": text }));
    }

    for msg in &req.messages {
        match msg.role {
            Role::System => {
                out.push(json!({ "role": "system", "content": plain_text(msg) }));
            }
            Role::User => out.push(user_message(msg, caps, warnings)),
            Role::Assistant => out.push(assistant_message(msg, vendor)),
            Role::Tool => {
                for part in &msg.content {
                    if let ContentPart::ToolResult(r) = part {
                        out.push(json!({
                            "role": "tool",
                            "tool_call_id": r.call_id,
                            "content": r.content,
                        }));
                    }
                }
            }
        }
    }

    out
}

fn plain_text(msg: &Message) -> String {
    msg.content
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn user_message(
    msg: &Message,
    caps: &zlogic_protocol::config::ModelCapabilities,
    warnings: &mut Vec<String>,
) -> Value {
    let all_text = msg
        .content
        .iter()
        .all(|p| matches!(p, ContentPart::Text(_)));
    if all_text {
        return json!({ "role": "user", "content": plain_text(msg) });
    }

    let parts: Vec<Value> = msg
        .content
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text(t) => Some(json!({ "type": "text", "text": t.text })),
            ContentPart::Image(img) => Some(match crate::media::resolve(img, caps, warnings) {
                crate::media::Resolved::Image { mime, base64 } => json!({
                    "type": "image_url",
                    "image_url": { "url": format!("data:{mime};base64,{base64}") }
                }),
                crate::media::Resolved::Placeholder(text) => {
                    json!({ "type": "text", "text": text })
                }
            }),
            _ => None,
        })
        .collect();

    json!({ "role": "user", "content": parts })
}

fn assistant_message<V: ChatVendor + ?Sized>(msg: &Message, vendor: &V) -> Value {
    let mut obj = Map::new();
    obj.insert("role".into(), json!("assistant"));

    let text = plain_text(msg);
    obj.insert("content".into(), json!(text));

    let mut spliced = false;
    for part in &msg.content {
        if let ContentPart::Reasoning(r) = part
            && let Some(raw) = &r.raw
        {
            let before = obj.len();
            vendor.splice_reasoning(&mut obj, raw);
            spliced |= obj.len() != before;
        }
    }
    if !spliced
        && vendor.always_send_reasoning_field()
        && let ReasoningRawSpec::Carrier(carrier) = vendor.reasoning_raw_spec()
    {
        obj.insert(carrier, json!(""));
    }

    let calls: Vec<Value> = msg
        .content
        .iter()
        .filter_map(|p| match p {
            ContentPart::ToolCall(tc) => Some(&tc.calls),
            _ => None,
        })
        .flatten()
        .map(|c| {
            let mut call = json!({
                "id": c.id,
                "type": "function",
                "function": { "name": c.name, "arguments": c.args },
            });
            if let (Some(raw), Some(map)) = (
                c.raw.as_ref().and_then(|v| v.as_object()),
                call.as_object_mut(),
            ) {
                for (k, v) in raw {
                    map.insert(k.clone(), v.clone());
                }
            }
            call
        })
        .collect();
    if !calls.is_empty() {
        obj.insert("tool_calls".into(), json!(calls));
    }

    Value::Object(obj)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::vendors::DeepSeek;
    use zlogic_protocol::config::ModelCapabilities;
    use zlogic_protocol::llm::{RequestMeta, ThinkingIntent};
    use zlogic_protocol::message::{
        ContentPart, ImageSource, ReasoningPart, Source, TextPart, ToolCall, ToolCallPart,
        ToolResultPart,
    };
    use zlogic_protocol::usage::Purpose;

    fn req(messages: Vec<Message>) -> LlmRequest {
        LlmRequest {
            model: "m".into(),
            system: vec![],
            messages,
            tools: vec![],
            cache: Default::default(),
            thinking: ThinkingIntent::default(),
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

    #[test]
    fn tool_results_become_separate_messages_one_per_call() {
        let r = req(vec![
            Message::assistant(
                Source::new("deepseek", "v4"),
                vec![ContentPart::ToolCall(ToolCallPart {
                    calls: vec![
                        ToolCall {
                            id: "c1".into(),
                            name: "a".into(),
                            args: "{}".into(),
                            raw: None,
                        },
                        ToolCall {
                            id: "c2".into(),
                            name: "b".into(),
                            args: "{}".into(),
                            raw: None,
                        },
                    ],
                })],
            ),
            Message::tool(vec![
                ContentPart::ToolResult(ToolResultPart {
                    files: Vec::new(),
                    call_id: "c1".into(),
                    name: "a".into(),
                    content: "r1".into(),
                    is_error: false,
                }),
                ContentPart::ToolResult(ToolResultPart {
                    files: Vec::new(),
                    call_id: "c2".into(),
                    name: "b".into(),
                    content: "r2".into(),
                    is_error: false,
                }),
            ]),
        ]);
        let mut w = Vec::new();
        let body = build_body(&r, &DeepSeek, &ModelCapabilities::default(), &mut w);
        let msgs = body["messages"].as_array().unwrap();

        assert_eq!(msgs.len(), 3, "1 assistant + 2 tool messages");
        assert_eq!(msgs[1]["role"], "tool");
        assert_eq!(msgs[1]["tool_call_id"], "c1");
        assert_eq!(msgs[2]["tool_call_id"], "c2");
        assert_eq!(msgs[0]["content"], "");
    }

    #[test]
    fn reasoning_raw_is_spliced_into_the_message_field() {
        let r = req(vec![Message::assistant(
            Source::new("deepseek", "v4"),
            vec![
                ContentPart::Reasoning(ReasoningPart {
                    text: "display projection".into(),
                    raw: Some(json!({ "carrier": "reasoning_content", "value": "raw payload" })),
                    truncated: false,
                }),
                ContentPart::Text(TextPart {
                    text: "answer".into(),
                    raw: None,
                    truncated: false,
                }),
            ],
        )]);
        let mut w = Vec::new();
        let body = build_body(&r, &DeepSeek, &ModelCapabilities::default(), &mut w);
        let m = &body["messages"][0];
        assert_eq!(
            m["reasoning_content"], "raw payload",
            "replay must use raw, not the displayed text"
        );
        assert_eq!(m["content"], "answer");
    }

    #[test]
    fn deepseek_always_carries_the_reasoning_field() {
        let r = req(vec![Message::assistant(
            Source::new("deepseek", "v4"),
            vec![
                ContentPart::Text(TextPart {
                    text: "calling a tool".into(),
                    raw: None,
                    truncated: false,
                }),
                ContentPart::ToolCall(ToolCallPart {
                    calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "shell".into(),
                        args: "{}".into(),
                        raw: None,
                    }],
                }),
            ],
        )]);
        let mut w = Vec::new();
        let body = build_body(&r, &DeepSeek, &ModelCapabilities::default(), &mut w);
        assert_eq!(
            body["messages"][0]["reasoning_content"], "",
            "a turn carrying tool_calls but no reasoning_content is a 400"
        );
    }

    #[test]
    fn always_send_never_overwrites_a_real_payload() {
        let r = req(vec![Message::assistant(
            Source::new("deepseek", "v4"),
            vec![ContentPart::Reasoning(ReasoningPart {
                text: "display projection".into(),
                raw: Some(json!({ "carrier": "reasoning_content", "value": "raw payload" })),
                truncated: false,
            })],
        )]);
        let mut w = Vec::new();
        let body = build_body(&r, &DeepSeek, &ModelCapabilities::default(), &mut w);
        assert_eq!(body["messages"][0]["reasoning_content"], "raw payload");
    }

    #[test]
    fn deepseek_sends_the_chosen_effort_on_the_wire() {
        use zlogic_protocol::config::ThinkingCapability;
        use zlogic_protocol::llm::{Effort, ThinkingMode};

        let mut r = req(vec![Message::user(vec![ContentPart::Text(TextPart {
            text: "hi".into(),
            raw: None,
            truncated: false,
        })])]);
        r.thinking = ThinkingIntent {
            mode: ThinkingMode::On,
            effort: Some(Effort::Low),
            budget_tokens: None,
        };
        let caps = ModelCapabilities {
            vision: None,
            thinking: ThinkingCapability {
                supported: true,
                can_disable: true,
                efforts: vec![Effort::Low, Effort::High, Effort::Max],
                budget: false,
            },
        };
        let mut w = Vec::new();
        let body = build_body(&r, &DeepSeek, &caps, &mut w);
        assert_eq!(body["thinking"], json!({ "type": "enabled" }));
        assert_eq!(body["reasoning_effort"], "low");

        let mut r2 = req(vec![]);
        r2.thinking = ThinkingIntent {
            mode: ThinkingMode::On,
            effort: Some(Effort::High),
            budget_tokens: None,
        };
        let caps2 = ModelCapabilities {
            vision: None,
            thinking: ThinkingCapability {
                supported: true,
                can_disable: true,
                efforts: vec![],
                budget: false,
            },
        };
        let mut w = Vec::new();
        let body = build_body(&r2, &DeepSeek, &caps2, &mut w);
        assert_eq!(body["thinking"], json!({ "type": "enabled" }));
        assert!(!body.contains_key("reasoning_effort"), "{body:?}");
    }

    #[test]
    fn the_prompt_cache_key_only_goes_to_endpoints_that_accept_it() {
        use crate::chat::vendors::VanillaOpenAi;
        let mut w = Vec::new();
        let mut r = req(vec![]);
        r.cache.prompt_key = Some("session-abc".into());

        let openai = build_body(&r, &VanillaOpenAi, &ModelCapabilities::default(), &mut w);
        assert_eq!(openai["prompt_cache_key"], "session-abc");

        let deepseek = build_body(&r, &DeepSeek, &ModelCapabilities::default(), &mut w);
        assert!(
            !deepseek.contains_key("prompt_cache_key"),
            "DeepSeek does not accept this field"
        );
    }

    #[test]
    fn vanilla_openai_never_gets_a_reasoning_field() {
        use crate::chat::vendors::VanillaOpenAi;
        let r = req(vec![Message::assistant(
            Source::new("openai", "gpt-5"),
            vec![ContentPart::Text(TextPart {
                text: "hi".into(),
                raw: None,
                truncated: false,
            })],
        )]);
        let mut w = Vec::new();
        let body = build_body(&r, &VanillaOpenAi, &ModelCapabilities::default(), &mut w);
        let m = body["messages"][0].as_object().unwrap();
        assert!(!m.contains_key("reasoning_content"));
        assert!(!m.contains_key("reasoning"));
    }

    #[test]
    fn unknown_raw_shape_is_dropped_not_forced_into_the_wire() {
        let r = req(vec![Message::assistant(
            Source::new("deepseek", "v4"),
            vec![ContentPart::Reasoning(ReasoningPart {
                text: "x".into(),
                raw: Some(json!({ "type": "thinking", "signature": "sig" })),
                truncated: false,
            })],
        )]);
        let mut w = Vec::new();
        let body = build_body(&r, &DeepSeek, &ModelCapabilities::default(), &mut w);
        let m = body["messages"][0].as_object().unwrap();
        assert!(
            !m.contains_key("signature"),
            "another vendor's field must never reach the wire"
        );
        assert_eq!(m["reasoning_content"], "");
    }

    #[test]
    fn tool_call_raw_merges_as_extra_fields_not_wholesale() {
        let r = req(vec![Message::assistant(
            Source::new("openai", "gpt"),
            vec![ContentPart::ToolCall(ToolCallPart {
                calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "shell".into(),
                    args: "{\"cmd\":\"ls\"}".into(),
                    raw: Some(json!({ "itemId": "fc_1" })),
                }],
            })],
        )]);
        let mut w = Vec::new();
        let body = build_body(&r, &DeepSeek, &ModelCapabilities::default(), &mut w);
        let call = &body["messages"][0]["tool_calls"][0];
        assert_eq!(
            call["id"], "c1",
            "expanding the whole blob would lose the id"
        );
        assert_eq!(call["function"]["name"], "shell");
        assert_eq!(call["function"]["arguments"], "{\"cmd\":\"ls\"}");
        assert_eq!(call["itemId"], "fc_1");
    }

    #[test]
    fn framework_fields_win_over_params() {
        let mut r = req(vec![]);
        r.params.insert("model".into(), json!("hacked"));
        r.params.insert("stream".into(), json!(false));
        r.params.insert("max_tokens".into(), json!(4096));
        let mut w = Vec::new();
        let body = build_body(&r, &DeepSeek, &ModelCapabilities::default(), &mut w);
        assert_eq!(body["model"], "m");
        assert_eq!(body["stream"], true);
        assert_eq!(
            body["max_tokens"], 4096,
            "non-framework fields pass through as usual"
        );
    }

    fn with_one_tool(mut r: LlmRequest) -> LlmRequest {
        use zlogic_protocol::llm::ToolDefinition;
        r.tools = vec![ToolDefinition {
            name: "t".into(),
            description: "d".into(),
            parameters: json!({ "type": "object" }),
        }];
        r
    }

    #[test]
    fn no_choice_means_no_field() {
        let r = with_one_tool(req(vec![]));
        let mut w = Vec::new();
        let body = build_body(&r, &DeepSeek, &ModelCapabilities::default(), &mut w);
        assert!(body.contains_key("tools"));
        assert!(!body.contains_key("tool_choice"));
        assert!(
            !body.contains_key("parallel_tool_calls"),
            "the parallelism switch is never touched"
        );
    }

    #[test]
    fn all_text_user_collapses_to_string_media_stays_structured() {
        use zlogic_protocol::message::ImagePart;
        let text_only = req(vec![Message::user(vec![
            ContentPart::Text(TextPart {
                text: "a".into(),
                raw: None,
                truncated: false,
            }),
            ContentPart::Text(TextPart {
                text: "b".into(),
                raw: None,
                truncated: false,
            }),
        ])]);
        let mut w = Vec::new();
        let body = build_body(&text_only, &DeepSeek, &ModelCapabilities::default(), &mut w);
        assert_eq!(body["messages"][0]["content"], "a\n\nb");

        let with_image = req(vec![Message::user(vec![
            ContentPart::Text(TextPart {
                text: "look".into(),
                raw: None,
                truncated: false,
            }),
            ContentPart::Image(ImagePart {
                mime_type: "image/png".into(),
                source: ImageSource::Base64 {
                    data: "iVBORw0KGgo=".into(),
                },
            }),
        ])]);
        let body = build_body(
            &with_image,
            &DeepSeek,
            &ModelCapabilities::default(),
            &mut w,
        );
        let parts = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(
            parts[1]["image_url"]["url"],
            "data:image/png;base64,iVBORw0KGgo="
        );
    }

    #[test]
    fn unresolved_image_path_degrades_and_warns() {
        use zlogic_protocol::message::ImagePart;
        let r = req(vec![Message::user(vec![ContentPart::Image(ImagePart {
            mime_type: "image/png".into(),
            source: ImageSource::Path {
                path: "/tmp/a.png".into(),
            },
        })])]);
        let mut w = Vec::new();
        let body = build_body(&r, &DeepSeek, &ModelCapabilities::default(), &mut w);
        assert_eq!(body["messages"][0]["content"][0]["type"], "text");
        assert!(!w.is_empty(), "must leave a trace");
    }
}
