//! Retention that runs without a session.
//!
//! A capture sweeps the repository it is about to write, which is the right place for the policy —
//! and the wrong place to be the only place. A repository the user has walked away from is never
//! swept again, so its copies of their code stay on disk for as long as the app is installed, and
//! the retention days in the settings apply to the one store that is busy rather than to the one
//! that is stale. This pass walks every store under the root on a timer instead.
//!
//! It runs whether or not checkpoints are on. Turning the feature off stops new copies, and what
//! is already on disk ages out under the same retention the user chose.

use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crate::Checkpoints;

/// How often the pass runs. The same hour the capture path uses between sweeps of one repository:
/// frequent enough that nothing outlives its retention by more than an hour, rare enough that
/// opening a store's commit chain is not something the app does while the user is reading.
pub const INTERVAL: Duration = Duration::from_secs(3600);

pub fn spawn(store: Arc<Checkpoints>, delay: Duration) -> Option<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("zlogic-checkpoint-retention".into())
        .spawn(move || {
            std::thread::sleep(delay);
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread().build() else {
                return;
            };
            loop {
                match runtime.block_on(store.sweep_all(crate::now())) {
                    Ok(report) if report.dropped > 0 => tracing::info!(
                        target: "zlogic::checkpoints",
                        dropped = report.dropped,
                        kept = report.kept,
                        "checkpoint retention dropped snapshots past the policy"
                    ),
                    Ok(_) => {}
                    Err(error) => {
                        tracing::warn!(target: "zlogic::checkpoints", %error, "checkpoint retention failed");
                    }
                }
                std::thread::sleep(INTERVAL);
            }
        })
        .ok()
}