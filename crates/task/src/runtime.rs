use std::future::Future;

use tokio::task::{JoinError, JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::TaskId;

/// Process-local ownership of one running task.
/// Dropping the handle detaches the future; cancellation is always explicit. Durable state belongs
/// to [`crate::TaskRun`], not here.
#[derive(Debug)]
pub struct RuntimeHandle {
    task_id: TaskId,
    cancel: CancellationToken,
    join: JoinHandle<()>,
}

impl RuntimeHandle {
    /// Spawns a future and gives it the same per-task cancellation token exposed by the handle.
    pub fn spawn<F, Fut>(task_id: TaskId, run: F) -> Self
    where
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let cancel = CancellationToken::new();
        let join = tokio::spawn(run(cancel.clone()));
        Self {
            task_id,
            cancel,
            join,
        }
    }

    pub const fn task_id(&self) -> TaskId {
        self.task_id
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    pub fn is_finished(&self) -> bool {
        self.join.is_finished()
    }

    pub fn abort(&self) {
        self.join.abort();
    }

    pub async fn wait(self) -> Result<(), JoinError> {
        self.join.await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    #[tokio::test]
    async fn cancellation_reaches_the_runtime() {
        let stopped = Arc::new(AtomicBool::new(false));
        let observed = stopped.clone();
        let handle = RuntimeHandle::spawn(TaskId::new(), move |cancel| async move {
            cancel.cancelled().await;
            observed.store(true, Ordering::SeqCst);
        });

        handle.cancel();
        handle.wait().await.unwrap();
        assert!(stopped.load(Ordering::SeqCst));
    }
}
