pub mod eventstream;
pub mod sigv4;

use std::sync::Arc;

use async_stream::try_stream;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use zlogic_protocol::config::{ModelCapabilities, ThinkingCapability};
use zlogic_protocol::llm::{
    FinishReason, LlmError, LlmEvent, LlmRequest, MessageCache, ThinkingIntent, ThinkingMode,
};
use zlogic_protocol::message::{ContentPart, Message, Role};

use crate::anthropic::{parse_args, sanitize_tool_id};
use crate::cache::Breakpoints;
use crate::chat::effort_budget;
use crate::emitter::{PartEmitter, ReasoningRawSpec};
use crate::transport::{HttpRequest, HttpTransport};
use crate::usage_map;
use crate::{Endpoint, EventStream, LlmClient, error};

use eventstream::EventStreamDecoder;
use sigv4::{Credentials, SignInput};

const MIN_THINKING_BUDGET: u32 = 1024;
const FALLBACK_MAX_TOKENS: u64 = 8192;

pub struct BedrockClient {
    endpoint: Arc<Endpoint>,
    transport: Arc<dyn HttpTransport>,
    region: String,
    creds: Credentials,
    max_output_tokens: Option<u64>,
}

impl BedrockClient {
    pub fn new(
        endpoint: Endpoint,
        transport: Arc<dyn HttpTransport>,
        region: impl Into<String>,
        creds: Credentials,
        max_output_tokens: Option<u64>,
    ) -> Self {
        Self {
            endpoint: Arc::new(endpoint),
            transport,
            region: region.into(),
            creds,
            max_output_tokens,
        }
    }
}

#[async_trait]
impl LlmClient for BedrockClient {
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

        let path = format!("/model/{}/converse-stream", urlencode(&req.model));
        let url = format!("{}{}", self.endpoint.base_url.trim_end_matches('/'), path);
        let host = host_of(&url);
        let amz_date = sigv4::amz_date(sigv4::now_unix());

        let mut headers = vec![
            ("host".to_string(), host),
            ("content-type".to_string(), "application/json".to_string()),
            ("x-amz-date".to_string(), amz_date.clone()),
        ];
        let signed = sigv4::sign(
            &SignInput {
                method: "POST",
                path: &path,
                query: "",
                headers: &headers,
                payload: &payload,
                region: &self.region,
                service: "bedrock",
                amz_date: &amz_date,
            },
            &self.creds,
        );
        headers.extend(signed);
        headers.extend(self.endpoint.extra_headers.iter().cloned());

        let http = HttpRequest {
            url,
            headers,
            body: payload,
            network: self.endpoint.network.clone(),
        };
        let bytes = self
            .transport
            .post_stream(http)
            .await
            .map_err(|e| crate::error::attach_endpoint(e, &self.endpoint, &path))?;
        let endpoint = self.endpoint.clone();
        Ok(Box::pin(drive(bytes, warnings).map(move |item| {
            item.map_err(|e| crate::error::attach_endpoint(e, &endpoint, &path))
        })))
    }
}

