use std::sync::Arc;

use async_stream::try_stream;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use zlogic_protocol::config::{ModelCapabilities, ThinkingCapability};
use zlogic_protocol::llm::{
    FinishReason, LlmError, LlmEvent, LlmRequest, ResponseFormat, ThinkingIntent, ThinkingMode,
};
use zlogic_protocol::message::{ContentPart, Message, Role};

use crate::anthropic::parse_args;
use crate::chat::{effort_budget, effort_for};
use crate::emitter::{PartEmitter, ReasoningRawSpec};
use crate::transport::HttpTransport;
use crate::usage_map;
use crate::{Endpoint, EventStream, LlmClient, error};

pub struct GeminiClient {
    endpoint: Arc<Endpoint>,
    transport: Arc<dyn HttpTransport>,
    max_output_tokens: Option<u64>,
}

impl GeminiClient {
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
impl LlmClient for GeminiClient {
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

        let path = format!("v1beta/models/{}:streamGenerateContent?alt=sse", req.model);
        let mut http = self.endpoint.request(&path, payload);
        http = self
            .endpoint
            .authorize(http, crate::AuthHeader::Raw("x-goog-api-key"));
        for (k, v) in &self.endpoint.extra_headers {
            http = http.header(k.clone(), v.clone());
        }

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

pub(crate) fn build_body(
    req: &LlmRequest,
    caps: &ModelCapabilities,
    max_output_tokens: Option<u64>,
    warnings: &mut Vec<String>,
) -> Map<String, Value> {
    let mut body = Map::new();

    let mut generation_config = Map::new();
    for (k, v) in &req.params {
        generation_config.insert(k.clone(), v.clone());
    }
    if let Some(m) = max_output_tokens
        && !generation_config.contains_key("maxOutputTokens")
    {
        generation_config.insert("maxOutputTokens".into(), json!(m));
    }
    if let Some(tc) = map_thinking(&req.thinking, &caps.thinking, warnings) {
        generation_config.insert("thinkingConfig".into(), tc);
    }
    match &req.response_format {
        Some(ResponseFormat::Json) => {
            generation_config.insert("responseMimeType".into(), json!("application/json"));
        }
        Some(ResponseFormat::JsonSchema { schema, .. }) => {
            generation_config.insert("responseMimeType".into(), json!("application/json"));
            generation_config.insert("responseSchema".into(), schema.clone());
        }
        None => {}
    }
    if !generation_config.is_empty() {
        body.insert("generationConfig".into(), Value::Object(generation_config));
    }

    if !req.system.is_empty() {
        let text = req
            .system
            .iter()
            .map(|p| p.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        body.insert(
            "systemInstruction".into(),
            json!({ "parts": [{ "text": text }] }),
        );
    }

    body.insert(
        "contents".into(),
        json!(build_contents(req, caps, warnings)),
    );

    if !req.tools.is_empty() {
        let decls: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                let mut decl = json!({ "name": t.name, "description": t.description });
                if let Some(params) = crate::tool_schema::gemini(&t.parameters) {
                    decl["parameters"] = params;
                }
                decl
            })
            .collect();
        body.insert("tools".into(), json!([{ "functionDeclarations": decls }]));
    }

    body
}

fn map_thinking(
    intent: &ThinkingIntent,
    caps: &ThinkingCapability,
    warnings: &mut Vec<String>,
) -> Option<Value> {
    match intent.mode {
        ThinkingMode::Default => None,
        ThinkingMode::On => {
            if !caps.supported {
                warnings.push("thinking requested but the model does not support it".into());
                return None;
            }
            let mut tc = Map::new();
            tc.insert("includeThoughts".into(), json!(true));
            if !caps.efforts.is_empty() {
                if let Some(level) = effort_for(intent, &caps.efforts) {
                    tc.insert("thinkingLevel".into(), json!(level.as_str()));
                }
            } else if caps.budget
                && let Some(b) = effort_budget(intent)
            {
                tc.insert("thinkingBudget".into(), json!(b));
            }
            Some(Value::Object(tc))
        }
        ThinkingMode::Off => {
            if !caps.supported {
                return None;
            }
            if caps.can_disable {
                Some(json!({ "thinkingBudget": 0, "includeThoughts": false }))
            } else {
                None
            }
        }
    }
}

fn build_contents(
    req: &LlmRequest,
    caps: &ModelCapabilities,
    warnings: &mut Vec<String>,
) -> Vec<Value> {
    let mut out: Vec<(&'static str, Vec<Value>)> = Vec::new();

    let mut push = |role: &'static str, parts: Vec<Value>| {
        if parts.is_empty() {
            return;
        }
        if let Some(last) = out.last_mut()
            && last.0 == role
        {
            last.1.extend(parts);
            return;
        }
        out.push((role, parts));
    };

    for msg in &req.messages {
        match msg.role {
            Role::System | Role::User => push("user", user_parts(msg, caps, warnings)),
            Role::Assistant => push("model", model_parts(msg)),
            Role::Tool => {
                let parts: Vec<Value> = msg
                    .content
                    .iter()
                    .filter_map(|p| match p {
                        ContentPart::ToolResult(r) => Some(json!({
                            "functionResponse": {
                                "name": r.name,
                                "response": if r.is_error {
                                    json!({ "error": r.content })
                                } else {
                                    json!({ "result": r.content })
                                },
                            }
                        })),
                        _ => None,
                    })
                    .collect();
                push("user", parts);
            }
        }
    }

    out.into_iter()
        .map(|(role, parts)| json!({ "role": role, "parts": parts }))
        .collect()
}

fn user_parts(msg: &Message, caps: &ModelCapabilities, warnings: &mut Vec<String>) -> Vec<Value> {
    msg.content
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text(t) => Some(json!({ "text": t.text })),
            ContentPart::Image(img) => Some(match crate::media::resolve(img, caps, warnings) {
                crate::media::Resolved::Image { mime, base64 } => json!({
                    "inlineData": { "mimeType": mime, "data": base64 }
                }),
                crate::media::Resolved::Placeholder(text) => json!({ "text": text }),
            }),
            _ => None,
        })
        .collect()
}

