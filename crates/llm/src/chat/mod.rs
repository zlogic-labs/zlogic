pub mod body;
pub mod generic;
pub mod vendors;

use std::sync::Arc;

use async_stream::try_stream;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Map, Value};
use zlogic_protocol::config::ThinkingCapability;
use zlogic_protocol::llm::{
    Effort, FinishReason, LlmError, LlmEvent, LlmRequest, ResponseFormat, ThinkingIntent,
};
use zlogic_protocol::usage::UsageReport;

use crate::emitter::{PartEmitter, ReasoningRawSpec};
use crate::think_tags::{Segment, ThinkTagSplitter};
use crate::transport::HttpTransport;
use crate::usage_map::{self, UsageOverrides, UsageShape};
use crate::{Endpoint, EventStream, LlmClient, error};

pub trait ChatVendor: Send + Sync + 'static {
    fn name(&self) -> &'static str;

    fn reasoning_raw_spec(&self) -> ReasoningRawSpec {
        ReasoningRawSpec::None
    }

    fn reasoning_delta<'a>(&self, delta: &'a Map<String, Value>) -> Option<&'a str> {
        let _ = delta;
        None
    }

    fn reasoning_raw_from_delta(
        &self,
        scratch: &mut Map<String, Value>,
        delta: &Map<String, Value>,
    ) -> Option<Value> {
        let _ = (scratch, delta);
        None
    }

    fn think_tags(&self) -> bool {
        false
    }

    fn always_send_reasoning_field(&self) -> bool {
        false
    }

    fn supports_prompt_cache_key(&self) -> bool {
        false
    }

    fn wants_stream_options(&self) -> bool {
        true
    }

    fn map_thinking(
        &self,
        intent: &ThinkingIntent,
        caps: &ThinkingCapability,
        warnings: &mut Vec<String>,
    ) -> Map<String, Value> {
        let _ = (intent, caps, warnings);
        Map::new()
    }

    fn transform_body(&self, body: &mut Map<String, Value>) {
        let _ = body;
    }

    fn response_format(&self, rf: &ResponseFormat, warnings: &mut Vec<String>) -> Value {
        let _ = (self, warnings);
        body::response_format(rf)
    }

    fn splice_reasoning(&self, msg: &mut Map<String, Value>, raw: &Value) {
        if let (Some(carrier), Some(value)) = (
            raw.get("carrier").and_then(|v| v.as_str()),
            raw.get("value"),
        ) {
            msg.insert(carrier.to_string(), value.clone());
        }
    }

    fn usage_shape(&self) -> &'static UsageShape {
        &usage_map::OPENAI
    }

    fn usage_overrides(&self) -> UsageOverrides {
        UsageOverrides::default()
    }

    fn read_usage(&self, raw: &Value) -> Result<UsageReport, String> {
        usage_map::extract_checked(raw, self.usage_shape(), &self.usage_overrides())
    }

    fn path(&self) -> &'static str {
        "chat/completions"
    }

    fn vendor_name(&self) -> &'static str {
        self.name()
    }
}

pub struct ChatClient<V: ChatVendor> {
    vendor: Arc<V>,
    endpoint: Arc<Endpoint>,
    transport: Arc<dyn HttpTransport>,
}

impl<V: ChatVendor> ChatClient<V> {
    pub fn new(vendor: V, endpoint: Endpoint, transport: Arc<dyn HttpTransport>) -> Self {
        Self {
            vendor: Arc::new(vendor),
            endpoint: Arc::new(endpoint),
            transport,
        }
    }
}

