//! A single-flight TTL cache for the usage report.
//!
//! `usage.summary` costs roughly a second of SQLite and several views ask for it at once — the
//! tasks list on a ten-second timer, the usage view and the session manager on demand, the chat
//! panel after a turn. Two things are shared here: the computation itself (identical queries within
//! the TTL) and the in-flight computation (a caller that arrives while one is running waits for
//! that one instead of starting a second).
//!
//! Only the store half is cached. The cost config is applied afterwards, because it can change
//! without a single usage row changing.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use tokio::sync::OnceCell;
use zlogic_protocol::{SessionId, TurnId, WorkspaceId};
use zlogic_store::{ToolUsageAggregate, UsageAggregate, UsageQuery};

use crate::EngineError;

/// How long a computed report answers identical requests.
/// Long enough to absorb a burst of pollers, short enough that the numbers on screen are never more
/// than one poll interval behind the database.
pub const REPORT_TTL: Duration = Duration::from_secs(10);

/// Everything the report's SQL reads. Any field that changes which rows come back, or how they are
/// grouped, has to be part of it — otherwise two different questions share one answer.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ReportKey {
    workspace_id: Option<WorkspaceId>,
    session_id: Option<SessionId>,
    self_only: bool,
    session_kind: Option<&'static str>,
    turn_id: Option<TurnId>,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
    utc_offset_minutes: i32,
}

impl ReportKey {
    pub fn of(query: &UsageQuery) -> Self {
        Self {
            workspace_id: query.workspace_id,
            session_id: query.session_id,
            self_only: query.self_only,
            session_kind: query.session_kind.map(|kind| kind.as_str()),
            turn_id: query.turn_id,
            since: query.since,
            until: query.until,
            utc_offset_minutes: query.utc_offset_minutes,
        }
    }
}

/// What the report costs a second to produce: the aggregate for every dimension plus the tool
/// roll-up.
#[derive(Clone)]
pub struct UsageReport {
    pub aggregate: UsageAggregate,
    pub tools: Vec<ToolUsageAggregate>,
}

pub struct ReportCache {
    ttl: Duration,
    slots: Mutex<HashMap<ReportKey, Arc<Slot>>>,
}

struct Slot {
    born: Instant,
    report: OnceCell<Arc<UsageReport>>,
}

impl Default for ReportCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ReportCache {
    pub fn new() -> Self {
        Self::with_ttl(REPORT_TTL)
    }

    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            ttl,
            slots: Mutex::new(HashMap::new()),
        }
    }

    /// Returns the report for `key`, computing it with `compute` when the cached one is missing or
    /// stale. A failed computation is not cached: the next caller retries.
    pub async fn get<F, Fut>(
        &self,
        key: ReportKey,
        compute: F,
    ) -> Result<Arc<UsageReport>, EngineError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<UsageReport, EngineError>>,
    {
        let slot = self.slot(key);
        slot.report
            .get_or_try_init(|| async { compute().await.map(Arc::new) })
            .await
            .map(Arc::clone)
    }

    /// The slot for `key`: the live one while it is fresh, a fresh one otherwise.
    fn slot(&self, key: ReportKey) -> Arc<Slot> {
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        // The keys come from the UI, so the map stays tiny: dropping anything several TTLs old is
        // cheaper than tracking a second timer.
        slots.retain(|_, slot| slot.born.elapsed() < self.ttl * 4);
        match slots.get(&key) {
            Some(slot) if slot.born.elapsed() < self.ttl => Arc::clone(slot),
            _ => {
                let slot = Arc::new(Slot {
                    born: Instant::now(),
                    report: OnceCell::new(),
                });
                slots.insert(key, Arc::clone(&slot));
                slot
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn key(offset_minutes: i32) -> ReportKey {
        ReportKey::of(&UsageQuery {
            utc_offset_minutes: offset_minutes,
            ..UsageQuery::default()
        })
    }

    fn report() -> UsageReport {
        UsageReport {
            aggregate: UsageAggregate::default(),
            tools: Vec::new(),
        }
    }

    #[tokio::test]
    async fn identical_requests_are_computed_once() {
        let cache = ReportCache::new();
        let computed = AtomicUsize::new(0);
        let compute = || async {
            computed.fetch_add(1, Ordering::SeqCst);
            tokio::task::yield_now().await;
            Ok(report())
        };

        let (first, second) =
            tokio::join!(cache.get(key(480), compute), cache.get(key(480), compute));
        assert!(first.is_ok() && second.is_ok());
        assert_eq!(computed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_different_query_is_a_different_entry() {
        let cache = ReportCache::new();
        let computed = AtomicUsize::new(0);
        let compute = || async {
            computed.fetch_add(1, Ordering::SeqCst);
            Ok(report())
        };

        cache.get(key(480), compute).await.unwrap();
        cache.get(key(-300), compute).await.unwrap();
        assert_eq!(computed.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn an_entry_expires_after_the_ttl() {
        let cache = ReportCache::with_ttl(Duration::from_millis(50));
        let computed = AtomicUsize::new(0);
        let compute = || async {
            computed.fetch_add(1, Ordering::SeqCst);
            Ok(report())
        };

        cache.get(key(480), compute).await.unwrap();
        cache.get(key(480), compute).await.unwrap();
        assert_eq!(computed.load(Ordering::SeqCst), 1);

        tokio::time::sleep(Duration::from_millis(80)).await;
        cache.get(key(480), compute).await.unwrap();
        assert_eq!(computed.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_failed_computation_is_retried_not_cached() {
        let cache = ReportCache::new();
        let computed = AtomicUsize::new(0);

        let failure = cache
            .get(key(480), || async {
                computed.fetch_add(1, Ordering::SeqCst);
                Err(EngineError::Invalid("no database".into()))
            })
            .await;
        assert!(failure.is_err());

        let success = cache
            .get(key(480), || async {
                computed.fetch_add(1, Ordering::SeqCst);
                Ok(report())
            })
            .await;
        assert!(success.is_ok());
        assert_eq!(computed.load(Ordering::SeqCst), 2);
    }
}
