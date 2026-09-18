use std::sync::Arc;

use async_stream::try_stream;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use zlogic_protocol::llm::{
    Effort, FinishReason, LlmError, LlmEvent, LlmRequest, MessageCache, ResponseFormat,
    ThinkingIntent, ThinkingMode,
};
use zlogic_protocol::message::{ContentPart, Message, Role};

use crate::cache::Breakpoints;
use crate::chat::effort_budget;
use crate::emitter::{PartEmitter, ReasoningRawSpec};
use crate::transport::HttpTransport;
use crate::usage_map;
use crate::{Endpoint, EventStream, LlmClient, error};

const API_VERSION: &str = "2023-06-01";

const BETA_FINE_GRAINED_TOOL_STREAMING: &str = "fine-grained-tool-streaming-2025-05-14";
const BETA_INTERLEAVED_THINKING: &str = "interleaved-thinking-2025-05-14";
const BETA_EXTENDED_CACHE_TTL: &str = "extended-cache-ttl-2025-04-11";
const FALLBACK_MAX_TOKENS: u64 = 8192;
const MIN_THINKING_BUDGET: u32 = 1024;

pub struct AnthropicClient {
    endpoint: Arc<Endpoint>,
    transport: Arc<dyn HttpTransport>,
    max_output_tokens: Option<u64>,
}

impl AnthropicClient {
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
impl LlmClient for AnthropicClient {
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

        let mut http = self
            .endpoint
            .request("v1/messages", payload)
            .header("anthropic-version", API_VERSION);
        if let Some(betas) = beta_header(&self.endpoint.capabilities, &req.cache) {
            http = http.header("anthropic-beta", betas);
        }
        http = self
            .endpoint
            .authorize(http, crate::AuthHeader::Raw("x-api-key"));
        for (k, v) in &self.endpoint.extra_headers {
            http = http.header(k.clone(), v.clone());
        }

        let bytes = self
            .transport
            .post_stream(http)
            .await
            .map_err(|e| crate::error::attach_endpoint(e, &self.endpoint, "v1/messages"))?;
        let endpoint = self.endpoint.clone();
        Ok(Box::pin(drive(bytes, warnings).map(move |item| {
            item.map_err(|e| crate::error::attach_endpoint(e, &endpoint, "v1/messages"))
        })))
    }
}

fn beta_header(
    caps: &zlogic_protocol::config::ModelCapabilities,
    cache: &zlogic_protocol::llm::CacheSpec,
) -> Option<String> {
    let mut betas = vec![BETA_FINE_GRAINED_TOOL_STREAMING];
    if caps.thinking.supported {
        betas.push(BETA_INTERLEAVED_THINKING);
    }
    if cache.wants_long_ttl() {
        betas.push(BETA_EXTENDED_CACHE_TTL);
    }
    Some(betas.join(","))
}

pub(crate) fn build_body(
    req: &LlmRequest,
    caps: &zlogic_protocol::config::ModelCapabilities,
    max_output_tokens: Option<u64>,
    warnings: &mut Vec<String>,
) -> Map<String, Value> {
    let mut body = Map::new();
    for (k, v) in &req.params {
        body.insert(k.clone(), v.clone());
    }

    let mut bp = Breakpoints::new(&req.cache);
    let tools = build_tools(req, &mut bp);

    let thinking = map_thinking(&req.thinking, &caps.thinking, warnings);
    if let Some(t) = &thinking.thinking {
        body.insert("thinking".into(), t.clone());
    }

    let budget = thinking
        .thinking
        .as_ref()
        .and_then(|t| t.get("budget_tokens"))
        .and_then(|b| b.as_u64())
        .unwrap_or(0);
    let want = body
        .get("max_tokens")
        .and_then(|m| m.as_u64())
        .or(max_output_tokens)
        .unwrap_or(FALLBACK_MAX_TOKENS);
    let max_tokens = want.max(budget + 1024);
    body.insert("max_tokens".into(), json!(max_tokens));

    if !req.system.is_empty() {
        let explicit = req.system.iter().any(|p| p.cache);
        let fill_last = req.cache.system && !explicit;
        let last = req.system.len() - 1;
        let blocks: Vec<Value> = req
            .system
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let mut b = json!({ "type": "text", "text": p.text });
                if (p.cache || (fill_last && i == last)) && bp.take() {
                    b["cache_control"] = cache_control(&bp);
                }
                b
            })
            .collect();
        body.insert("system".into(), json!(blocks));
    }

    let mut messages = build_messages(req, caps, warnings);
    if req.cache.messages == MessageCache::LatestUser {
        mark_latest_user(&mut messages, &mut bp);
    }
    body.insert("messages".into(), json!(messages));
    body.insert("model".into(), json!(req.model));
    body.insert("stream".into(), json!(true));

    if bp.dropped() > 0 {
        warnings.push(format!(
            "dropped {} cache breakpoint(s); anthropic allows at most {}",
            bp.dropped(),
            crate::cache::BREAKPOINT_CAP
        ));
    }

    if let Some(tools) = tools {
        body.insert("tools".into(), json!(tools));
    }

    let mut output_config = Map::new();
    if let Some(effort) = thinking.effort {
        output_config.insert("effort".into(), json!(effort));
    }
    if let Some(format) = response_format(req.response_format.as_ref(), warnings) {
        output_config.insert("format".into(), format);
    }
    if !output_config.is_empty() {
        body.insert("output_config".into(), Value::Object(output_config));
    }

    body
}