#[async_trait]
impl<V: ChatVendor> LlmClient for ChatClient<V> {
    async fn stream(&self, req: LlmRequest) -> Result<EventStream, LlmError> {
        let mut warnings = Vec::new();
        let body = body::build_body(
            &req,
            &*self.vendor,
            &self.endpoint.capabilities,
            &mut warnings,
        );
        let payload = serde_json::to_vec(&body).map_err(|e| {
            error::err(
                zlogic_protocol::llm::LlmErrorKind::BadRequest,
                e.to_string(),
            )
        })?;

        let mut http = self.endpoint.request(self.vendor.path(), payload);
        http = self.endpoint.authorize(http, crate::AuthHeader::Bearer);
        for (k, v) in &self.endpoint.extra_headers {
            http = http.header(k.clone(), v.clone());
        }

        let bytes =
            self.transport.post_stream(http).await.map_err(|e| {
                crate::error::attach_endpoint(e, &self.endpoint, self.vendor.path())
            })?;

        let vendor = self.vendor.clone();
        let endpoint = self.endpoint.clone();
        let path = self.vendor.path().to_string();
        Ok(Box::pin(drive(bytes, vendor, warnings).map(move |item| {
            item.map_err(|e| crate::error::attach_endpoint(e, &endpoint, &path))
        })))
    }
}

