//! The configured request budget, enforced before a request leaves the process.
//!
//! A limit that is only discovered through a 429 has already cost a round trip and, on a shared
//! key, a slice of someone else's quota. When a model configures `rate_limit.rpm`, the request
//! waits here instead — and the wait is visible in the log, because a turn that pauses for a
//! minute is otherwise indistinguishable from a hung one.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use zlogic_protocol::llm::{LlmError, LlmRequest};

use crate::{EventStream, LlmClient};

/// The window `rpm` is counted over.
const WINDOW: Duration = Duration::from_secs(60);

/// One sliding window per `provider:model`, shared by every client built for it.
///
/// Clients are rebuilt per turn, so the window cannot live in the client: each new one would
/// start its own count and the configured limit would be multiplied by the number of turns.
pub struct RateLimiter {
    windows: Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl RateLimiter {
    fn new() -> Self {
        Self {
            windows: Mutex::new(HashMap::new()),
        }
    }

    /// Wait until `key` is under `rpm` requests in the trailing minute, then take a slot.
    ///
    /// The lock is only ever held long enough to read the window, never across the sleep: every
    /// client for the same model has to be able to queue behind the same window.
    async fn acquire(&self, key: &str, rpm: u32) {
        loop {
            let wait = {
                let mut windows = self.windows.lock().expect("rate limiter");
                let seen = windows.entry(key.to_string()).or_default();
                let now = Instant::now();
                while seen
                    .front()
                    .is_some_and(|at| now.duration_since(*at) >= WINDOW)
                {
                    seen.pop_front();
                }
                if seen.len() < rpm as usize {
                    seen.push_back(now);
                    return;
                }
                WINDOW
                    .saturating_sub(now.duration_since(*seen.front().expect("window is not empty")))
            };
            tracing::info!(
                target: "zlogic::llm::ratelimit",
                model = key,
                rpm,
                wait_ms = wait.as_millis() as u64,
                "request held back by the configured rate limit"
            );
            tokio::time::sleep(wait).await;
        }
    }
}

pub fn shared() -> &'static RateLimiter {
    static SHARED: OnceLock<RateLimiter> = OnceLock::new();
    SHARED.get_or_init(RateLimiter::new)
}

/// Holds a request until the model's configured budget allows it.
pub struct RateLimitedClient {
    inner: Arc<dyn LlmClient>,
    model: String,
    rpm: u32,
}

impl RateLimitedClient {
    /// A model with no limit is not wrapped at all, so the common case pays nothing.
    pub fn wrap(inner: Arc<dyn LlmClient>, model: &str, rpm: Option<u32>) -> Arc<dyn LlmClient> {
        match rpm {
            Some(rpm) if rpm > 0 => Arc::new(Self {
                inner,
                model: model.to_string(),
                rpm,
            }),
            _ => inner,
        }
    }
}

#[async_trait]
impl LlmClient for RateLimitedClient {
    async fn stream(&self, req: LlmRequest) -> Result<EventStream, LlmError> {
        shared().acquire(&self.model, self.rpm).await;
        self.inner.stream(req).await
    }
}