fn response_format(rf: Option<&ResponseFormat>, warnings: &mut Vec<String>) -> Option<Value> {
    match rf? {
        ResponseFormat::JsonSchema { schema, .. } => Some(json!({
            "type": "json_schema",
            "schema": crate::tool_schema::anthropic_output(schema),
        })),
        ResponseFormat::Json => {
            warnings.push(
                "anthropic structured output requires a schema; \
                 schemaless ResponseFormat::Json was ignored"
                    .into(),
            );
            None
        }
    }
}

fn cache_control(bp: &Breakpoints) -> Value {
    match bp.ttl() {
        Some(ttl) => json!({ "type": "ephemeral", "ttl": ttl }),
        None => json!({ "type": "ephemeral" }),
    }
}

fn build_tools(req: &LlmRequest, bp: &mut Breakpoints) -> Option<Vec<Value>> {
    if req.tools.is_empty() {
        return None;
    }
    let last = req.tools.len() - 1;
    let mark_last = req.cache.tools;
    Some(
        req.tools
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let mut v = json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.parameters,
                });
                if mark_last && i == last && bp.take() {
                    v["cache_control"] = cache_control(bp);
                }
                v
            })
            .collect(),
    )
}

fn mark_latest_user(messages: &mut [Value], bp: &mut Breakpoints) {
    let Some(msg) = messages
        .iter_mut()
        .rev()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
    else {
        return;
    };
    let Some(last_block) = msg
        .get_mut("content")
        .and_then(|c| c.as_array_mut())
        .and_then(|a| a.last_mut())
    else {
        return;
    };
    if last_block.get("cache_control").is_some() || !bp.take() {
        return;
    }
    last_block["cache_control"] = cache_control(bp);
}

#[derive(Default)]
pub(crate) struct ThinkingWire {
    pub thinking: Option<Value>,
    pub effort: Option<&'static str>,
}

fn map_thinking(
    intent: &ThinkingIntent,
    caps: &zlogic_protocol::config::ThinkingCapability,
    warnings: &mut Vec<String>,
) -> ThinkingWire {
    if !caps.supported {
        if intent.mode == ThinkingMode::On {
            warnings.push("thinking requested but the model does not support it".into());
        }
        return ThinkingWire::default();
    }
    let adaptive = !(caps.budget && caps.efforts.is_empty());

    match intent.mode {
        ThinkingMode::Default => ThinkingWire::default(),

        ThinkingMode::Off => {
            if caps.can_disable {
                ThinkingWire {
                    thinking: Some(json!({ "type": "disabled" })),
                    effort: None,
                }
            } else {
                ThinkingWire::default()
            }
        }

        ThinkingMode::On => {
            if adaptive {
                ThinkingWire {
                    thinking: Some(json!({ "type": "adaptive", "display": "summarized" })),
                    effort: crate::chat::effort_for(intent, &caps.efforts).map(Effort::as_str),
                }
            } else {
                let budget = effort_budget(intent)
                    .unwrap_or(8192)
                    .max(MIN_THINKING_BUDGET);
                ThinkingWire {
                    thinking: Some(json!({ "type": "enabled", "budget_tokens": budget })),
                    effort: None,
                }
            }
        }
    }
}

fn build_messages(
    req: &LlmRequest,
    caps: &zlogic_protocol::config::ModelCapabilities,
    warnings: &mut Vec<String>,
) -> Vec<Value> {
    let mut out: Vec<(&'static str, Vec<Value>, usize)> = Vec::new();

    let mut push = |role: &'static str, blocks: Vec<Value>, front: bool| {
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
            Role::System => {
                let text = collect_text(msg);
                if !text.is_empty() {
                    push("user", vec![json!({ "type": "text", "text": text })], false);
                }
            }
            Role::User => push("user", user_blocks(msg, caps, warnings), false),
            Role::Assistant => {
                let blocks = assistant_blocks(msg, warnings);
                if !blocks.is_empty() {
                    push("assistant", blocks, false);
                }
            }
            Role::Tool => {
                for part in &msg.content {
                    if let ContentPart::ToolResult(r) = part {
                        let block = json!({
                            "type": "tool_result",
                            "tool_use_id": sanitize_tool_id(&r.call_id),
                            "content": r.content,
                            "is_error": r.is_error,
                        });
                        push("user", vec![block], true);
                    }
                }
            }
        }
    }

    out.into_iter()
        .map(|(role, content, _)| json!({ "role": role, "content": content }))
        .collect()
}