fn drive<V: ChatVendor>(
    bytes: crate::transport::ByteStream,
    vendor: Arc<V>,
    initial_warnings: Vec<String>,
) -> impl futures_core::Stream<Item = Result<LlmEvent, LlmError>> {
    try_stream! {
        let mut sse = crate::sse::events(bytes);
        let mut emitter = PartEmitter::new(vendor.reasoning_raw_spec());
        for w in initial_warnings {
            emitter.warn(w);
        }
        let mut splitter = vendor.think_tags().then(ThinkTagSplitter::new);
        let mut finish = FinishReason::Stop;
        let mut scratch = Map::new();

        while let Some(ev) = sse.next().await {
            let ev = ev?;
            if ev.is_done() {
                continue;
            }
            handle_data(
                &ev.data, &*vendor, &mut emitter, &mut splitter, &mut finish, &mut scratch,
            )?;
            for e in emitter.drain() {
                yield e;
            }
        }

        if let Some(sp) = &mut splitter {
            for seg in sp.finish() {
                match seg {
                    Segment::Text(t) => emitter.text_delta(&t),
                    Segment::Reasoning(t) => emitter.reasoning_delta_display_only(&t),
                }
            }
        }

        if matches!(finish, FinishReason::Stop) && emitter.has_tool_calls() {
            finish = FinishReason::ToolCalls;
        }

        emitter.finish(finish);
        for e in emitter.drain() {
            yield e;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_data<V: ChatVendor + ?Sized>(
    data: &str,
    vendor: &V,
    emitter: &mut PartEmitter,
    splitter: &mut Option<ThinkTagSplitter>,
    finish: &mut FinishReason,
    scratch: &mut Map<String, Value>,
) -> Result<(), LlmError> {
    let Ok(v) = serde_json::from_str::<Value>(data) else {
        emitter.warn(format!("unparseable SSE data chunk ({} bytes)", data.len()));
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

    if let Some(usage) = v.get("usage").filter(|u| u.is_object()) {
        match vendor.read_usage(usage) {
            Ok(report) => emitter.usage(report),
            Err(_) => {}
        }
    }

    let Some(choice) = v.get("choices").and_then(|c| c.get(0)) else {
        return Ok(());
    };

    if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str()) {
        *finish = map_finish_reason(fr);
    }

    let Some(delta) = choice.get("delta").and_then(|d| d.as_object()) else {
        return Ok(());
    };

    if let Some(r) = vendor.reasoning_delta(delta) {
        let r = r.to_string();
        emitter.reasoning_delta(&r);
    }
    if let Some(raw) = vendor.reasoning_raw_from_delta(scratch, delta) {
        emitter.set_reasoning_raw(raw);
    }

    if let Some(content) = delta.get("content").and_then(|c| c.as_str())
        && !content.is_empty()
    {
        match splitter {
            Some(sp) => {
                for seg in sp.push(content) {
                    match seg {
                        Segment::Text(t) => emitter.text_delta(&t),
                        Segment::Reasoning(t) => emitter.reasoning_delta_display_only(&t),
                    }
                }
            }
            None => emitter.text_delta(content),
        }
    }

    if let Some(calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
        for (fallback_idx, call) in calls.iter().enumerate() {
            let idx = call
                .get("index")
                .and_then(|i| i.as_u64())
                .unwrap_or(fallback_idx as u64) as u32;
            if let Some(id) = call.get("id").and_then(|i| i.as_str())
                && !id.is_empty()
            {
                emitter.tool_call_id(idx, id);
            }
            if let Some(f) = call.get("function") {
                if let Some(name) = f.get("name").and_then(|n| n.as_str()) {
                    emitter.tool_call_name(idx, name);
                }
                if let Some(args) = f.get("arguments").and_then(|a| a.as_str()) {
                    emitter.tool_call_args(idx, args);
                }
            }
        }
    }

    Ok(())
}

pub(crate) fn map_finish_reason(s: &str) -> FinishReason {
    match s {
        "stop" | "end_turn" | "STOP" => FinishReason::Stop,
        "tool_calls" | "function_call" | "tool_use" => FinishReason::ToolCalls,
        "length" | "max_tokens" | "MAX_TOKENS" => FinishReason::Length,
        "content_filter" | "SAFETY" | "refusal" => FinishReason::ContentFilter,
        _ => FinishReason::Other,
    }
}

pub(crate) fn effort_for(intent: &ThinkingIntent, declared: &[Effort]) -> Option<Effort> {
    if declared.is_empty() {
        return None;
    }
    let want = intent.effort?;
    if declared.contains(&want) {
        return Some(want);
    }
    declared
        .iter()
        .filter(|e| e.rank() <= want.rank())
        .max_by_key(|e| e.rank())
        .or_else(|| declared.iter().min_by_key(|e| e.rank()))
        .copied()
}

pub(crate) fn lowest_effort(declared: &[Effort]) -> Option<Effort> {
    declared.iter().min_by_key(|e| e.rank()).copied()
}

pub(crate) fn effort_budget(intent: &ThinkingIntent) -> Option<u32> {
    if let Some(b) = intent.budget_tokens {
        return Some(b);
    }
    match intent.effort? {
        Effort::Minimal => Some(1024),
        Effort::Low => Some(4096),
        Effort::Medium => Some(16384),
        Effort::High => Some(32768),
        Effort::XHigh => Some(49152),
        Effort::Max => Some(65536),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::generic::GenericOpenAi;
    use zlogic_protocol::config::{GenericOpenAiDialect, UsageFieldMap};

    #[test]
    fn bad_usage_mapping_does_not_fail_the_response_chunk() {
        let vendor = GenericOpenAi::new(GenericOpenAiDialect {
            usage_fields: UsageFieldMap {
                input: Some("wrong.prompt_tokens".into()),
                output: Some("completion_tokens".into()),
                ..Default::default()
            },
            ..Default::default()
        });
        let mut emitter = PartEmitter::new(ReasoningRawSpec::None);
        let mut splitter = None;
        let mut finish = FinishReason::Stop;
        let mut scratch = Map::new();
        let data = serde_json::json!({
            "usage": {
                "prompt_tokens": 20,
                "completion_tokens": 4
            },
            "choices": [{
                "delta": { "content": "answer" },
                "finish_reason": "stop"
            }]
        })
        .to_string();

        handle_data(
            &data,
            &vendor,
            &mut emitter,
            &mut splitter,
            &mut finish,
            &mut scratch,
        )
        .unwrap();
        let events = emitter.drain();

        // Unreadable usage is dropped quietly: the same mapping failure would repeat on every
        // response, and a per-response notice is just noise in the transcript.
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, LlmEvent::Notice { .. })),
            "{events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, LlmEvent::Usage(_))),
            "{events:?}"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                LlmEvent::PartDelta { delta, .. } if delta == "answer"
            )),
            "{events:?}"
        );
    }
}
