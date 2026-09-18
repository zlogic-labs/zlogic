use std::sync::Arc;

use async_stream::try_stream;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use zlogic_protocol::config::{ModelCapabilities, ThinkingCapability};
use zlogic_protocol::llm::{
    Effort, FinishReason, LlmError, LlmEvent, LlmRequest, ResponseFormat, ThinkingIntent,
    ThinkingMode,
};
use zlogic_protocol::message::{ContentPart, Role};

use crate::chat::{effort_for, lowest_effort};
use crate::emitter::{PartEmitter, ReasoningRawSpec};
use crate::transport::HttpTransport;
use crate::usage_map;
use crate::{Endpoint, EventStream, LlmClient, error};

pub struct ResponsesClient {
    endpoint: Arc<Endpoint>,
    transport: Arc<dyn HttpTransport>,
    max_output_tokens: Option<u64>,
}

impl ResponsesClient {
    pub fn new(
        endpoint: Endpoint,
        transport: Arc<dyn HttpTransport>,
        max_output_tokens: Option<u64>,
    ) -> Self {
        Self {
            endpoint: Arc::new(endpoint),
            transport,
            max_output_tokens,
        }
    }
}

#[async_trait]
impl LlmClient for ResponsesClient {
    async fn stream(&self, req: LlmRequest) -> Result<EventStream, LlmError> {
        let mut warnings = Vec::new();
        let body = build_body(
            &req,
            &self.endpoint.capabilities,
            self.max_output_tokens,
            &mut warnings,
        );
        let payload = serde_json::to_vec(&body).map_err(|e| {
            error::err(
                zlogic_protocol::llm::LlmErrorKind::BadRequest,
                e.to_string(),
            )
        })?;

        let mut http = self.endpoint.request("v1/responses", payload);
        http = self.endpoint.authorize(http, crate::AuthHeader::Bearer);
        for (k, v) in &self.endpoint.extra_headers {
            http = http.header(k.clone(), v.clone());
        }

        let bytes = self
            .transport
            .post_stream(http)
            .await
            .map_err(|e| crate::error::attach_endpoint(e, &self.endpoint, "v1/responses"))?;
        let endpoint = self.endpoint.clone();
        Ok(Box::pin(drive(bytes, warnings).map(move |item| {
            item.map_err(|e| crate::error::attach_endpoint(e, &endpoint, "v1/responses"))
        })))
    }
}

pub(crate) fn build_body(
    req: &LlmRequest,
    caps: &ModelCapabilities,
    max_output_tokens: Option<u64>,
    warnings: &mut Vec<String>,
) -> Map<String, Value> {
    let mut body = Map::new();
    for (k, v) in &req.params {
        body.insert(k.clone(), v.clone());
    }

    if let Some(r) = map_thinking(&req.thinking, &caps.thinking, warnings) {
        body.insert("reasoning".into(), r);
    }
    if let Some(k) = &req.cache.prompt_key {
        body.insert("prompt_cache_key".into(), json!(k));
    }

    if !req.system.is_empty() {
        let text = req
            .system
            .iter()
            .map(|p| p.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        body.insert("instructions".into(), json!(text));
    }

    body.insert("input".into(), json!(build_input(req, caps, warnings)));
    body.insert("model".into(), json!(req.model));
    body.insert("stream".into(), json!(true));
    body.insert("store".into(), json!(false));
    if caps.thinking.supported {
        body.insert("include".into(), json!(["reasoning.encrypted_content"]));
    }
    if let Some(m) = max_output_tokens
        && !body.contains_key("max_output_tokens")
    {
        body.insert("max_output_tokens".into(), json!(m));
    }

    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                })
            })
            .collect();
        body.insert("tools".into(), json!(tools));
    }

    match &req.response_format {
        Some(ResponseFormat::Json) => {
            body.insert(
                "text".into(),
                json!({ "format": { "type": "json_object" } }),
            );
        }
        Some(ResponseFormat::JsonSchema {
            name,
            schema,
            strict,
        }) => {
            body.insert(
                "text".into(),
                json!({ "format": {
                    "type": "json_schema", "name": name, "schema": schema, "strict": strict
                }}),
            );
        }
        None => {}
    }

    body
}