fn collect_text(msg: &Message) -> String {
    msg.content
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn user_blocks(
    msg: &Message,
    caps: &zlogic_protocol::config::ModelCapabilities,
    warnings: &mut Vec<String>,
) -> Vec<Value> {
    msg.content
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text(t) => Some(json!({ "type": "text", "text": t.text })),
            ContentPart::Image(img) => Some(match crate::media::resolve(img, caps, warnings) {
                crate::media::Resolved::Image { mime, base64 } => json!({
                    "type": "image",
                    "source": { "type": "base64", "media_type": mime, "data": base64 }
                }),
                crate::media::Resolved::Placeholder(text) => {
                    json!({ "type": "text", "text": text })
                }
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
                Some(obj) if obj.contains_key("type") => {
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
                    blocks.push(json!({ "type": "text", "text": t.text }));
                }
            }
            ContentPart::ToolCall(tc) => {
                for c in &tc.calls {
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": sanitize_tool_id(&c.id),
                        "name": c.name,
                        "input": parse_args(&c.args),
                    }));
                }
            }
            _ => {}
        }
    }
    blocks
}

pub(crate) fn parse_args(args: &str) -> Value {
    serde_json::from_str(args).unwrap_or_else(|_| json!({}))
}

pub(crate) fn sanitize_tool_id(id: &str) -> String {
    if id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return id.to_string();
    }
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
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

        if let Some(u) = st.take_usage() {
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
    block: Option<BlockKind>,
    thinking_text: String,
    signature: String,
    next_call_index: u32,
    current_call_index: u32,
    usage: Map<String, Value>,
    usage_seen: bool,
    finish: FinishReason,
}

enum BlockKind {
    Thinking,
    Text,
    ToolUse,
}

impl State {
    fn take_usage(&mut self) -> Option<zlogic_protocol::usage::UsageReport> {
        if !self.usage_seen {
            return None;
        }
        self.usage_seen = false;
        let raw = Value::Object(std::mem::take(&mut self.usage));
        Some(usage_map::extract(
            &raw,
            &usage_map::ANTHROPIC,
            &Default::default(),
        ))
    }

    fn merge_usage(&mut self, v: &Value) {
        if let Some(obj) = v.as_object() {
            for (k, val) in obj {
                self.usage.insert(k.clone(), val.clone());
            }
            self.usage_seen = true;
        }
    }
}

fn handle(data: &str, em: &mut PartEmitter, st: &mut State) -> Result<(), LlmError> {
    let Ok(v) = serde_json::from_str::<Value>(data) else {
        em.warn(format!("unparseable SSE data chunk ({} bytes)", data.len()));
        return Ok(());
    };

    match v.get("type").and_then(|t| t.as_str()).unwrap_or("") {
        "error" => {
            let msg = v
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or(data);
            return Err(error::classify_body(error::protocol(msg.to_string()), data));
        }
        "message_start" => {
            if let Some(u) = v.get("message").and_then(|m| m.get("usage")) {
                st.merge_usage(u);
            }
        }
        "content_block_start" => {
            let cb = v.get("content_block").cloned().unwrap_or(Value::Null);
            match cb.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                "thinking" => {
                    st.block = Some(BlockKind::Thinking);
                    st.thinking_text.clear();
                    st.signature.clear();
                    em.open_reasoning();
                }
                "redacted_thinking" => {
                    st.block = None;
                    em.open_reasoning();
                    if let Some(d) = cb.get("data") {
                        em.set_reasoning_raw(
                            json!({ "type": "redacted_thinking", "data": d.clone() }),
                        );
                    }
                }
                "text" => st.block = Some(BlockKind::Text),
                "tool_use" => {
                    let name = cb.get("name").and_then(|n| n.as_str()).unwrap_or("");
                    st.block = Some(BlockKind::ToolUse);
                    st.current_call_index = st.next_call_index;
                    st.next_call_index += 1;
                    if let Some(id) = cb.get("id").and_then(|i| i.as_str()) {
                        em.tool_call_id(st.current_call_index, id);
                    }
                    if !name.is_empty() {
                        em.tool_call_name(st.current_call_index, name);
                    }
                }
                _ => st.block = None,
            }
        }
        "content_block_delta" => {
            let d = v.get("delta").cloned().unwrap_or(Value::Null);
            match d.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                "thinking_delta" => {
                    if let Some(t) = d.get("thinking").and_then(|x| x.as_str()) {
                        st.thinking_text.push_str(t);
                        em.reasoning_delta(t);
                    }
                }
                "signature_delta" => {
                    if let Some(s) = d.get("signature").and_then(|x| x.as_str()) {
                        st.signature.push_str(s);
                    }
                }
                "text_delta" => {
                    if let Some(t) = d.get("text").and_then(|x| x.as_str()) {
                        em.text_delta(t);
                    }
                }
                "input_json_delta" => {
                    if let Some(p) = d.get("partial_json").and_then(|x| x.as_str()) {
                        em.tool_call_args(st.current_call_index, p);
                    }
                }
                _ => {}
            }
        }
        "content_block_stop" => {
            if matches!(st.block, Some(BlockKind::Thinking)) {
                if !st.signature.is_empty() {
                    em.set_reasoning_raw(json!({
                        "type": "thinking",
                        "thinking": st.thinking_text,
                        "signature": st.signature,
                    }));
                } else {
                    em.warn("thinking block closed without a signature; raw dropped");
                }
            }
            st.block = None;
        }
        "message_delta" => {
            if let Some(sr) = v
                .get("delta")
                .and_then(|d| d.get("stop_reason"))
                .and_then(|s| s.as_str())
            {
                st.finish = map_stop_reason(sr);
            }
            if let Some(u) = v.get("usage") {
                st.merge_usage(u);
            }
        }
        _ => {}
    }

    Ok(())
}