fn model_parts(msg: &Message) -> Vec<Value> {
    let mut parts = Vec::new();
    for part in &msg.content {
        match part {
            ContentPart::Reasoning(r) => {
                let mut p = json!({ "text": r.text, "thought": true });
                if let Some(sig) = signature_of(r.raw.as_ref()) {
                    p["thoughtSignature"] = json!(sig);
                }
                parts.push(p);
            }
            ContentPart::Text(t) => {
                let mut p = json!({ "text": t.text });
                if let Some(sig) = signature_of(t.raw.as_ref()) {
                    p["thoughtSignature"] = json!(sig);
                }
                parts.push(p);
            }
            ContentPart::ToolCall(tc) => {
                for c in &tc.calls {
                    let mut p = json!({
                        "functionCall": { "name": c.name, "args": parse_args(&c.args) }
                    });
                    if let Some(sig) = signature_of(c.raw.as_ref()) {
                        p["thoughtSignature"] = json!(sig);
                    }
                    parts.push(p);
                }
            }
            _ => {}
        }
    }
    parts
}

fn signature_of(raw: Option<&Value>) -> Option<String> {
    raw?.get("thoughtSignature")?.as_str().map(str::to_string)
}

fn map_finish_reason(s: &str) -> FinishReason {
    match s {
        "STOP" => FinishReason::Stop,
        "MAX_TOKENS" => FinishReason::Length,
        "SAFETY" | "IMAGE_SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" => {
            FinishReason::ContentFilter
        }
        _ => FinishReason::Other,
    }
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
        let mut next_call = 0u32;
        let mut finish = FinishReason::Stop;
        let mut usage: Option<zlogic_protocol::usage::UsageReport> = None;

        while let Some(ev) = sse.next().await {
            let ev = ev?;
            handle(&ev.data, &mut em, &mut next_call, &mut finish, &mut usage)?;
            for e in em.drain() {
                yield e;
            }
        }

        if let Some(u) = usage {
            em.usage(u);
        }
        if matches!(finish, FinishReason::Stop) && em.has_tool_calls() {
            finish = FinishReason::ToolCalls;
        }
        em.finish(finish);
        for e in em.drain() {
            yield e;
        }
    }
}