fn host_of(url: &str) -> String {
    let after_scheme = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    after_scheme.split('/').next().unwrap_or("").to_string()
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub(crate) fn build_body(
    req: &LlmRequest,
    caps: &ModelCapabilities,
    max_output_tokens: Option<u64>,
    warnings: &mut Vec<String>,
) -> Map<String, Value> {
    let mut body = Map::new();

    let mut bp = Breakpoints::new(&req.cache);
    let tool_config = build_tool_config(req, &mut bp);
    let thinking = map_thinking(&req.thinking, &caps.thinking, warnings);

    let mut inference = Map::new();
    for (k, v) in &req.params {
        inference.insert(k.clone(), v.clone());
    }
    let budget = thinking
        .as_ref()
        .and_then(|t| t.get("budget_tokens"))
        .and_then(|b| b.as_u64())
        .unwrap_or(0);
    let want = inference
        .get("maxTokens")
        .and_then(|m| m.as_u64())
        .or(max_output_tokens)
        .unwrap_or(FALLBACK_MAX_TOKENS);
    inference.insert("maxTokens".into(), json!(want.max(budget + 1024)));
    body.insert("inferenceConfig".into(), Value::Object(inference));

    if let Some(t) = thinking {
        body.insert(
            "additionalModelRequestFields".into(),
            json!({ "thinking": t }),
        );
    }

    if !req.system.is_empty() {
        let mut blocks: Vec<Value> = req
            .system
            .iter()
            .map(|p| json!({ "text": p.text }))
            .collect();
        let explicit = req.system.iter().any(|p| p.cache);
        if (explicit || req.cache.system) && bp.take() {
            blocks.push(cache_point(&bp));
        }
        body.insert("system".into(), json!(blocks));
    }

    let mut messages = build_messages(req, caps, warnings);
    if req.cache.messages == MessageCache::LatestUser {
        mark_latest_user(&mut messages, &mut bp);
    }
    body.insert("messages".into(), json!(messages));

    if let Some(cfg) = tool_config {
        body.insert("toolConfig".into(), cfg);
    }

    if bp.dropped() > 0 {
        warnings.push(format!(
            "dropped {} cache breakpoint(s); bedrock allows at most {}",
            bp.dropped(),
            crate::cache::BREAKPOINT_CAP
        ));
    }

    if req.response_format.is_some() {
        warnings.push(
            "bedrock converse has no structured-output field; response_format was ignored".into(),
        );
    }

    body
}

fn cache_point(bp: &Breakpoints) -> Value {
    match bp.ttl() {
        Some(ttl) => json!({ "cachePoint": { "type": "default", "ttl": ttl } }),
        None => json!({ "cachePoint": { "type": "default" } }),
    }
}

fn build_tool_config(req: &LlmRequest, bp: &mut Breakpoints) -> Option<Value> {
    if req.tools.is_empty() {
        return None;
    }
    let mut tools: Vec<Value> = req
        .tools
        .iter()
        .map(|t| {
            json!({ "toolSpec": {
                "name": t.name,
                "description": t.description,
                "inputSchema": { "json": t.parameters },
            }})
        })
        .collect();
    if req.cache.tools && bp.take() {
        tools.push(cache_point(bp));
    }

    let mut cfg = Map::new();
    cfg.insert("tools".into(), json!(tools));
    Some(Value::Object(cfg))
}

fn mark_latest_user(messages: &mut [Value], bp: &mut Breakpoints) {
    let Some(msg) = messages
        .iter_mut()
        .rev()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
    else {
        return;
    };
    let Some(content) = msg.get_mut("content").and_then(|c| c.as_array_mut()) else {
        return;
    };
    if content.iter().any(|b| b.get("cachePoint").is_some()) || !bp.take() {
        return;
    }
    let point = cache_point(bp);
    content.push(point);
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
    match intent.mode {
        ThinkingMode::Default => None,
        ThinkingMode::Off => Some(json!({ "type": "disabled" })),
        ThinkingMode::On => {
            let budget = effort_budget(intent)
                .unwrap_or(8192)
                .max(MIN_THINKING_BUDGET);
            Some(json!({ "type": "enabled", "budget_tokens": budget }))
        }
    }
}

fn build_messages(
    req: &LlmRequest,
    caps: &ModelCapabilities,
    warnings: &mut Vec<String>,
) -> Vec<Value> {
    let mut out: Vec<(&'static str, Vec<Value>, usize)> = Vec::new();

    let mut push = |role: &'static str, blocks: Vec<Value>, front: bool| {
        if blocks.is_empty() {
            return;
        }
        if let Some(last) = out.last_mut()
            && last.0 == role
        {
            if front {
                for b in blocks.into_iter().rev() {
                    last.1.insert(last.2, b);
                }
                last.2 += 1;
            } else {
                last.1.extend(blocks);
            }
            return;
        }
        let pinned = if front { blocks.len() } else { 0 };
        out.push((role, blocks, pinned));
    };

    for msg in &req.messages {
        match msg.role {
            Role::System | Role::User => push("user", user_blocks(msg, caps, warnings), false),
            Role::Assistant => push("assistant", assistant_blocks(msg, warnings), false),
            Role::Tool => {
                for part in &msg.content {
                    if let ContentPart::ToolResult(r) = part {
                        push(
                            "user",
                            vec![json!({ "toolResult": {
                                "toolUseId": sanitize_tool_id(&r.call_id),
                                "content": [{ "text": r.content }],
                                "status": if r.is_error { "error" } else { "success" },
                            }})],
                            true,
                        );
                    }
                }
            }
        }
    }

    out.into_iter()
        .map(|(role, content, _)| json!({ "role": role, "content": content }))
        .collect()
}

fn user_blocks(msg: &Message, caps: &ModelCapabilities, warnings: &mut Vec<String>) -> Vec<Value> {
    msg.content
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text(t) => Some(json!({ "text": t.text })),
            ContentPart::Image(img) => Some(match crate::media::resolve(img, caps, warnings) {
                crate::media::Resolved::Image { mime, base64 } => json!({ "image": {
                    "format": crate::media::bedrock_format(&mime),
                    "source": { "bytes": base64 },
                }}),
                crate::media::Resolved::Placeholder(text) => json!({ "text": text }),
            }),
            _ => None,
        })
        .collect()
}

