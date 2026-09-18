//! Store calls off the async runtime's worker threads, and off the interactive connection pool.
//!
//! `SharedStore::with*` runs its SQL on the calling thread, and store calls are reached from async
//! engine methods — which the host polls on the runtime's worker threads. A multi-second query
//! therefore parks a worker, and everything queued behind that worker waits with it. These helpers
//! hand the call to a blocking thread instead; the runtime keeps polling.

use std::panic::Location;

use zlogic_store::{Db, SharedStore};

/// Runs `f` on a blocking thread, against the interactive lane.
/// For interactive reads whose cost grows with the size of what they return — a transcript page, a
/// session open — so that one large call cannot park an async worker.
pub async fn blocking_at<R>(
    store: &SharedStore,
    operation: &'static str,
    caller: &'static Location<'static>,
    f: impl FnOnce(&Db) -> R + Send + 'static,
) -> R
where
    R: Send + 'static,
{
    let store = store.clone();
    offload(move || store.with_named_at(operation, caller, f)).await
}

/// Runs `f` on a blocking thread, against the report lane.
/// Reports are the calls that are allowed to be slow: this keeps them off the interactive pool and
/// off the runtime, so the worst a slow report can do is delay another report.
pub async fn report_at<R>(
    store: &SharedStore,
    operation: &'static str,
    caller: &'static Location<'static>,
    f: impl FnOnce(&Db) -> R + Send + 'static,
) -> R
where
    R: Send + 'static,
{
    let store = store.clone();
    offload(move || store.with_report_named_at(operation, caller, f)).await
}

async fn offload<R>(call: impl FnOnce() -> R + Send + 'static) -> R
where
    R: Send + 'static,
{
    match tokio::task::spawn_blocking(call).await {
        Ok(value) => value,
        // Keep the caller's behaviour: a panicking store closure panics the awaiting command, which
        // the desktop's panic guard turns into an error the UI can show.
        Err(error) => std::panic::resume_unwind(error.into_panic()),
    }
}