fn handle(
    data: &str,
    em: &mut PartEmitter,
    next_call: &mut u32,
    finish: &mut FinishReason,
    usage: &mut Option<zlogic_protocol::usage::UsageReport>,
) -> Result<(), LlmError> {
    let Ok(v) = serde_json::from_str::<Value>(data) else {
        em.warn(format!("unparseable SSE data chunk ({} bytes)", data.len()));
        return Ok(());
    };

    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or(data)
            .to_string();
        return Err(error::classify_body(error::protocol(msg), data));
    }

    if let Some(um) = v.get("usageMetadata") {
        *usage = Some(usage_map::extract(
            um,
            &usage_map::GEMINI,
            &Default::default(),
        ));
    }

    let Some(cand) = v.get("candidates").and_then(|c| c.get(0)) else {
        return Ok(());
    };

    if let Some(fr) = cand.get("finishReason").and_then(|f| f.as_str()) {
        *finish = map_finish_reason(fr);
        if fr == "MALFORMED_FUNCTION_CALL" {
            em.warn("gemini returned MALFORMED_FUNCTION_CALL");
        }
    }

    let Some(parts) = cand
        .get("content")
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.as_array())
    else {
        return Ok(());
    };

    for part in parts {
        let sig = part.get("thoughtSignature").and_then(|s| s.as_str());

        if let Some(fc) = part.get("functionCall") {
            let idx = *next_call;
            *next_call += 1;
            if let Some(name) = fc.get("name").and_then(|n| n.as_str()) {
                em.tool_call_id(idx, format!("tool_{idx}"));
                em.tool_call_name(idx, name);
            }
            let args = fc.get("args").cloned().unwrap_or_else(|| json!({}));
            em.tool_call_args(
                idx,
                &serde_json::to_string(&args).unwrap_or_else(|_| "{}".into()),
            );
            if let Some(s) = sig {
                em.tool_call_raw(idx, json!({ "thoughtSignature": s }));
            }
            continue;
        }

        let Some(text) = part.get("text").and_then(|t| t.as_str()) else {
            continue;
        };
        let is_thought = part
            .get("thought")
            .and_then(|t| t.as_bool())
            .unwrap_or(false);
        if is_thought {
            em.reasoning_delta(text);
            if let Some(s) = sig {
                em.set_reasoning_raw(json!({ "thoughtSignature": s }));
            }
        } else {
            em.text_delta(text);
            if let Some(s) = sig {
                em.set_text_raw(json!({ "thoughtSignature": s }));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zlogic_protocol::llm::RequestMeta;
    use zlogic_protocol::message::{
        ReasoningPart, Source, TextPart, ToolCall, ToolCallPart, ToolResultPart,
    };
    use zlogic_protocol::usage::Purpose;

    fn req(messages: Vec<Message>) -> LlmRequest {
        LlmRequest {
            model: "gemini-3-pro".into(),
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

    fn caps(efforts: Vec<zlogic_protocol::llm::Effort>, can_disable: bool) -> ModelCapabilities {
        ModelCapabilities {
            vision: Some(true),
            thinking: ThinkingCapability {
                supported: true,
                can_disable,
                efforts,
                budget: true,
            },
        }
    }

    #[test]
    fn signatures_stay_on_their_own_part() {
        let r = req(vec![Message::assistant(
            Source::new("google", "gemini-3-pro"),
            vec![
                ContentPart::Text(TextPart {
                    text: "Answer".into(),
                    raw: Some(json!({ "thoughtSignature": "Ct2A" })),
                    truncated: false,
                }),
                ContentPart::ToolCall(ToolCallPart {
                    calls: vec![
                        ToolCall {
                            id: "tool_0".into(),
                            name: "weather".into(),
                            args: "{\"city\":\"Paris\"}".into(),
                            raw: Some(json!({ "thoughtSignature": "Cs8B" })),
                        },
                        ToolCall {
                            id: "tool_1".into(),
                            name: "weather".into(),
                            args: "{\"city\":\"London\"}".into(),
                            raw: None,
                        },
                    ],
                }),
            ],
        )]);
        let mut w = Vec::new();
        let body = build_body(&r, &caps(vec![], true), None, &mut w);
        let parts = body["contents"][0]["parts"].as_array().unwrap();

        assert_eq!(
            parts[0]["thoughtSignature"], "Ct2A",
            "a 2.5 signature can land on text"
        );
        assert_eq!(
            parts[1]["thoughtSignature"], "Cs8B",
            "attached to the first call only"
        );
        assert!(
            parts[2].get("thoughtSignature").is_none(),
            "must not be copied onto other calls"
        );
        assert_eq!(parts[1]["functionCall"]["args"]["city"], "Paris");
    }

    #[test]
    fn function_responses_go_into_one_user_turn_in_order() {
        let r = req(vec![
            Message::assistant(
                Source::new("google", "gemini-3-pro"),
                vec![ContentPart::ToolCall(ToolCallPart {
                    calls: vec![ToolCall {
                        id: "tool_0".into(),
                        name: "a".into(),
                        args: "{}".into(),
                        raw: None,
                    }],
                })],
            ),
            Message::tool(vec![ContentPart::ToolResult(ToolResultPart {
                files: Vec::new(),
                call_id: "tool_0".into(),
                name: "a".into(),
                content: "r1".into(),
                is_error: false,
            })]),
            Message::tool(vec![ContentPart::ToolResult(ToolResultPart {
                files: Vec::new(),
                call_id: "tool_1".into(),
                name: "b".into(),
                content: "r2".into(),
                is_error: false,
            })]),
        ]);
        let mut w = Vec::new();
        let body = build_body(&r, &caps(vec![], true), None, &mut w);
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 2);
        let parts = contents[1]["parts"].as_array().unwrap();
        assert_eq!(
            parts.len(),
            2,
            "the whole result group must sit in one user turn"
        );
        assert_eq!(parts[0]["functionResponse"]["name"], "a");
        assert_eq!(parts[1]["functionResponse"]["name"], "b");
    }

    #[test]
    fn reasoning_part_carries_thought_flag() {
        let r = req(vec![Message::assistant(
            Source::new("google", "gemini-3-pro"),
            vec![ContentPart::Reasoning(ReasoningPart {
                text: "thinking".into(),
                raw: Some(json!({ "thoughtSignature": "sig" })),
                truncated: false,
            })],
        )]);
        let mut w = Vec::new();
        let body = build_body(&r, &caps(vec![], true), None, &mut w);
        let p = &body["contents"][0]["parts"][0];
        assert_eq!(p["thought"], true);
        assert_eq!(p["thoughtSignature"], "sig");
    }

    #[test]
    fn gemini3_uses_thinking_level_gemini25_uses_budget() {
        use zlogic_protocol::llm::Effort;
        let mut r = req(vec![]);
        r.thinking = ThinkingIntent {
            mode: ThinkingMode::On,
            effort: Some(Effort::Low),
            budget_tokens: None,
        };
        let mut w = Vec::new();

        let g3 = build_body(
            &r,
            &caps(vec![Effort::Low, Effort::High], false),
            None,
            &mut w,
        );
        assert_eq!(
            g3["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "low"
        );

        let g25 = build_body(&r, &caps(vec![], true), None, &mut w);
        assert_eq!(
            g25["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            4096
        );
    }

    #[test]
    fn off_on_a_model_that_cannot_disable_omits() {
        use zlogic_protocol::llm::Effort;
        let mut r = req(vec![]);
        r.thinking = ThinkingIntent {
            mode: ThinkingMode::Off,
            effort: None,
            budget_tokens: None,
        };
        let mut w = Vec::new();
        let body = build_body(&r, &caps(vec![Effort::Low], false), None, &mut w);
        assert!(
            body.get("generationConfig")
                .and_then(|gc| gc.get("thinkingConfig"))
                .is_none(),
            "thinkingConfig must not be sent when it cannot be fully disabled — the gateway returns 400"
        );
        assert!(w.is_empty(), "no flooring any more, so no warning either");
    }

    #[test]
    fn tool_schemas_are_projected_into_the_gemini_dialect() {
        use zlogic_protocol::llm::ToolDefinition;
        let mut r = req(vec![]);
        r.tools = vec![
            ToolDefinition {
                name: "edit".into(),
                description: "d".into(),
                parameters: json!({
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "type": "object",
                    "additionalProperties": false,
                    "properties": { "level": { "type": "integer", "enum": [1, 2] } },
                    "required": ["level", "ghost"]
                }),
            },
            ToolDefinition {
                name: "noargs".into(),
                description: "d".into(),
                parameters: json!({ "type": "object", "properties": {} }),
            },
        ];
        let mut w = Vec::new();
        let body = build_body(&r, &caps(vec![], true), None, &mut w);
        let decls = body["tools"][0]["functionDeclarations"].as_array().unwrap();

        let params = &decls[0]["parameters"];
        assert!(
            params.get("additionalProperties").is_none(),
            "fields that would cause a 400 must be stripped"
        );
        assert!(params.get("$schema").is_none());
        assert_eq!(params["properties"]["level"]["type"], "string");
        assert_eq!(params["required"], json!(["level"]));

        assert!(
            decls[1].get("parameters").is_none(),
            "a no-arg tool must omit parameters outright; an empty object is rejected too"
        );
    }

    #[test]
    fn safety_finish_reasons_map_to_content_filter() {
        for fr in [
            "SAFETY",
            "IMAGE_SAFETY",
            "RECITATION",
            "BLOCKLIST",
            "PROHIBITED_CONTENT",
            "SPII",
        ] {
            assert_eq!(map_finish_reason(fr), FinishReason::ContentFilter, "{fr}");
        }
        assert_eq!(map_finish_reason("STOP"), FinishReason::Stop);
        assert_eq!(map_finish_reason("MAX_TOKENS"), FinishReason::Length);
        assert_eq!(
            map_finish_reason("MALFORMED_FUNCTION_CALL"),
            FinishReason::Other
        );
    }

    #[test]
    fn params_land_in_generation_config_not_at_the_root() {
        let mut r = req(vec![]);
        r.params.insert("temperature".into(), json!(0.3));
        let mut w = Vec::new();
        let body = build_body(&r, &caps(vec![], true), None, &mut w);
        assert_eq!(body["generationConfig"]["temperature"], 0.3);
        assert!(body.get("temperature").is_none());
    }
}