fn map_thinking(
    intent: &ThinkingIntent,
    caps: &ThinkingCapability,
    warnings: &mut Vec<String>,
) -> Option<Value> {
    if !caps.supported {
        if intent.mode == ThinkingMode::On {
            warnings.push("thinking requested but the model does not support it".into());
        }
        return None;
    }

    let effort = match intent.mode {
        ThinkingMode::Default => None,
        ThinkingMode::On => effort_for(intent, &caps.efforts),
        ThinkingMode::Off => {
            if caps.can_disable {
                Some(lowest_effort(&caps.efforts).unwrap_or(Effort::Minimal))
            } else {
                warnings.push(
                    "thinking cannot be fully disabled on this model; letting it use its own default"
                        .into(),
                );
                None
            }
        }
    };

    let mut reasoning = Map::new();
    if let Some(e) = effort {
        reasoning.insert("effort".into(), json!(e.as_str()));
    }
    reasoning.insert("summary".into(), json!("auto"));
    Some(Value::Object(reasoning))
}

fn build_input(
    req: &LlmRequest,
    caps: &ModelCapabilities,
    warnings: &mut Vec<String>,
) -> Vec<Value> {
    let mut items = Vec::new();

    for msg in &req.messages {
        match msg.role {
            Role::System | Role::User => {
                let content: Vec<Value> = msg
                    .content
                    .iter()
                    .filter_map(|p| match p {
                        ContentPart::Text(t) => {
                            Some(json!({ "type": "input_text", "text": t.text }))
                        }
                        ContentPart::Image(img) => {
                            Some(match crate::media::resolve(img, caps, warnings) {
                                crate::media::Resolved::Image { mime, base64 } => json!({
                                    "type": "input_image",
                                    "image_url": format!("data:{mime};base64,{base64}")
                                }),
                                crate::media::Resolved::Placeholder(text) => {
                                    json!({ "type": "input_text", "text": text })
                                }
                            })
                        }
                        _ => None,
                    })
                    .collect();
                if !content.is_empty() {
                    items.push(json!({ "type": "message", "role": "user", "content": content }));
                }
            }
            Role::Assistant => {
                for part in &msg.content {
                    match part {
                        ContentPart::Reasoning(r) => {
                            let Some(raw) = r.raw.as_ref().and_then(|v| v.as_object()) else {
                                if r.raw.is_some() {
                                    warnings.push("dropped unrecognised reasoning raw".into());
                                }
                                continue;
                            };
                            if !raw.contains_key("encrypted_content") {
                                warnings.push(
                                    "dropped a reasoning item without encrypted_content (store:false cannot replay by id)"
                                        .into(),
                                );
                                continue;
                            }
                            let mut item = json!({ "type": "reasoning", "summary": [] });
                            for (k, v) in raw {
                                item[k.as_str()] = v.clone();
                            }
                            items.push(item);
                        }
                        ContentPart::Text(t) => {
                            if !t.text.is_empty() {
                                items.push(json!({
                                    "type": "message",
                                    "role": "assistant",
                                    "content": [{ "type": "output_text", "text": t.text }]
                                }));
                            }
                        }
                        ContentPart::ToolCall(tc) => {
                            for c in &tc.calls {
                                let mut item = json!({
                                    "type": "function_call",
                                    "call_id": c.id,
                                    "name": c.name,
                                    "arguments": c.args,
                                });
                                if let Some(item_id) = c
                                    .raw
                                    .as_ref()
                                    .and_then(|r| r.get("itemId"))
                                    .and_then(|i| i.as_str())
                                {
                                    item["id"] = json!(item_id);
                                }
                                items.push(item);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Role::Tool => {
                for part in &msg.content {
                    if let ContentPart::ToolResult(r) = part {
                        items.push(json!({
                            "type": "function_call_output",
                            "call_id": r.call_id,
                            "output": r.content,
                        }));
                    }
                }
            }
        }
    }

    items
}

fn drive(
    bytes: crate::transport::ByteStream,
    initial_warnings: Vec<String>,
) -> impl futures_core::Stream<Item = Result<LlmEvent, LlmError>> {
    try_stream! {
        let mut sse = crate::sse::events(bytes);
        let mut em = PartEmitter::new(ReasoningRawSpec::Explicit);
        for w in initial_warnings {
            em.warn(w);
        }
        let mut st = State::default();

        while let Some(ev) = sse.next().await {
            let ev = ev?;
            handle(&ev.data, &mut em, &mut st)?;
            for e in em.drain() {
                yield e;
            }
        }

        if let Some(u) = st.usage.take() {
            em.usage(u);
        }
        em.finish(st.finish);
        for e in em.drain() {
            yield e;
        }
    }
}

#[derive(Default)]
struct State {
    call_of_item: Vec<(String, u32)>,
    next_call: u32,
    finish: FinishReason,
    usage: Option<zlogic_protocol::usage::UsageReport>,
}

impl State {
    fn call_index(&self, item_id: &str) -> Option<u32> {
        self.call_of_item
            .iter()
            .find(|(k, _)| k == item_id)
            .map(|(_, v)| *v)
    }
}

fn handle(data: &str, em: &mut PartEmitter, st: &mut State) -> Result<(), LlmError> {
    let Ok(v) = serde_json::from_str::<Value>(data) else {
        em.warn(format!("unparseable SSE data chunk ({} bytes)", data.len()));
        return Ok(());
    };

    let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match ty {
        "error" | "response.failed" => {
            let msg = v
                .pointer("/response/error/message")
                .or_else(|| v.pointer("/error/message"))
                .and_then(|m| m.as_str())
                .unwrap_or(data);
            return Err(error::classify_body(error::protocol(msg.to_string()), data));
        }

        "response.output_item.added" => {
            let item = v.get("item").cloned().unwrap_or(Value::Null);
            match item.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                "reasoning" => em.open_reasoning(),
                "function_call" => {
                    let idx = st.next_call;
                    st.next_call += 1;
                    if let Some(item_id) = item.get("id").and_then(|i| i.as_str()) {
                        st.call_of_item.push((item_id.to_string(), idx));
                        em.tool_call_raw(idx, json!({ "itemId": item_id }));
                    }
                    if let Some(call_id) = item.get("call_id").and_then(|i| i.as_str()) {
                        em.tool_call_id(idx, call_id);
                    }
                    if let Some(name) = item.get("name").and_then(|n| n.as_str()) {
                        em.tool_call_name(idx, name);
                    }
                }
                _ => {}
            }
        }

        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            if let Some(d) = v.get("delta").and_then(|d| d.as_str()) {
                em.reasoning_delta(d);
            }
        }

        "response.output_text.delta" => {
            if let Some(d) = v.get("delta").and_then(|d| d.as_str()) {
                em.text_delta(d);
            }
        }

        "response.function_call_arguments.delta" => {
            let idx = v
                .get("item_id")
                .and_then(|i| i.as_str())
                .and_then(|id| st.call_index(id))
                .unwrap_or(st.next_call.saturating_sub(1));
            if let Some(d) = v.get("delta").and_then(|d| d.as_str()) {
                em.tool_call_args(idx, d);
            }
        }

        "response.output_item.done" => {
            let item = v.get("item").cloned().unwrap_or(Value::Null);
            if item.get("type").and_then(|t| t.as_str()) == Some("reasoning") {
                let mut raw = Map::new();
                if let Some(id) = item.get("id") {
                    raw.insert("id".into(), id.clone());
                }
                if let Some(enc) = item.get("encrypted_content").filter(|e| !e.is_null()) {
                    raw.insert("encrypted_content".into(), enc.clone());
                }
                if !raw.is_empty() {
                    em.set_reasoning_raw(Value::Object(raw));
                }
            }
        }

        "response.completed" | "response.incomplete" => {
            if let Some(u) = v.pointer("/response/usage") {
                st.usage = Some(usage_map::extract(
                    u,
                    &usage_map::RESPONSES,
                    &Default::default(),
                ));
            }
            if let Some(reason) = v
                .pointer("/response/incomplete_details/reason")
                .and_then(|r| r.as_str())
            {
                st.finish = match reason {
                    "max_output_tokens" => FinishReason::Length,
                    "content_filter" => FinishReason::ContentFilter,
                    _ => FinishReason::Other,
                };
            } else if em.has_tool_calls() {
                st.finish = FinishReason::ToolCalls;
            }
        }

        _ => {}
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zlogic_protocol::llm::RequestMeta;
    use zlogic_protocol::message::{
        Message, ReasoningPart, Source, TextPart, ToolCall, ToolCallPart, ToolResultPart,
    };
    use zlogic_protocol::usage::Purpose;

    fn req(messages: Vec<Message>) -> LlmRequest {
        LlmRequest {
            model: "gpt-5".into(),
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

    fn caps(can_disable: bool) -> ModelCapabilities {
        ModelCapabilities {
            vision: Some(true),
            thinking: ThinkingCapability {
                supported: true,
                can_disable,
                efforts: vec![],
                budget: false,
            },
        }
    }

    #[test]
    fn stateless_mode_always_asks_for_encrypted_reasoning() {
        let mut w = Vec::new();
        let body = build_body(&req(vec![]), &caps(false), None, &mut w);
        assert_eq!(body["store"], false);
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    }

    #[test]
    fn a_non_reasoning_model_is_not_asked_for_encrypted_reasoning() {
        let mut w = Vec::new();
        let body = build_body(&req(vec![]), &ModelCapabilities::default(), None, &mut w);
        assert_eq!(
            body["store"], false,
            "store is unrelated to reasoning; off either way"
        );
        assert!(!body.contains_key("include"));
        assert!(!body.contains_key("reasoning"));
    }

    #[test]
    fn every_thinking_mode_asks_for_a_reasoning_summary() {
        for mode in [ThinkingMode::Default, ThinkingMode::On, ThinkingMode::Off] {
            let mut w = Vec::new();
            let mut r = req(vec![]);
            r.thinking = ThinkingIntent {
                mode,
                ..Default::default()
            };
            let body = build_body(&r, &caps(false), None, &mut w);
            assert_eq!(
                body["reasoning"]["summary"], "auto",
                "{mode:?} needs a summary too — it is an axis orthogonal to effort"
            );
        }
    }

    #[test]
    fn default_mode_sends_a_summary_without_pinning_effort() {
        let mut w = Vec::new();
        let body = build_body(&req(vec![]), &caps(true), None, &mut w);
        assert!(
            body["reasoning"].get("effort").is_none(),
            "Default must not pin a tier: {}",
            body["reasoning"]
        );
        assert_eq!(body["reasoning"]["summary"], "auto");
    }

    #[test]
    fn the_prompt_cache_key_reaches_the_wire() {
        let mut w = Vec::new();
        let mut r = req(vec![]);
        r.cache.prompt_key = Some("session-abc".into());
        let body = build_body(&r, &caps(false), None, &mut w);
        assert_eq!(body["prompt_cache_key"], "session-abc");
    }

    #[test]
    fn reasoning_item_is_replayed_with_id_and_encrypted_state() {
        let r = req(vec![Message::assistant(
            Source::new("openai", "gpt-5"),
            vec![
                ContentPart::Reasoning(ReasoningPart {
                    text: "Considering…".into(),
                    raw: Some(json!({ "id": "rs_1", "encrypted_content": "gAAAA" })),
                    truncated: false,
                }),
                ContentPart::ToolCall(ToolCallPart {
                    calls: vec![ToolCall {
                        id: "call_abc".into(),
                        name: "get_weather".into(),
                        args: "{\"city\":\"SF\"}".into(),
                        raw: Some(json!({ "itemId": "fc_1" })),
                    }],
                }),
            ],
        )]);
        let mut w = Vec::new();
        let body = build_body(&r, &caps(false), None, &mut w);
        let input = body["input"].as_array().unwrap();

        assert_eq!(input[0]["type"], "reasoning");
        assert_eq!(input[0]["id"], "rs_1");
        assert_eq!(input[0]["encrypted_content"], "gAAAA");

        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(
            input[1]["call_id"], "call_abc",
            "results correlate by call_id"
        );
        assert_eq!(
            input[1]["id"], "fc_1",
            "itemId is the item's identity; the two must not be mixed up"
        );
        assert_eq!(input[1]["arguments"], "{\"city\":\"SF\"}", "args verbatim");
    }

    #[test]
    fn id_only_reasoning_is_dropped_under_stateless_mode() {
        let r = req(vec![Message::assistant(
            Source::new("openai", "gpt-5"),
            vec![ContentPart::Reasoning(ReasoningPart {
                text: "Considering…".into(),
                raw: Some(json!({ "id": "rs_1" })),
                truncated: false,
            })],
        )]);
        let mut w = Vec::new();
        let body = build_body(&r, &caps(false), None, &mut w);
        assert!(
            body["input"].as_array().unwrap().is_empty(),
            "replaying an id-only reasoning item under store:false is always a 400"
        );
        assert!(!w.is_empty(), "must leave a trace");
    }

    #[test]
    fn tool_results_become_function_call_output_items() {
        let r = req(vec![Message::tool(vec![ContentPart::ToolResult(
            ToolResultPart {
                files: Vec::new(),
                call_id: "call_abc".into(),
                name: "x".into(),
                content: "done".into(),
                is_error: false,
            },
        )])]);
        let mut w = Vec::new();
        let body = build_body(&r, &caps(false), None, &mut w);
        assert_eq!(body["input"][0]["type"], "function_call_output");
        assert_eq!(body["input"][0]["call_id"], "call_abc");
    }

    #[test]
    fn off_on_undisableable_omits_effort_and_warns() {
        let mut r = req(vec![]);
        r.thinking = ThinkingIntent {
            mode: ThinkingMode::Off,
            effort: None,
            budget_tokens: None,
        };
        let mut w = Vec::new();
        let body = build_body(&r, &caps(false), None, &mut w);
        assert!(
            body["reasoning"].get("effort").is_none(),
            "reasoning_effort must not be sent when it cannot be fully disabled — the gateway returns 400"
        );
        assert!(
            !w.is_empty(),
            "the model gets the default, but this still deserves a note"
        );
    }

    #[test]
    fn off_on_disableable_floors_to_minimal() {
        let mut r = req(vec![]);
        r.thinking = ThinkingIntent {
            mode: ThinkingMode::Off,
            effort: None,
            budget_tokens: None,
        };
        let mut w = Vec::new();
        let body = build_body(&r, &caps(true), None, &mut w);
        assert_eq!(body["reasoning"]["effort"], "minimal");
        assert!(w.is_empty());
    }

    #[test]
    fn unrecognised_reasoning_raw_is_dropped() {
        let r = req(vec![Message::assistant(
            Source::new("openai", "gpt-5"),
            vec![ContentPart::Reasoning(ReasoningPart {
                text: "x".into(),
                raw: Some(json!({ "carrier": "reasoning_content", "value": "x" })),
                truncated: false,
            })],
        )]);
        let mut w = Vec::new();
        let body = build_body(&r, &caps(false), None, &mut w);
        assert!(body["input"].as_array().unwrap().is_empty());
        assert!(!w.is_empty());
    }

    #[test]
    fn assistant_text_becomes_an_output_text_message() {
        let r = req(vec![Message::assistant(
            Source::new("openai", "gpt-5"),
            vec![ContentPart::Text(TextPart {
                text: "hi".into(),
                raw: None,
                truncated: false,
            })],
        )]);
        let mut w = Vec::new();
        let body = build_body(&r, &caps(false), None, &mut w);
        assert_eq!(body["input"][0]["content"][0]["type"], "output_text");
    }
}
