use std::sync::Arc;

use async_trait::async_trait;
use futures_util::StreamExt;
use zlogic_protocol::llm::{LlmError, LlmRequest};

use crate::{EventStream, LlmClient};

pub struct DetailLoggingClient {
    inner: Arc<dyn LlmClient>,
}

impl DetailLoggingClient {
    pub fn new(inner: Arc<dyn LlmClient>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl LlmClient for DetailLoggingClient {
    async fn stream(&self, req: LlmRequest) -> Result<EventStream, LlmError> {
        match self.inner.stream(req.clone()).await {
            Err(error) => {
                log_error("handshake", &req, &error);
                Err(error)
            }
            Ok(stream) => Ok(Box::pin(stream.map(move |item| {
                if let Err(error) = &item {
                    log_error("stream", &req, error);
                }
                item
            }))),
        }
    }
}

fn log_error(phase: &str, req: &LlmRequest, error: &LlmError) {
    tracing::error!(
        target: "zlogic::llm",
        phase,
        model = %req.model,
        session_id = %req.meta.session_id,
        turn_id = %req.meta.turn_id,
        round_id = %req.meta.round_id,
        purpose = ?req.meta.purpose,
        kind = ?error.kind,
        retryable = error.retryable,
        status = error.status,
        request_id = ?error.request_id,
        "LLM request failed ({phase}): {error}",
    );
}
