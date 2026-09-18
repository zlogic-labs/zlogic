use zlogic_core::{CancellationToken, SharedStore};
use zlogic_protocol::{HolderId, SessionId, TurnId};
use zlogic_store::{LockOutcome, SessionLock};

use crate::{EngineError, Result};

pub use zlogic_store::lock::{HEARTBEAT_INTERVAL, STALE_AFTER};

pub struct SessionLocks {
    store: SharedStore,
    holder_kind: String,
}

impl SessionLocks {
    pub fn new(store: SharedStore, holder_kind: impl Into<String>) -> Self {
        Self {
            store,
            holder_kind: holder_kind.into(),
        }
    }

    pub fn claim(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        cancel: CancellationToken,
    ) -> Result<LockGuard> {
        let kind = self.holder_kind.clone();
        let outcome = self
            .store
            .with(|db| db.locks().acquire(session_id, turn_id, Some(&kind)))?;

        let lock = match outcome {
            LockOutcome::Acquired(lock) => lock,
            LockOutcome::Busy(held) => {
                tracing::info!(
                    target: "zlogic::engine",
                    session = %session_id,
                    holder = %held.holder_id,
                    pid = held.pid,
                    kind = held.holder_kind.as_deref().unwrap_or("?"),
                    "session is already held"
                );
                return Err(EngineError::Busy {
                    session_id: session_id.to_string(),
                });
            }
        };

        let stop = CancellationToken::new();
        spawn_heartbeat(self.store.clone(), lock.clone(), stop.clone(), cancel);

        Ok(LockGuard {
            store: self.store.clone(),
            lock,
            stop,
        })
    }

    pub fn live_turn(&self, session_id: SessionId) -> Result<Option<TurnId>> {
        Ok(self.store.with(|db| db.locks().live_turn(session_id))?)
    }

    pub fn holder(&self, session_id: SessionId) -> Result<Option<SessionLock>> {
        Ok(self.store.with(|db| db.locks().get(session_id))?)
    }
}

pub struct LockGuard {
    store: SharedStore,
    lock: SessionLock,
    stop: CancellationToken,
}

impl LockGuard {
    pub fn session_id(&self) -> SessionId {
        self.lock.session_id
    }

    pub fn turn_id(&self) -> TurnId {
        self.lock.turn_id
    }

    pub fn holder_id(&self) -> HolderId {
        self.lock.holder_id
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        self.stop.cancel();
        let released = self.store.with(|db| {
            db.locks()
                .release(self.lock.session_id, self.lock.holder_id)
        });
        match released {
            Ok(true) => {}
            Ok(false) => tracing::warn!(
                target: "zlogic::engine",
                session = %self.lock.session_id,
                "lock no longer belongs to this holder at release (taken over meanwhile)"
            ),
            Err(e) => {
                tracing::error!(target: "zlogic::engine", "failed to release session lock: {e}")
            }
        }
    }
}

fn spawn_heartbeat(
    store: SharedStore,
    lock: SessionLock,
    stop: CancellationToken,
    cancel: CancellationToken,
) {
    let interval = HEARTBEAT_INTERVAL
        .to_std()
        .unwrap_or(std::time::Duration::from_secs(20));
    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = stop.cancelled() => return,
                () = tokio::time::sleep(interval) => {}
            }

            let still_ours = store.with(|db| db.locks().heartbeat(lock.session_id, lock.holder_id));
            match still_ours {
                Ok(true) => {}
                Ok(false) => {
                    tracing::warn!(
                        target: "zlogic::engine",
                        session = %lock.session_id,
                        "session lock was taken over, stopping this turn"
                    );
                    cancel.cancel();
                    return;
                }
                Err(e) => tracing::warn!(target: "zlogic::engine", "heartbeat failed: {e}"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use zlogic_protocol::WorkspaceId;
    use zlogic_store::{Db, NewSession};

    fn setup() -> (SharedStore, SessionId) {
        let db = Db::open_in_memory().unwrap();
        let session = db
            .sessions()
            .create(NewSession::root(WorkspaceId::new()))
            .unwrap()
            .session_id;
        (SharedStore::new(db), session)
    }

    #[tokio::test]
    async fn one_turn_at_a_time_per_session() {
        let (store, session) = setup();
        let locks = SessionLocks::new(store.clone(), "test");

        let first = locks
            .claim(session, TurnId::new(), CancellationToken::new())
            .unwrap();
        assert_eq!(locks.live_turn(session).unwrap(), Some(first.turn_id()));

        let second = locks.claim(session, TurnId::new(), CancellationToken::new());
        assert!(matches!(second, Err(EngineError::Busy { .. })));

        drop(first);
        assert_eq!(
            locks.live_turn(session).unwrap(),
            None,
            "dropping it releases the lock"
        );
        assert!(
            locks
                .claim(session, TurnId::new(), CancellationToken::new())
                .is_ok()
        );
    }

    #[tokio::test]
    async fn a_panicking_turn_still_releases() {
        let (store, session) = setup();
        let locks = SessionLocks::new(store.clone(), "test");

        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = locks
                .claim(session, TurnId::new(), CancellationToken::new())
                .unwrap();
            panic!("boom");
        }));

        assert_eq!(locks.live_turn(session).unwrap(), None);
    }

    #[tokio::test]
    async fn locks_are_per_session() {
        let db = Db::open_in_memory().unwrap();
        let ws = WorkspaceId::new();
        let a = db
            .sessions()
            .create(NewSession::root(ws))
            .unwrap()
            .session_id;
        let b = db
            .sessions()
            .create(NewSession::root(ws))
            .unwrap()
            .session_id;
        let locks = SessionLocks::new(SharedStore::new(db), "test");

        let _one = locks
            .claim(a, TurnId::new(), CancellationToken::new())
            .unwrap();
        assert!(
            locks
                .claim(b, TurnId::new(), CancellationToken::new())
                .is_ok()
        );
    }

    #[tokio::test]
    async fn releasing_after_a_takeover_does_not_steal_the_new_holders_lock() {
        let (store, session) = setup();
        let locks = SessionLocks::new(store.clone(), "test");
        let guard = locks
            .claim(session, TurnId::new(), CancellationToken::new())
            .unwrap();

        let taken = store
            .with(|db| {
                db.locks()
                    .steal(session, guard.holder_id(), TurnId::new(), Some("other"))
            })
            .unwrap();
        assert!(taken);
        let new_holder = locks.holder(session).unwrap().unwrap().holder_id;

        drop(guard);
        assert_eq!(
            locks.holder(session).unwrap().unwrap().holder_id,
            new_holder,
            "the taker's lock must still be there, otherwise the session is left unowned"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn losing_the_lock_cancels_the_turn() {
        let (store, session) = setup();
        let locks = SessionLocks::new(store.clone(), "test");
        let cancel = CancellationToken::new();
        let guard = locks.claim(session, TurnId::new(), cancel.clone()).unwrap();

        store
            .with(|db| {
                db.locks()
                    .steal(session, guard.holder_id(), TurnId::new(), Some("other"))
            })
            .unwrap();
        assert!(!cancel.is_cancelled(), "the heartbeat is not due yet");

        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(21)).await;
        for _ in 0..20 {
            if cancel.is_cancelled() {
                break;
            }
            tokio::task::yield_now().await;
        }

        assert!(
            cancel.is_cancelled(),
            "the heartbeat must cancel this turn once it sees it was taken over"
        );
    }
}