fn map_stop_reason(s: &str) -> FinishReason {
    match s {
        "end_turn" | "stop_sequence" | "pause_turn" => FinishReason::Stop,
        "tool_use" => FinishReason::ToolCalls,
        "max_tokens" => FinishReason::Length,
        "refusal" => FinishReason::ContentFilter,
        _ => FinishReason::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zlogic_protocol::config::{ModelCapabilities, ThinkingCapability};
    use zlogic_protocol::llm::RequestMeta;
    use zlogic_protocol::message::{
        ReasoningPart, Source, TextPart, ToolCall, ToolCallPart, ToolResultPart,
    };
    use zlogic_protocol::usage::Purpose;

    fn req(messages: Vec<Message>) -> LlmRequest {
        LlmRequest {
            model: "claude-opus-5".into(),
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
    fn tool_results_merge_into_one_user_message_at_the_front() {
        let r = req(vec![
            Message::assistant(
                Source::new("anthropic", "claude-opus-5"),
                vec![ContentPart::ToolCall(ToolCallPart {
                    calls: vec![
                        ToolCall {
                            id: "toolu_1".into(),
                            name: "a".into(),
                            args: "{}".into(),
                            raw: None,
                        },
                        ToolCall {
                            id: "toolu_2".into(),
                            name: "b".into(),
                            args: "{}".into(),
                            raw: None,
                        },
                    ],
                })],
            ),
            Message::tool(vec![ContentPart::ToolResult(ToolResultPart {
                files: Vec::new(),
                call_id: "toolu_1".into(),
                name: "a".into(),
                content: "r1".into(),
                is_error: false,
            })]),
            Message::tool(vec![ContentPart::ToolResult(ToolResultPart {
                files: Vec::new(),
                call_id: "toolu_2".into(),
                name: "b".into(),
                content: "r2".into(),
                is_error: true,
            })]),
            Message::user(vec![ContentPart::Text(TextPart {
                text: "by the way".into(),
                raw: None,
                truncated: false,
            })]),
        ]);
        let mut w = Vec::new();
        let body = build_body(&r, &caps(), None, &mut w);
        let msgs = body["messages"].as_array().unwrap();

        assert_eq!(msgs.len(), 2, "assistant + one merged user");
        let content = msgs[1]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "tool_result");
        assert_eq!(content[0]["tool_use_id"], "toolu_1");
        assert_eq!(
            content[1]["tool_use_id"], "toolu_2",
            "order within the group is kept"
        );
        assert_eq!(content[1]["is_error"], true);
        assert_eq!(
            content[2]["type"], "text",
            "injected text must come after the results"
        );
    }

    #[test]
    fn thinking_block_is_replayed_verbatim_before_tool_use() {
        let raw = json!({ "type": "thinking", "thinking": "Let me…", "signature": "EqoBCk" });
        let r = req(vec![Message::assistant(
            Source::new("anthropic", "claude-opus-5"),
            vec![
                ContentPart::Reasoning(ReasoningPart {
                    text: "Let me…".into(),
                    raw: Some(raw.clone()),
                    truncated: false,
                }),
                ContentPart::ToolCall(ToolCallPart {
                    calls: vec![ToolCall {
                        id: "toolu_1".into(),
                        name: "x".into(),
                        args: "{\"b\":1,\"a\":2}".into(),
                        raw: None,
                    }],
                }),
            ],
        )]);
        let mut w = Vec::new();
        let body = build_body(&r, &caps(), None, &mut w);
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content[0], raw, "must be byte-for-byte identical");
        assert_eq!(content[1]["type"], "tool_use");
        assert_eq!(
            serde_json::to_string(&content[1]["input"]).unwrap(),
            "{\"b\":1,\"a\":2}"
        );
    }

    #[test]
    fn unrecognised_reasoning_raw_is_dropped() {
        let r = req(vec![Message::assistant(
            Source::new("anthropic", "claude-opus-5"),
            vec![ContentPart::Reasoning(ReasoningPart {
                text: "x".into(),
                raw: Some(json!({ "carrier": "reasoning_content", "value": "x" })),
                truncated: false,
            })],
        )]);
        let mut w = Vec::new();
        let body = build_body(&r, &caps(), None, &mut w);
        let msgs = body["messages"].as_array().unwrap();
        assert!(msgs.is_empty() || msgs[0]["content"].as_array().unwrap().is_empty());
        assert!(!w.is_empty());
    }

    #[test]
    fn max_tokens_always_present_and_exceeds_budget() {
        let mut r = req(vec![]);
        r.thinking = ThinkingIntent {
            mode: ThinkingMode::On,
            effort: None,
            budget_tokens: Some(30000),
        };
        let mut w = Vec::new();
        let body = build_body(&r, &caps(), Some(8192), &mut w);
        assert_eq!(body["thinking"]["budget_tokens"], 30000);
        assert!(
            body["max_tokens"].as_u64().unwrap() > 30000,
            "max_tokens must exceed the thinking budget"
        );
    }

    #[test]
    fn thinking_budget_floors_at_the_official_minimum() {
        let mut r = req(vec![]);
        r.thinking = ThinkingIntent {
            mode: ThinkingMode::On,
            effort: None,
            budget_tokens: Some(10),
        };
        let mut w = Vec::new();
        let body = build_body(&r, &caps(), None, &mut w);
        assert_eq!(body["thinking"]["budget_tokens"], MIN_THINKING_BUDGET);
    }

    #[test]
    fn consecutive_same_role_messages_are_merged() {
        let r = req(vec![
            Message::user(vec![ContentPart::Text(TextPart {
                text: "a".into(),
                raw: None,
                truncated: false,
            })]),
            Message::user(vec![ContentPart::Text(TextPart {
                text: "b".into(),
                raw: None,
                truncated: false,
            })]),
        ]);
        let mut w = Vec::new();
        let body = build_body(&r, &caps(), None, &mut w);
        assert_eq!(
            body["messages"].as_array().unwrap().len(),
            1,
            "roles must alternate"
        );
    }

    // ───────────────────────── prompt caching ─────────────────────────

    fn tool_def(name: &str) -> zlogic_protocol::llm::ToolDefinition {
        zlogic_protocol::llm::ToolDefinition {
            name: name.into(),
            description: "d".into(),
            parameters: json!({ "type": "object" }),
        }
    }

    #[test]
    fn default_policy_places_three_breakpoints_at_the_stable_boundaries() {
        use zlogic_protocol::llm::SystemPart;
        let mut r = req(vec![Message::user(vec![ContentPart::Text(TextPart {
            text: "question".into(),
            raw: None,
            truncated: false,
        })])]);
        r.system = vec![
            SystemPart {
                text: "a".into(),
                cache: false,
            },
            SystemPart {
                text: "b".into(),
                cache: false,
            },
        ];
        r.tools = vec![tool_def("t1"), tool_def("t2")];

        let mut w = Vec::new();
        let body = build_body(&r, &caps(), None, &mut w);

        let tools = body["tools"].as_array().unwrap();
        assert!(tools[0].get("cache_control").is_none());
        assert_eq!(tools[1]["cache_control"], json!({ "type": "ephemeral" }));

        assert!(body["system"][0].get("cache_control").is_none());
        assert_eq!(
            body["system"][1]["cache_control"],
            json!({ "type": "ephemeral" })
        );

        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(
            content.last().unwrap()["cache_control"],
            json!({ "type": "ephemeral" })
        );

        assert!(
            w.is_empty(),
            "3 breakpoints fit the budget, so there should be no warning"
        );
    }

    #[test]
    fn cache_off_emits_no_markers_at_all() {
        use zlogic_protocol::llm::{CacheSpec, SystemPart};
        let mut r = req(vec![Message::user(vec![ContentPart::Text(TextPart {
            text: "question".into(),
            raw: None,
            truncated: false,
        })])]);
        r.cache = CacheSpec::off();
        r.system = vec![SystemPart {
            text: "a".into(),
            cache: false,
        }];
        r.tools = vec![tool_def("t1")];

        let mut w = Vec::new();
        let body = build_body(&r, &caps(), None, &mut w);
        assert!(
            !serde_json::to_string(&body)
                .unwrap()
                .contains("cache_control")
        );
    }

    #[test]
    fn the_breakpoint_lands_on_the_latest_user_message_after_tool_results_merge() {
        let r = req(vec![
            Message::user(vec![ContentPart::Text(TextPart {
                text: "first round".into(),
                raw: None,
                truncated: false,
            })]),
            Message::assistant(
                Source::new("anthropic", "claude-opus-5"),
                vec![ContentPart::ToolCall(ToolCallPart {
                    calls: vec![ToolCall {
                        id: "toolu_1".into(),
                        name: "a".into(),
                        args: "{}".into(),
                        raw: None,
                    }],
                })],
            ),
            Message::tool(vec![ContentPart::ToolResult(ToolResultPart {
                files: Vec::new(),
                call_id: "toolu_1".into(),
                name: "a".into(),
                content: "r1".into(),
                is_error: false,
            })]),
        ]);
        let mut w = Vec::new();
        let body = build_body(&r, &caps(), None, &mut w);
        let msgs = body["messages"].as_array().unwrap();

        assert!(
            msgs[0]["content"][0].get("cache_control").is_none(),
            "only the most recent user is marked"
        );
        let last = msgs.last().unwrap();
        assert_eq!(last["role"], "user");
        let blocks = last["content"].as_array().unwrap();
        assert_eq!(
            blocks.last().unwrap()["cache_control"],
            json!({ "type": "ephemeral" })
        );
    }

    #[test]
    fn breakpoints_beyond_the_cap_are_dropped_with_a_warning() {
        use zlogic_protocol::llm::SystemPart;
        let mut r = req(vec![Message::user(vec![ContentPart::Text(TextPart {
            text: "question".into(),
            raw: None,
            truncated: false,
        })])]);
        r.system = (0..5)
            .map(|i| SystemPart {
                text: format!("s{i}"),
                cache: true,
            })
            .collect();
        r.tools = vec![tool_def("t1")];

        let mut w = Vec::new();
        let body = build_body(&r, &caps(), None, &mut w);

        let marked = serde_json::to_string(&body)
            .unwrap()
            .matches("cache_control")
            .count();
        assert_eq!(
            marked,
            crate::cache::BREAKPOINT_CAP as usize,
            "the cap is hard"
        );
        assert!(
            w.iter().any(|m| m.contains("cache breakpoint")),
            "silently caching less is invisible on the bill, so it must be recorded: {w:?}"
        );
    }

    #[tokio::test]
    async fn the_long_ttl_bucket_comes_with_its_beta_header() {
        use crate::transport::RecordingTransport;
        use zlogic_protocol::llm::{CacheSpec, SystemPart};

        let mut r = req(vec![]);
        r.cache = CacheSpec {
            ttl_seconds: Some(3600),
            ..Default::default()
        };
        r.system = vec![SystemPart {
            text: "big preamble".into(),
            cache: false,
        }];

        let t = Arc::new(RecordingTransport::new());
        let client = AnthropicClient::new(
            Endpoint::new("https://api.test").with_capabilities(caps()),
            t.clone(),
            None,
        );
        let _ = client.stream(r).await;
        let sent = t.last().expect("a request was sent");

        let body = String::from_utf8_lossy(&sent.body);
        assert!(body.contains(r#""ttl":"1h""#), "body: {body}");
        let beta = sent
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("anthropic-beta"))
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        assert!(
            beta.contains(BETA_EXTENDED_CACHE_TTL),
            "a request carrying a ttl but no beta header is a 400: {beta}"
        );
    }

    #[tokio::test]
    async fn the_default_ttl_stays_implicit() {
        use crate::transport::RecordingTransport;
        let t = Arc::new(RecordingTransport::new());
        let client = AnthropicClient::new(
            Endpoint::new("https://api.test").with_capabilities(caps()),
            t.clone(),
            None,
        );
        let _ = client.stream(req(vec![])).await;
        let sent = t.last().expect("a request was sent");
        assert!(!String::from_utf8_lossy(&sent.body).contains("ttl"));
        let beta = sent
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("anthropic-beta"))
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        assert!(!beta.contains(BETA_EXTENDED_CACHE_TTL));
    }

    #[test]
    fn system_uses_cache_control_when_requested() {
        use zlogic_protocol::llm::SystemPart;
        let mut r = req(vec![]);
        r.system = vec![
            SystemPart {
                text: "big preamble".into(),
                cache: true,
            },
            SystemPart {
                text: "tail".into(),
                cache: false,
            },
        ];
        let mut w = Vec::new();
        let body = build_body(&r, &caps(), None, &mut w);
        assert_eq!(
            body["system"][0]["cache_control"],
            json!({ "type": "ephemeral" })
        );
        assert!(body["system"][1].get("cache_control").is_none());
    }

    #[test]
    fn tool_ids_are_sanitized_identically_on_both_sides() {
        let dirty = "call:abc.123/xyz";
        let r = req(vec![
            Message::assistant(
                Source::new("anthropic", "claude-opus-5"),
                vec![ContentPart::ToolCall(ToolCallPart {
                    calls: vec![ToolCall {
                        id: dirty.into(),
                        name: "a".into(),
                        args: "{}".into(),
                        raw: None,
                    }],
                })],
            ),
            Message::tool(vec![ContentPart::ToolResult(ToolResultPart {
                files: Vec::new(),
                call_id: dirty.into(),
                name: "a".into(),
                content: "r".into(),
                is_error: false,
            })]),
        ]);
        let mut w = Vec::new();
        let body = build_body(&r, &caps(), None, &mut w);

        let use_id = body["messages"][0]["content"][0]["id"].as_str().unwrap();
        let result_id = body["messages"][1]["content"][0]["tool_use_id"]
            .as_str()
            .unwrap();
        assert_eq!(use_id, "call_abc_123_xyz");
        assert_eq!(use_id, result_id, "both sides must match, or pairing fails");
    }

    #[test]
    fn legal_tool_ids_pass_through_untouched() {
        assert_eq!(sanitize_tool_id("toolu_01A2b-3C"), "toolu_01A2b-3C");
    }

    #[tokio::test]
    async fn beta_header_is_derived_from_capabilities() {
        use crate::transport::RecordingTransport;
        use zlogic_protocol::config::ThinkingCapability;

        async fn headers_for(caps: ModelCapabilities) -> Vec<(String, String)> {
            let t = Arc::new(RecordingTransport::new());
            let client = AnthropicClient::new(
                Endpoint::new("https://api.test").with_capabilities(caps),
                t.clone(),
                None,
            );
            let _ = client.stream(req(vec![])).await;
            t.last().expect("a request was sent").headers
        }

        let beta = |h: &[(String, String)]| {
            h.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("anthropic-beta"))
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };

        let thinking = beta(&headers_for(caps()).await);
        assert!(thinking.contains(BETA_FINE_GRAINED_TOOL_STREAMING));
        assert!(thinking.contains(BETA_INTERLEAVED_THINKING));

        let no_thinking = beta(
            &headers_for(ModelCapabilities {
                vision: Some(true),
                thinking: ThinkingCapability::default(),
            })
            .await,
        );
        assert!(no_thinking.contains(BETA_FINE_GRAINED_TOOL_STREAMING));
        assert!(
            !no_thinking.contains(BETA_INTERLEAVED_THINKING),
            "a model without thinking support must not receive the interleaved beta"
        );
    }

    #[test]
    fn real_tools_are_never_forced() {
        use zlogic_protocol::llm::ToolDefinition;
        let mut r = req(vec![]);
        r.tools = vec![ToolDefinition {
            name: "t".into(),
            description: "d".into(),
            parameters: json!({ "type": "object" }),
        }];
        let mut w = Vec::new();
        let body = build_body(&r, &caps(), None, &mut w);
        assert!(body.contains_key("tools"));
        assert!(
            !body.contains_key("tool_choice"),
            "real tools are always left to the model to decide"
        );
    }

    // ─────────────────── adaptive thinking ───────────────────

    fn adaptive_caps(efforts: Vec<Effort>, can_disable: bool) -> ModelCapabilities {
        ModelCapabilities {
            vision: Some(true),
            thinking: ThinkingCapability {
                supported: true,
                can_disable,
                efforts,
                budget: false,
            },
        }
    }

    #[test]
    fn declared_efforts_select_the_adaptive_dialect() {
        let mut r = req(vec![]);
        r.thinking = ThinkingIntent {
            mode: ThinkingMode::On,
            effort: Some(Effort::XHigh),
            budget_tokens: None,
        };
        let caps = adaptive_caps(
            vec![Effort::Low, Effort::High, Effort::XHigh, Effort::Max],
            false,
        );

        let mut w = Vec::new();
        let body = build_body(&r, &caps, None, &mut w);

        assert_eq!(body["thinking"]["type"], "adaptive");
        assert!(
            body["thinking"].get("budget_tokens").is_none(),
            "on 4.7+ a budget_tokens always means a 400"
        );
        assert_eq!(body["output_config"]["effort"], "xhigh");
    }

    #[test]
    fn adaptive_always_asks_for_summarized_thinking() {
        let mut r = req(vec![]);
        r.thinking = ThinkingIntent {
            mode: ThinkingMode::On,
            effort: None,
            budget_tokens: None,
        };
        let mut w = Vec::new();
        let body = build_body(&r, &adaptive_caps(vec![Effort::High], false), None, &mut w);
        assert_eq!(body["thinking"]["display"], "summarized");
    }

    #[test]
    fn budget_only_capability_keeps_the_extended_dialect() {
        let mut r = req(vec![]);
        r.thinking = ThinkingIntent {
            mode: ThinkingMode::On,
            effort: None,
            budget_tokens: Some(20_000),
        };
        let mut w = Vec::new();
        let body = build_body(&r, &caps(), None, &mut w);

        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 20_000);
        assert!(
            body.get("output_config").is_none(),
            "the older dialect has no effort"
        );
    }

    #[test]
    fn effort_is_clamped_to_the_declared_ladder() {
        let mut r = req(vec![]);
        r.thinking = ThinkingIntent {
            mode: ThinkingMode::On,
            effort: Some(Effort::Max),
            budget_tokens: None,
        };
        let mut w = Vec::new();
        let body = build_body(
            &r,
            &adaptive_caps(vec![Effort::Low, Effort::Medium], false),
            None,
            &mut w,
        );
        assert_eq!(body["output_config"]["effort"], "medium");
    }

    #[test]
    fn disabling_thinking_sends_no_effort() {
        let mut r = req(vec![]);
        r.thinking = ThinkingIntent {
            mode: ThinkingMode::Off,
            effort: Some(Effort::Max),
            budget_tokens: None,
        };
        let mut w = Vec::new();
        let body = build_body(
            &r,
            &adaptive_caps(vec![Effort::High, Effort::Max], true),
            None,
            &mut w,
        );

        assert_eq!(body["thinking"]["type"], "disabled");
        assert!(
            body.get("output_config").is_none(),
            "with thinking off the tier is meaningless, and it would 400"
        );
    }

    #[test]
    fn undisableable_model_off_omits() {
        let mut r = req(vec![]);
        r.thinking = ThinkingIntent {
            mode: ThinkingMode::Off,
            effort: None,
            budget_tokens: None,
        };
        let mut w = Vec::new();
        let body = build_body(
            &r,
            &adaptive_caps(vec![Effort::Medium, Effort::Max], false),
            None,
            &mut w,
        );

        assert!(
            body.get("thinking").is_none(),
            "the thinking field must not be sent when it cannot be fully disabled"
        );
        assert!(w.is_empty(), "no flooring any more, so no warning either");
    }

    #[test]
    fn response_format_uses_the_native_field() {
        let mut r = req(vec![]);
        r.response_format = Some(ResponseFormat::JsonSchema {
            name: "verdict".into(),
            schema: json!({ "type": "object", "properties": { "ok": { "type": "boolean" } } }),
            strict: true,
        });
        let mut w = Vec::new();
        let body = build_body(&r, &caps(), None, &mut w);

        let f = &body["output_config"]["format"];
        assert_eq!(f["type"], "json_schema");
        assert_eq!(f["schema"]["properties"]["ok"]["type"], "boolean");
        assert_eq!(f["schema"]["additionalProperties"], false);
        assert!(body.get("tools").is_none());
        assert!(!body.contains_key("tool_choice"));
    }

    #[test]
    fn effort_and_format_share_output_config() {
        let mut r = req(vec![]);
        r.thinking = ThinkingIntent {
            mode: ThinkingMode::On,
            effort: Some(Effort::High),
            budget_tokens: None,
        };
        r.response_format = Some(ResponseFormat::JsonSchema {
            name: "v".into(),
            schema: json!({ "type": "object" }),
            strict: true,
        });
        let mut w = Vec::new();
        let body = build_body(&r, &adaptive_caps(vec![Effort::High], false), None, &mut w);

        assert_eq!(body["output_config"]["effort"], "high");
        assert_eq!(body["output_config"]["format"]["type"], "json_schema");
    }

    #[test]
    fn schemaless_json_is_refused_not_faked() {
        let mut r = req(vec![]);
        r.response_format = Some(ResponseFormat::Json);
        let mut w = Vec::new();
        let body = build_body(&r, &caps(), None, &mut w);

        assert!(body.get("output_config").is_none());
        assert!(w.iter().any(|m| m.contains("requires a schema")), "{w:?}");
    }
}
