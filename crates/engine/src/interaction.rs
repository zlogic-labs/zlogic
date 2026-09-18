use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use tokio::sync::oneshot;
use zlogic_protocol::interaction::{InteractionDecision, InteractionPort, InteractionRequest};
use zlogic_protocol::stream::{StateChange, StateNotice};
use zlogic_protocol::{SessionId, TurnId};

use crate::hub::EventHub;
use crate::{EngineError, Result};

struct Waiting {
    session_id: SessionId,
    turn_id: TurnId,
    answer: oneshot::Sender<InteractionDecision>,
}

pub struct EngineInteractions {
    hub: Arc<EventHub>,
    waiting: Mutex<HashMap<String, Waiting>>,
}

pub trait InteractionRouter: Send + Sync {
    fn answer(&self, interaction_id: &str, decision: InteractionDecision) -> Result<()>;

    fn pending(&self, session_id: SessionId) -> Vec<String>;
}

impl EngineInteractions {
    pub fn new(hub: Arc<EventHub>) -> Self {
        Self {
            hub,
            waiting: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Waiting>> {
        self.waiting.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[async_trait]
impl InteractionPort for EngineInteractions {
    async fn ask(
        &self,
        req: InteractionRequest,
    ) -> std::result::Result<InteractionDecision, String> {
        let interaction_id = req.interaction_id.clone();

        let (tx, rx) = oneshot::channel();
        self.lock().insert(
            interaction_id.clone(),
            Waiting {
                session_id: req.session_id,
                turn_id: req.turn_id,
                answer: tx,
            },
        );

        self.hub.notify(StateNotice {
            session_id: req.session_id.to_string(),
            turn_id: Some(req.turn_id.to_string()),
            change: StateChange::InteractionPending,
        });

        let decision = match rx.await {
            Ok(d) => d,
            Err(_) => InteractionDecision::Cancelled,
        };
        self.lock().remove(&interaction_id);

        self.hub.notify(StateNotice {
            session_id: req.session_id.to_string(),
            turn_id: Some(req.turn_id.to_string()),
            change: StateChange::InteractionResolved,
        });

        Ok(decision)
    }
}

impl InteractionRouter for EngineInteractions {
    fn answer(&self, interaction_id: &str, decision: InteractionDecision) -> Result<()> {
        let waiting = self
            .lock()
            .remove(interaction_id)
            .ok_or_else(|| EngineError::NotFound(format!("interaction {interaction_id}")))?;

        waiting.answer.send(decision).map_err(|_| {
            EngineError::Invalid(format!(
                "the waiter for interaction {interaction_id} has already finished (session {}, turn {})",
                waiting.session_id, waiting.turn_id
            ))
        })
    }

    fn pending(&self, session_id: SessionId) -> Vec<String> {
        self.lock()
            .iter()
            .filter(|(_, w)| w.session_id == session_id)
            .map(|(id, _)| id.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zlogic_core::SharedStore;
    use zlogic_protocol::interaction::{Form, GrantScope, InteractionBody};
    use zlogic_protocol::{CallId, EntryId, WorkspaceId};
    use zlogic_store::{Db, EntryKind, NewEntry, NewSession};

    fn setup() -> (Arc<EngineInteractions>, SharedStore, SessionId) {
        let db = Db::open_in_memory().unwrap();
        let session = db
            .sessions()
            .create(NewSession::root(WorkspaceId::new()))
            .unwrap()
            .session_id;
        let store = SharedStore::new(db);
        let it = Arc::new(EngineInteractions::new(Arc::new(EventHub::new())));
        (it, store, session)
    }

    fn request(session: SessionId, turn: TurnId) -> InteractionRequest {
        InteractionRequest {
            interaction_id: EntryId::new().to_string(),
            session_id: session,
            turn_id: turn,
            call_id: Some(CallId::new("call_1")),
            body: InteractionBody::Permission {
                tool: "write_file".into(),
                args_preview: "{}".into(),
                reason: "test".into(),
                caveats: Vec::new(),
                offered_scopes: vec![GrantScope::Once, GrantScope::Session],
                grant_preview: None,
            },
        }
    }

    fn kinds(store: &SharedStore, session: SessionId) -> Vec<EntryKind> {
        store
            .with(|db| db.entries().list(session).unwrap())
            .iter()
            .map(|e| e.kind)
            .collect()
    }

    #[tokio::test]
    async fn routing_an_answer_does_not_write_an_entry() {
        let (it, store, session) = setup();
        let turn = TurnId::new();

        store
            .with(|db| {
                db.entries().append(NewEntry::new(
                    session,
                    turn,
                    1,
                    EntryKind::User,
                    serde_json::json!({ "type": "text", "text": "go" }),
                ))
            })
            .unwrap();

        let asker = it.clone();
        let handle = tokio::spawn(async move { asker.ask(request(session, turn)).await });

        let id = loop {
            let pending = it.pending(session);
            if let Some(id) = pending.first() {
                break id.clone();
            }
            tokio::task::yield_now().await;
        };

        it.answer(
            &id,
            InteractionDecision::Allow {
                scope: GrantScope::Session,
                source: None,
            },
        )
        .unwrap();
        let decision = handle.await.unwrap().unwrap();
        assert!(matches!(decision, InteractionDecision::Allow { .. }));

        assert_eq!(kinds(&store, session), [EntryKind::User]);
        assert!(it.pending(session).is_empty());
        assert!(
            store
                .with(|db| db.entries().pending_interactions(session, turn).unwrap())
                .is_empty()
        );
    }

    #[tokio::test]
    async fn answering_an_unknown_interaction_is_an_error() {
        let (it, _store, session) = setup();
        assert!(matches!(
            it.answer("nope", InteractionDecision::Cancelled),
            Err(EngineError::NotFound(_))
        ));
        assert!(it.pending(session).is_empty());
    }

    #[tokio::test]
    async fn answering_twice_fails_the_second_time() {
        let (it, _store, session) = setup();
        let turn = TurnId::new();
        let asker = it.clone();
        let handle = tokio::spawn(async move { asker.ask(request(session, turn)).await });
        let id = loop {
            if let Some(id) = it.pending(session).first() {
                break id.clone();
            }
            tokio::task::yield_now().await;
        };

        assert!(it.answer(&id, InteractionDecision::Cancelled).is_ok());
        assert!(it.answer(&id, InteractionDecision::Cancelled).is_err());
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_dropped_waiter_is_never_treated_as_consent() {
        let (it, _store, session) = setup();
        let turn = TurnId::new();

        let asker = it.clone();
        let handle = tokio::spawn(async move { asker.ask(request(session, turn)).await });
        let id = loop {
            if let Some(id) = it.pending(session).first() {
                break id.clone();
            }
            tokio::task::yield_now().await;
        };

        handle.abort();
        let _ = handle.await;

        let err = it.answer(
            &id,
            InteractionDecision::Allow {
                scope: GrantScope::Once,
                source: None,
            },
        );
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn a_form_uses_the_same_channel() {
        let (it, _store, session) = setup();
        let turn = TurnId::new();
        let mut req = request(session, turn);
        req.body = InteractionBody::Form(Form::confirm("Proceed?", "40 files change."));

        let asker = it.clone();
        let handle = tokio::spawn(async move { asker.ask(req).await });
        let id = loop {
            if let Some(id) = it.pending(session).first() {
                break id.clone();
            }
            tokio::task::yield_now().await;
        };
        it.answer(&id, InteractionDecision::Submitted(Default::default()))
            .unwrap();
        assert!(matches!(
            handle.await.unwrap().unwrap(),
            InteractionDecision::Submitted(_)
        ));

        assert!(it.pending(session).is_empty());
    }

    #[tokio::test]
    async fn pending_is_scoped_to_its_session() {
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
        let it = Arc::new(EngineInteractions::new(Arc::new(EventHub::new())));

        for session in [a, b] {
            let asker = it.clone();
            tokio::spawn(async move { asker.ask(request(session, TurnId::new())).await });
        }
        while it.pending(a).is_empty() || it.pending(b).is_empty() {
            tokio::task::yield_now().await;
        }
        assert_eq!(it.pending(a).len(), 1);
        assert_eq!(it.pending(b).len(), 1);
    }
}
