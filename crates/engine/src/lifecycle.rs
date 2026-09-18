//! |---|---|---|

use chrono::Utc;
use zlogic_core::SharedStore;
use zlogic_protocol::interaction::InteractionDecision;

use crate::Result;

pub const INTERRUPTED_AT_STARTUP: &str = "interrupted_at_startup";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Reconciled {
    pub locks_released: usize,
    pub interactions_closed: usize,
}

impl Reconciled {
    pub fn is_empty(&self) -> bool {
        self.locks_released == 0 && self.interactions_closed == 0
    }
}

pub struct Lifecycle {
    store: SharedStore,
}

impl Lifecycle {
    pub fn new(store: SharedStore) -> Self {
        Self { store }
    }

    pub fn reconcile(&self) -> Result<Reconciled> {
        let now = Utc::now();
        let mut out = Reconciled::default();

        let stale = self
            .store
            .with_named("reconcile.stale_locks", |db| db.locks().stale(now))?;
        for lock in stale {
            let released = self.store.with_named("reconcile.release_lock", |db| {
                db.locks().release(lock.session_id, lock.holder_id)
            });
            match released {
                Ok(true) => {
                    out.locks_released += 1;
                    tracing::info!(
                        target: "zlogic::engine",
                        session = %lock.session_id,
                        holder = lock.holder_kind.as_deref().unwrap_or("?"),
                        pid = lock.pid,
                        "released the session lock left by the previous run (heartbeat stopped)"
                    );
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(target: "zlogic::engine", "failed to release stale lock: {e}")
                }
            }
        }

        let orphans = self
            .store
            .with_named("reconcile.orphan_interactions", |db| {
                db.entries().unanswered_interactions()
            })?;
        for pending in orphans {
            let session_id = pending.request.session_id;
            let live = self
                .store
                .with_named("reconcile.session_lock", |db| db.locks().get(session_id));
            match live {
                Ok(Some(lock)) if !lock.is_stale_at(now) => continue,
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(target: "zlogic::engine", "could not look up session lock, skipping cleanup: {e}");
                    continue;
                }
            }

            let entry = zlogic_core::entry_data::interaction_response_entry(
                session_id,
                pending.request.turn_id,
                pending.request.turn_seq,
                &pending.interaction_id,
                &InteractionDecision::Cancelled,
            );
            let mut entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(target: "zlogic::engine", "could not build the confirmation-wrap-up entry: {e}");
                    continue;
                }
            };
            if let Some(obj) = entry.data.as_object_mut() {
                obj.insert("reason".into(), serde_json::json!(INTERRUPTED_AT_STARTUP));
            }
            let written = self.store.with_named("reconcile.close_interaction", |db| {
                db.entries().append(entry)
            });
            match written {
                Ok(_) => {
                    out.interactions_closed += 1;
                    tracing::info!(
                        target: "zlogic::engine",
                        session = %session_id,
                        interaction = %pending.interaction_id,
                        "wrapped up a confirmation box left by the previous run (nobody was waiting)"
                    );
                }
                Err(e) => {
                    tracing::warn!(target: "zlogic::engine", "failed to wrap up confirmation box: {e}")
                }
            }
        }

        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zlogic_core::CancellationToken;
    use zlogic_protocol::interaction::{GrantScope, InteractionBody};
    use zlogic_protocol::{SessionId, TurnId, WorkspaceId};
    use zlogic_store::{Db, NewSession};
    use zlogic_store::{EntryKind, NewEntry};

    fn setup() -> (SharedStore, SessionId) {
        let db = Db::open_in_memory().unwrap();
        let session = db
            .sessions()
            .create(NewSession::root(WorkspaceId::new()))
            .unwrap()
            .session_id;
        (SharedStore::new(db), session)
    }

    fn ask(store: &SharedStore, session: SessionId, turn: TurnId, id: &str) {
        store
            .with(|db| {
                db.entries().append(NewEntry::new(
                    session,
                    turn,
                    1,
                    EntryKind::InteractionRequest,
                    serde_json::json!({
                        "interaction_id": id,
                        "body": InteractionBody::Permission {
                            tool: "write_file".into(),
                            args_preview: "{}".into(),
                            reason: "test".into(),
                            caveats: Vec::new(),
                            offered_scopes: vec![GrantScope::Once],
                        grant_preview: None,
                        },
                    }),
                ))
            })
            .unwrap();
    }

    fn make_stale(store: &SharedStore, session: SessionId) {
        let back = Utc::now() - zlogic_store::lock::STALE_AFTER - chrono::Duration::seconds(5);
        store
            .with(|db| {
                db.conn().execute(
                    "UPDATE session_locks SET heartbeat_at = ?1 WHERE session_id = ?2",
                    rusqlite::params![back, session],
                )
            })
            .unwrap();
    }

    fn pending_count(store: &SharedStore, session: SessionId, turn: TurnId) -> usize {
        store
            .with(|db| db.entries().pending_interactions(session, turn))
            .unwrap()
            .len()
    }

    #[test]
    fn an_orphaned_prompt_gets_an_answer_nobody_has_to_give() {
        let (store, session) = setup();
        let turn = TurnId::new();
        ask(&store, session, turn, "i-1");
        assert_eq!(pending_count(&store, session, turn), 1);

        let out = Lifecycle::new(store.clone()).reconcile().unwrap();
        assert_eq!(out.interactions_closed, 1);
        assert_eq!(
            pending_count(&store, session, turn),
            0,
            "no longer awaiting an answer"
        );

        let response = store
            .with(|db| db.entries().list(session).unwrap())
            .into_iter()
            .find(|e| e.kind == EntryKind::InteractionResponse)
            .expect("a response was filled in");
        assert_eq!(response.data["decision"]["type"], "cancelled");
        assert_eq!(response.data["reason"], INTERRUPTED_AT_STARTUP);
        assert_eq!(response.turn_id, turn, "attached to the original turn");
    }

    #[tokio::test]
    async fn a_prompt_another_live_host_is_waiting_on_is_untouched() {
        let (store, session) = setup();
        let turn = TurnId::new();
        ask(&store, session, turn, "i-live");
        let locks = crate::SessionLocks::new(store.clone(), "other-host");
        let _held = locks
            .claim(session, turn, CancellationToken::new())
            .unwrap();

        let out = Lifecycle::new(store.clone()).reconcile().unwrap();
        assert_eq!(out, Reconciled::default(), "nothing should be done");
        assert_eq!(
            pending_count(&store, session, turn),
            1,
            "it is still waiting for an answer"
        );
    }

    #[tokio::test]
    async fn a_lock_whose_heartbeat_stopped_is_released() {
        let (store, session) = setup();
        let locks = crate::SessionLocks::new(store.clone(), "crashed");
        let guard = locks
            .claim(session, TurnId::new(), CancellationToken::new())
            .unwrap();
        std::mem::forget(guard);
        make_stale(&store, session);
        assert!(
            locks.live_turn(session).unwrap().is_some(),
            "the row is still there"
        );

        let out = Lifecycle::new(store.clone()).reconcile().unwrap();
        assert_eq!(out.locks_released, 1);
        assert_eq!(
            locks.live_turn(session).unwrap(),
            None,
            "no longer reported as running"
        );
    }

    #[tokio::test]
    async fn a_live_lock_is_left_alone() {
        let (store, session) = setup();
        let locks = crate::SessionLocks::new(store.clone(), "alive");
        let _guard = locks
            .claim(session, TurnId::new(), CancellationToken::new())
            .unwrap();

        assert_eq!(
            Lifecycle::new(store.clone())
                .reconcile()
                .unwrap()
                .locks_released,
            0
        );
        assert!(locks.live_turn(session).unwrap().is_some());
    }

    #[tokio::test]
    async fn a_crashed_session_gets_both_halves_cleaned_in_one_pass() {
        let (store, session) = setup();
        let turn = TurnId::new();
        let locks = crate::SessionLocks::new(store.clone(), "crashed");
        let guard = locks
            .claim(session, turn, CancellationToken::new())
            .unwrap();
        std::mem::forget(guard);
        make_stale(&store, session);
        ask(&store, session, turn, "i-2");

        let out = Lifecycle::new(store.clone()).reconcile().unwrap();
        assert_eq!(
            out,
            Reconciled {
                locks_released: 1,
                interactions_closed: 1
            }
        );
        assert_eq!(pending_count(&store, session, turn), 0);
        assert_eq!(locks.live_turn(session).unwrap(), None);
    }

    #[test]
    fn running_twice_changes_nothing_the_second_time() {
        let (store, session) = setup();
        let turn = TurnId::new();
        ask(&store, session, turn, "i-3");

        let life = Lifecycle::new(store.clone());
        assert_eq!(life.reconcile().unwrap().interactions_closed, 1);
        assert!(
            life.reconcile().unwrap().is_empty(),
            "there is nothing left to reconcile the second time"
        );

        let responses = store
            .with(|db| db.entries().list(session).unwrap())
            .into_iter()
            .filter(|e| e.kind == EntryKind::InteractionResponse)
            .count();
        assert_eq!(responses, 1, "must not fill in a second one");
    }

    #[test]
    fn an_already_answered_prompt_is_not_touched() {
        let (store, session) = setup();
        let turn = TurnId::new();
        ask(&store, session, turn, "i-4");
        store
            .with(|db| {
                db.entries().append(NewEntry::new(
                    session,
                    turn,
                    1,
                    EntryKind::InteractionResponse,
                    serde_json::json!({
                        "interaction_id": "i-4",
                        "decision": InteractionDecision::Deny { reason: None },
                    }),
                ))
            })
            .unwrap();

        assert!(
            Lifecycle::new(store.clone())
                .reconcile()
                .unwrap()
                .is_empty()
        );
        let denied = store
            .with(|db| db.entries().list(session).unwrap())
            .into_iter()
            .filter(|e| e.kind == EntryKind::InteractionResponse)
            .count();
        assert_eq!(denied, 1, "the user's denial is still the only response");
    }

    #[test]
    fn a_clean_database_needs_nothing() {
        let (store, _session) = setup();
        assert!(Lifecycle::new(store).reconcile().unwrap().is_empty());
    }
}