fn assistant_blocks(msg: &Message, warnings: &mut Vec<String>) -> Vec<Value> {
    let mut blocks = Vec::new();
    for part in &msg.content {
        match part {
            ContentPart::Reasoning(r) => match r.raw.as_ref().and_then(|v| v.as_object()) {
                Some(obj) if obj.contains_key("reasoningContent") => {
                    blocks.push(Value::Object(obj.clone()));
                }
                _ => {
                    if r.raw.is_some() {
                        warnings.push("dropped unrecognised reasoning raw".into());
                    }
                }
            },
            ContentPart::Text(t) => {
                if !t.text.is_empty() {
                    blocks.push(json!({ "text": t.text }));
                }
            }
            ContentPart::ToolCall(tc) => {
                for c in &tc.calls {
                    blocks.push(json!({ "toolUse": {
                        "toolUseId": sanitize_tool_id(&c.id),
                        "name": c.name,
                        "input": parse_args(&c.args),
                    }}));
                }
            }
            _ => {}
        }
    }
    blocks
}

fn drive(
    mut bytes: crate::transport::ByteStream,
    initial_warnings: Vec<String>,
) -> impl futures_core::Stream<Item = Result<LlmEvent, LlmError>> {
    try_stream! {
        let mut dec = EventStreamDecoder::new();
        let mut em = PartEmitter::new(ReasoningRawSpec::Explicit);
        for w in initial_warnings {
            em.warn(w);
        }
        let mut st = State::default();

        while let Some(chunk) = bytes.next().await {
            let chunk = chunk?;
            for msg in dec.push(&chunk)? {
                handle(&msg, &mut em, &mut st)?;
                for e in em.drain() {
                    yield e;
                }
            }
        }
        if dec.has_partial() {
            em.warn("eventstream ended mid-frame");
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
    reasoning_text: String,
    signature: String,
    in_reasoning: bool,
    call_of_block: Vec<(u64, u32)>,
    next_call: u32,
    finish: FinishReason,
    usage: Option<zlogic_protocol::usage::UsageReport>,
}

impl State {
    fn call_index(&self, block: u64) -> Option<u32> {
        self.call_of_block
            .iter()
            .find(|(b, _)| *b == block)
            .map(|(_, c)| *c)
    }
}

fn handle(
    msg: &eventstream::EventMessage,
    em: &mut PartEmitter,
    st: &mut State,
) -> Result<(), LlmError> {
    let text = String::from_utf8_lossy(&msg.payload);

    if msg.is_exception() {
        let kind = msg.header(":exception-type").unwrap_or("exception");
        return Err(error::classify_body(
            error::protocol(format!("{kind}: {text}")),
            &text,
        ));
    }

    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        em.warn(format!(
            "unparseable eventstream payload ({} bytes)",
            msg.payload.len()
        ));
        return Ok(());
    };

    let block_index = v
        .get("contentBlockIndex")
        .and_then(|i| i.as_u64())
        .unwrap_or(0);

    match msg.event_type().unwrap_or("") {
        "contentBlockStart" => {
            if let Some(tu) = v.pointer("/start/toolUse") {
                let idx = st.next_call;
                st.next_call += 1;
                st.call_of_block.push((block_index, idx));
                if let Some(id) = tu.get("toolUseId").and_then(|i| i.as_str()) {
                    em.tool_call_id(idx, id);
                }
                if let Some(name) = tu.get("name").and_then(|n| n.as_str()) {
                    em.tool_call_name(idx, name);
                }
            }
        }

        "contentBlockDelta" => {
            let Some(delta) = v.get("delta") else {
                return Ok(());
            };

            if let Some(rc) = delta.get("reasoningContent") {
                if let Some(t) = rc.get("text").and_then(|t| t.as_str()) {
                    st.in_reasoning = true;
                    st.reasoning_text.push_str(t);
                    em.reasoning_delta(t);
                }
                if let Some(s) = rc.get("signature").and_then(|s| s.as_str()) {
                    st.signature.push_str(s);
                }
                if let Some(r) = rc.get("redactedContent") {
                    em.open_reasoning();
                    em.set_reasoning_raw(json!({
                        "reasoningContent": { "redactedContent": r.clone() }
                    }));
                }
                return Ok(());
            }

            if let Some(t) = delta.get("text").and_then(|t| t.as_str()) {
                em.text_delta(t);
            }
            if let Some(inp) = delta.pointer("/toolUse/input").and_then(|i| i.as_str()) {
                let idx = st
                    .call_index(block_index)
                    .unwrap_or(st.next_call.saturating_sub(1));
                em.tool_call_args(idx, inp);
            }
        }

        "contentBlockStop" => {
            if st.in_reasoning {
                st.in_reasoning = false;
                if !st.signature.is_empty() {
                    em.set_reasoning_raw(json!({ "reasoningContent": { "reasoningText": {
                        "text": st.reasoning_text,
                        "signature": st.signature,
                    }}}));
                } else {
                    em.warn("reasoning block closed without a signature; raw dropped");
                }
                st.reasoning_text.clear();
                st.signature.clear();
            }
        }

        "messageStop" => {
            if let Some(sr) = v.get("stopReason").and_then(|s| s.as_str()) {
                st.finish = match sr {
                    "end_turn" | "stop_sequence" => FinishReason::Stop,
                    "tool_use" => FinishReason::ToolCalls,
                    "max_tokens" => FinishReason::Length,
                    "content_filtered" | "guardrail_intervened" => FinishReason::ContentFilter,
                    _ => FinishReason::Other,
                };
            }
        }

        "metadata" => {
            if let Some(u) = v.get("usage") {
                st.usage = Some(usage_map::extract(
                    u,
                    &usage_map::BEDROCK,
                    &Default::default(),
                ));
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
    use zlogic_protocol::message::{ReasoningPart, Source, ToolCall, ToolCallPart, ToolResultPart};
    use zlogic_protocol::usage::Purpose;

    fn req(messages: Vec<Message>) -> LlmRequest {
        LlmRequest {
            model: "anthropic.claude-opus-4:0".into(),
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

    fn caps() -> ModelCapabilities {
        ModelCapabilities {
            vision: Some(true),
            thinking: ThinkingCapability {
                supported: true,
                can_disable: true,
                efforts: vec![],
                budget: true,
            },
        }
    }

    #[test]
    fn response_format_is_refused_loudly_not_ignored() {
        let mut r = req(vec![]);
        r.response_format = Some(zlogic_protocol::llm::ResponseFormat::JsonSchema {
            name: "v".into(),
            schema: json!({ "type": "object" }),
            strict: true,
        });
        let mut w = Vec::new();
        let body = build_body(&r, &caps(), None, &mut w);

        assert!(
            !body.contains_key("output_config"),
            "Converse has no such field"
        );
        assert!(
            w.iter().any(|m| m.contains("structured-output")),
            "the caller must know it did not take effect: {w:?}"
        );
    }

    #[test]
    fn cache_points_are_appended_as_their_own_blocks() {
        use zlogic_protocol::llm::{SystemPart, ToolDefinition};
        let mut r = req(vec![Message::user(vec![ContentPart::Text(
            zlogic_protocol::message::TextPart {
                text: "question".into(),
                raw: None,
                truncated: false,
            },
        )])]);
        r.system = vec![SystemPart {
            text: "preamble".into(),
            cache: false,
        }];
        r.tools = vec![ToolDefinition {
            name: "t".into(),
            description: "d".into(),
            parameters: json!({ "type": "object" }),
        }];

        let mut w = Vec::new();
        let body = build_body(&r, &caps(), None, &mut w);

        let tools = body["toolConfig"]["tools"].as_array().unwrap();
        assert!(tools[0].get("toolSpec").is_some());
        assert_eq!(tools[1], json!({ "cachePoint": { "type": "default" } }));

        let system = body["system"].as_array().unwrap();
        assert_eq!(system[0], json!({ "text": "preamble" }));
        assert_eq!(system[1], json!({ "cachePoint": { "type": "default" } }));

        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(
            content.last().unwrap(),
            &json!({ "cachePoint": { "type": "default" } })
        );
        assert!(w.is_empty());
    }

    #[test]
    fn bedrock_cache_off_emits_nothing() {
        use zlogic_protocol::llm::CacheSpec;
        let mut r = req(vec![Message::user(vec![ContentPart::Text(
            zlogic_protocol::message::TextPart {
                text: "question".into(),
                raw: None,
                truncated: false,
            },
        )])]);
        r.cache = CacheSpec::off();
        let mut w = Vec::new();
        let body = build_body(&r, &caps(), None, &mut w);
        assert!(!serde_json::to_string(&body).unwrap().contains("cachePoint"));
    }

    #[test]
    fn tool_results_merge_into_one_user_message_at_the_front() {
        let r = req(vec![
            Message::assistant(
                Source::new("bedrock", "claude"),
                vec![ContentPart::ToolCall(ToolCallPart {
                    calls: vec![ToolCall {
                        id: "tu_1".into(),
                        name: "a".into(),
                        args: "{\"z\":1,\"a\":2}".into(),
                        raw: None,
                    }],
                })],
            ),
            Message::tool(vec![ContentPart::ToolResult(ToolResultPart {
                files: Vec::new(),
                call_id: "tu_1".into(),
                name: "a".into(),
                content: "ok".into(),
                is_error: false,
            })]),
        ]);
        let mut w = Vec::new();
        let body = build_body(&r, &caps(), None, &mut w);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1]["content"][0]["toolResult"]["toolUseId"], "tu_1");
        assert_eq!(msgs[1]["content"][0]["toolResult"]["status"], "success");
        assert_eq!(
            serde_json::to_string(&msgs[0]["content"][0]["toolUse"]["input"]).unwrap(),
            "{\"z\":1,\"a\":2}"
        );
    }

    #[test]
    fn reasoning_block_replays_verbatim() {
        let raw = json!({ "reasoningContent": { "reasoningText": {
            "text": "Let me think…", "signature": "sig_1"
        }}});
        let r = req(vec![Message::assistant(
            Source::new("bedrock", "claude"),
            vec![ContentPart::Reasoning(ReasoningPart {
                text: "Let me think…".into(),
                raw: Some(raw.clone()),
                truncated: false,
            })],
        )]);
        let mut w = Vec::new();
        let body = build_body(&r, &caps(), None, &mut w);
        assert_eq!(body["messages"][0]["content"][0], raw);
    }

    #[test]
    fn thinking_goes_through_additional_model_request_fields() {
        let mut r = req(vec![]);
        r.thinking = ThinkingIntent {
            mode: ThinkingMode::On,
            effort: None,
            budget_tokens: Some(20000),
        };
        let mut w = Vec::new();
        let body = build_body(&r, &caps(), None, &mut w);
        assert_eq!(
            body["additionalModelRequestFields"]["thinking"]["budget_tokens"],
            20000
        );
        assert!(body["inferenceConfig"]["maxTokens"].as_u64().unwrap() > 20000);
    }

    #[test]
    fn model_id_is_url_encoded_in_the_path() {
        assert_eq!(
            urlencode("anthropic.claude-opus-4:0"),
            "anthropic.claude-opus-4%3A0"
        );
        assert_eq!(
            host_of("https://bedrock-runtime.us-east-1.amazonaws.com/x"),
            "bedrock-runtime.us-east-1.amazonaws.com"
        );
    }

    #[test]
    fn tool_config_wraps_each_tool_in_a_tool_spec() {
        use zlogic_protocol::llm::ToolDefinition;
        let mut r = req(vec![]);
        r.tools = vec![ToolDefinition {
            name: "shell".into(),
            description: "run".into(),
            parameters: json!({ "type": "object" }),
        }];
        let mut w = Vec::new();
        let body = build_body(&r, &caps(), None, &mut w);
        let spec = &body["toolConfig"]["tools"][0]["toolSpec"];
        assert_eq!(spec["name"], "shell");
        assert_eq!(spec["inputSchema"]["json"]["type"], "object");
    }
}
