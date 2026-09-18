use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use zlogic_protocol::llm::{LlmError, LlmRequest};

use crate::{EventStream, LlmClient};

pub const MAX_RETRIES: usize = 3;

fn retry_delay_ms(attempt: usize) -> u64 {
    match attempt {
        1 => 1_000,
        2 => 2_000,
        _ => 3_000,
    }
}

pub struct RetryingClient {
    inner: Arc<dyn LlmClient>,
    max_retries: usize,
}

impl RetryingClient {
    pub fn new(inner: Arc<dyn LlmClient>) -> Self {
        Self {
            inner,
            max_retries: MAX_RETRIES,
        }
    }
}

#[async_trait]
impl LlmClient for RetryingClient {
    async fn stream(&self, req: LlmRequest) -> Result<EventStream, LlmError> {
        let mut attempt = 0usize;
        loop {
            match self.inner.stream(req.clone()).await {
                Ok(stream) => return Ok(stream),
                Err(error) => {
                    if !error.retryable || attempt >= self.max_retries {
                        return Err(error);
                    }
                    attempt += 1;
                    let delay_ms = retry_delay_ms(attempt);
                    tracing::warn!(
                        target: "zlogic::llm::retry",
                        attempt,
                        kind = ?error.kind,
                        "retrying in {delay_ms}ms (attempt {attempt}): {error}"
                    );
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use futures_util::StreamExt;
    use futures_util::stream;
    use zlogic_protocol::llm::{
        CacheSpec, LlmErrorKind, LlmRequest, RequestMeta, ThinkingIntent, ThinkingMode,
    };
    use zlogic_protocol::message::{ContentPart, Message, TextPart};
    use zlogic_protocol::usage::Purpose;

    use super::*;

    fn request() -> LlmRequest {
        LlmRequest {
            model: "test-model".into(),
            system: vec![],
            messages: vec![Message::user(vec![ContentPart::Text(TextPart {
                text: "hi".into(),
                raw: None,
                truncated: false,
            })])],
            tools: vec![],
            cache: CacheSpec::off(),
            thinking: ThinkingIntent {
                mode: ThinkingMode::Off,
                ..Default::default()
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

    struct Flaky {
        failures: usize,
        error: LlmError,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl LlmClient for Flaky {
        async fn stream(&self, _req: LlmRequest) -> Result<EventStream, LlmError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if call <= self.failures {
                return Err(self.error.clone());
            }
            Ok(Box::pin(stream::empty()))
        }
    }

    struct FailingStream {
        error: LlmError,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl LlmClient for FailingStream {
        async fn stream(&self, _req: LlmRequest) -> Result<EventStream, LlmError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let items: Vec<Result<zlogic_protocol::llm::LlmEvent, LlmError>> =
                vec![Err(self.error.clone())];
            Ok(Box::pin(stream::iter(items)))
        }
    }

    fn retryable(kind: LlmErrorKind) -> LlmError {
        LlmError {
            kind,
            retryable: true,
            message: "boom".into(),
            status: None,
            request_id: None,
        }
    }

    #[tokio::test]
    async fn retries_until_success() {
        let inner = Flaky {
            failures: 2,
            error: retryable(LlmErrorKind::Network),
            calls: AtomicUsize::new(0),
        };
        let client = RetryingClient::new(Arc::new(inner));
        assert!(client.stream(request()).await.is_ok());
    }

    #[tokio::test]
    async fn gives_up_after_max_retries_and_returns_the_last_error() {
        let inner = Arc::new(Flaky {
            failures: usize::MAX,
            error: retryable(LlmErrorKind::RateLimit),
            calls: AtomicUsize::new(0),
        });
        let client = RetryingClient::new(inner.clone());
        let Err(error) = client.stream(request()).await else {
            panic!("a client that always fails should return Err");
        };
        assert_eq!(error.kind, LlmErrorKind::RateLimit);
        assert_eq!(inner.calls.load(Ordering::SeqCst), MAX_RETRIES + 1);
    }

    #[tokio::test]
    async fn never_retries_non_retryable_errors() {
        let inner = Arc::new(Flaky {
            failures: 1,
            error: LlmError {
                kind: LlmErrorKind::Auth,
                retryable: false,
                message: "bad key".into(),
                status: Some(401),
                request_id: None,
            },
            calls: AtomicUsize::new(0),
        });
        let client = RetryingClient::new(inner.clone());
        let Err(error) = client.stream(request()).await else {
            panic!("a non-retryable error should be returned as-is");
        };
        assert_eq!(error.kind, LlmErrorKind::Auth);
        assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn mid_stream_errors_pass_through_without_retry() {
        let inner = Arc::new(FailingStream {
            error: retryable(LlmErrorKind::Server),
            calls: AtomicUsize::new(0),
        });
        let client = RetryingClient::new(inner.clone());
        let mut stream = client.stream(request()).await.expect("handshake succeeds");
        let item = stream.next().await.expect("the stream carries one error");
        assert!(item.is_err());
        assert_eq!(
            inner.calls.load(Ordering::SeqCst),
            1,
            "a mid-stream error is never resent"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn waits_for_the_fixed_schedule_between_attempts() {
        let inner = Flaky {
            failures: 3,
            error: retryable(LlmErrorKind::RateLimit),
            calls: AtomicUsize::new(0),
        };
        let client = RetryingClient::new(Arc::new(inner));
        let start = tokio::time::Instant::now();
        assert!(client.stream(request()).await.is_ok());
        assert_eq!(
            start.elapsed(),
            Duration::from_millis(6_000),
            "should wait on the fixed schedule 1s+2s+3s: {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn retry_delays_are_fixed_1s_2s_3s() {
        assert_eq!(retry_delay_ms(1), 1_000);
        assert_eq!(retry_delay_ms(2), 2_000);
        assert_eq!(retry_delay_ms(3), 3_000);
        assert_eq!(
            retry_delay_ms(4),
            3_000,
            "capped at 3s past the third attempt too"
        );
        assert_eq!(retry_delay_ms(100), 3_000);
    }
}
