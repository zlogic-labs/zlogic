use async_trait::async_trait;
use futures_util::stream;
use zlogic_protocol::llm::{FinishReason, LlmError, LlmEvent, LlmRequest};
use zlogic_protocol::usage::{TokenUsage, UsageReport};

use crate::emitter::{PartEmitter, ReasoningRawSpec};
use crate::{EventStream, LlmClient};

#[derive(Debug, Clone, Default)]
pub struct MockScript {
    pub reasoning: Option<String>,
    pub text: Option<String>,
    /// `(call_index, id, name, args)`
    pub tool_calls: Vec<(u32, String, String, String)>,
    pub usage: Option<TokenUsage>,
    pub notice: Option<(String, String)>,
    pub finish: Option<FinishReason>,
    pub fail_with: Option<LlmError>,
}

impl MockScript {
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            text: Some(s.into()),
            ..Default::default()
        }
    }
}

pub struct MockClient {
    script: MockScript,
}

impl MockClient {
    pub fn new(script: MockScript) -> Self {
        Self { script }
    }
}

impl Default for MockClient {
    fn default() -> Self {
        Self::new(MockScript::text(
            "[mock client] no usable API key is configured; this is not a real model response.",
        ))
    }
}

#[async_trait]
impl LlmClient for MockClient {
    async fn stream(&self, _req: LlmRequest) -> Result<EventStream, LlmError> {
        if let Some(e) = &self.script.fail_with {
            return Err(e.clone());
        }

        let mut em = PartEmitter::new(ReasoningRawSpec::None);
        if let Some(r) = &self.script.reasoning {
            em.reasoning_delta(r);
        }
        if let Some(t) = &self.script.text {
            em.text_delta(t);
        }
        for (idx, id, name, args) in &self.script.tool_calls {
            em.tool_call_id(*idx, id.clone());
            em.tool_call_name(*idx, name.clone());
            em.tool_call_args(*idx, args);
        }
        if let Some(u) = self.script.usage {
            em.usage(UsageReport {
                tokens: u,
                cost: None,
                raw: None,
            });
        }
        if let Some((code, message)) = &self.script.notice {
            em.notice(code.clone(), message.clone());
        }
        let finish = self
            .script
            .finish
            .unwrap_or(if self.script.tool_calls.is_empty() {
                FinishReason::Stop
            } else {
                FinishReason::ToolCalls
            });
        em.finish(finish);

        let events: Vec<Result<LlmEvent, LlmError>> = em.drain().into_iter().map(Ok).collect();
        Ok(Box::pin(stream::iter(events)))
    }
}
