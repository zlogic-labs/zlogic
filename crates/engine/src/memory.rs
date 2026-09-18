//! Durable memory service and the host behind `memory_update`.

use async_trait::async_trait;
use std::sync::Arc;
use zlogic_core::SharedStore;
use zlogic_objects::ObjectStore;
use zlogic_protocol::query::{ApiError, ApiResult};
use zlogic_protocol::{
    MemoryAddReq, MemoryCategory, MemoryEditReq, MemoryId, MemoryListReq, MemoryRecord,
    MemoryRemoveReq, MemoryScope, MemoryUndoReq, SessionId, TurnId,
};
use zlogic_tools::MemoryHost;

use crate::service::MemoryService;

pub struct Memories {
    store: SharedStore,
    objects: Arc<dyn ObjectStore>,
}

impl Memories {
    pub fn new(store: SharedStore, objects: Arc<dyn ObjectStore>) -> Self {
        Self { store, objects }
    }

    fn source_is_user_text(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        source_quote: &str,
    ) -> std::result::Result<(), String> {
        let found = self
            .store
            .with(|db| {
                db.entries().user_turn_contains(
                    session_id,
                    turn_id,
                    source_quote,
                    self.objects.as_ref(),
                )
            })
            .map_err(|e| e.to_string())?;
        if found {
            Ok(())
        } else {
            Err("source_quote must be an exact quote from user input in the current turn".into())
        }
    }
}

#[async_trait]
impl MemoryHost for Memories {
    async fn get(&self, memory_id: MemoryId) -> std::result::Result<MemoryRecord, String> {
        self.store
            .with(|db| db.memories().get(memory_id))
            .map_err(|e| e.to_string())
    }

    async fn add(
        &self,
        scope: MemoryScope,
        category: MemoryCategory,
        fact: String,
        source_quote: String,
        session_id: SessionId,
        turn_id: TurnId,
    ) -> std::result::Result<MemoryRecord, String> {
        self.source_is_user_text(session_id, turn_id, &source_quote)?;
        let workspace_id = match scope {
            MemoryScope::Global => None,
            MemoryScope::Workspace => Some(
                self.store
                    .with(|db| db.sessions().get(session_id))
                    .map_err(|e| e.to_string())?
                    .workspace_id,
            ),
        };
        self.store
            .with(|db| {
                db.memories().add(MemoryAddReq {
                    scope,
                    workspace_id,
                    category,
                    fact,
                    source_quote,
                    source_session_id: Some(session_id),
                    source_turn_id: Some(turn_id),
                })
            })
            .map_err(|e| e.to_string())
    }

    async fn update(
        &self,
        memory_id: MemoryId,
        category: MemoryCategory,
        fact: String,
        source_quote: String,
        session_id: SessionId,
        turn_id: TurnId,
    ) -> std::result::Result<MemoryRecord, String> {
        self.source_is_user_text(session_id, turn_id, &source_quote)?;
        self.store
            .with(|db| {
                db.memories().update(MemoryEditReq {
                    memory_id,
                    category,
                    fact,
                    source_quote,
                    source_session_id: Some(session_id),
                    source_turn_id: Some(turn_id),
                })
            })
            .map_err(|e| e.to_string())
    }

    async fn remove(
        &self,
        memory_id: MemoryId,
        source_quote: String,
        session_id: SessionId,
        turn_id: TurnId,
    ) -> std::result::Result<MemoryRecord, String> {
        self.source_is_user_text(session_id, turn_id, &source_quote)?;
        self.store
            .with(|db| {
                db.memories().remove(MemoryRemoveReq {
                    memory_id,
                    source_session_id: Some(session_id),
                    source_turn_id: Some(turn_id),
                })
            })
            .map_err(|e| e.to_string())
    }
}

#[async_trait]
impl MemoryService for Memories {
    async fn list(&self, req: MemoryListReq) -> ApiResult<Vec<MemoryRecord>> {
        self.store
            .with(|db| db.memories().list(&req))
            .map_err(|e| ApiError::from(crate::EngineError::from(e)))
    }

    async fn add(&self, req: MemoryAddReq) -> ApiResult<MemoryRecord> {
        self.store
            .with(|db| db.memories().add(req))
            .map_err(|e| ApiError::from(crate::EngineError::from(e)))
    }

    async fn update(&self, req: MemoryEditReq) -> ApiResult<MemoryRecord> {
        self.store
            .with(|db| db.memories().update(req))
            .map_err(|e| ApiError::from(crate::EngineError::from(e)))
    }

    async fn remove(&self, req: MemoryRemoveReq) -> ApiResult<MemoryRecord> {
        self.store
            .with(|db| db.memories().remove(req))
            .map_err(|e| ApiError::from(crate::EngineError::from(e)))
    }

    async fn undo(&self, req: MemoryUndoReq) -> ApiResult<Option<MemoryRecord>> {
        self.store
            .with(|db| db.memories().undo_last(req))
            .map_err(|e| ApiError::from(crate::EngineError::from(e)))
    }
}
