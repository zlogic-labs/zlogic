//! Durable user memories and their append-only audit log.

use rusqlite::{Connection, OptionalExtension, named_params};
use zlogic_protocol::{
    MemoryAddReq, MemoryEditReq, MemoryEventId, MemoryId, MemoryListReq, MemoryRecord,
    MemoryRemoveReq, MemoryScope, MemoryStatus, MemoryUndoReq,
};

use crate::{Json, Result, StoreError, now};

pub struct MemoryStore<'a> {
    conn: &'a Connection,
}

impl<'a> MemoryStore<'a> {
    pub(crate) fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub fn list(&self, req: &MemoryListReq) -> Result<Vec<MemoryRecord>> {
        validate_scope(req.scope, req.workspace_id)?;
        let mut sql = format!(
            "SELECT {COLS} FROM memory
             WHERE scope = :scope
               AND ((:workspace_id IS NULL AND workspace_id IS NULL) OR workspace_id = :workspace_id)"
        );
        if !req.include_removed {
            sql.push_str(" AND status = 'active'");
        }
        sql.push_str(" ORDER BY updated_at DESC, id DESC");
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map(
            named_params! {
                ":scope": req.scope,
                ":workspace_id": req.workspace_id,
            },
            row,
        )?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn get(&self, memory_id: MemoryId) -> Result<MemoryRecord> {
        self.find(memory_id)?.ok_or_else(|| StoreError::NotFound {
            kind: "memory",
            id: memory_id.to_string(),
        })
    }

    pub fn find(&self, memory_id: MemoryId) -> Result<Option<MemoryRecord>> {
        self.conn
            .query_row(
                &format!("SELECT {COLS} FROM memory WHERE memory_id = :memory_id"),
                named_params! { ":memory_id": memory_id },
                row,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Adds a memory, or refreshes the exact same active fact in the same scope/category.
    pub fn add(&self, req: MemoryAddReq) -> Result<MemoryRecord> {
        validate_source_pair(req.source_session_id, req.source_turn_id)?;
        validate_write(
            req.scope,
            req.workspace_id,
            req.fact.as_str(),
            req.source_quote.as_str(),
        )?;
        let tx = self.conn.unchecked_transaction()?;
        let candidates = {
            let mut statement = tx.prepare(
                "SELECT memory_id, fact FROM memory
                 WHERE scope = :scope
                   AND ((:workspace_id IS NULL AND workspace_id IS NULL) OR workspace_id = :workspace_id)
                   AND category = :category
                   AND status = 'active'
                 ORDER BY id DESC",
            )?;
            let rows = statement.query_map(
                named_params! {
                    ":scope": req.scope,
                    ":workspace_id": req.workspace_id,
                    ":category": req.category,
                },
                |r| Ok((r.get::<_, MemoryId>(0)?, r.get::<_, String>(1)?)),
            )?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        let normalized = normalize_fact(&req.fact);
        let duplicate = candidates
            .into_iter()
            .find_map(|(id, fact)| (normalize_fact(&fact) == normalized).then_some(id));

        let record = match duplicate {
            Some(memory_id) => {
                let before = get_conn(&tx, memory_id)?;
                let at = now();
                tx.execute(
                    "UPDATE memory
                     SET source_quote = :source_quote,
                         source_session_id = :source_session_id,
                         source_turn_id = :source_turn_id,
                         updated_at = :updated_at
                     WHERE memory_id = :memory_id",
                    named_params! {
                        ":source_quote": req.source_quote,
                        ":source_session_id": req.source_session_id,
                        ":source_turn_id": req.source_turn_id,
                        ":updated_at": at,
                        ":memory_id": memory_id,
                    },
                )?;
                let after = get_conn(&tx, memory_id)?;
                append_event(
                    &tx,
                    memory_id,
                    "update",
                    Some(&before),
                    Some(&after),
                    req.source_session_id,
                    req.source_turn_id,
                    None,
                )?;
                after
            }
            None => {
                let memory_id = MemoryId::new();
                let at = now();
                tx.execute(
                    "INSERT INTO memory (
                       memory_id, scope, workspace_id, category, fact, source_quote,
                       source_session_id, source_turn_id, status, created_at, updated_at
                     ) VALUES (
                       :memory_id, :scope, :workspace_id, :category, :fact, :source_quote,
                       :source_session_id, :source_turn_id, 'active', :created_at, :updated_at
                     )",
                    named_params! {
                        ":memory_id": memory_id,
                        ":scope": req.scope,
                        ":workspace_id": req.workspace_id,
                        ":category": req.category,
                        ":fact": req.fact.trim(),
                        ":source_quote": req.source_quote.trim(),
                        ":source_session_id": req.source_session_id,
                        ":source_turn_id": req.source_turn_id,
                        ":created_at": at,
                        ":updated_at": at,
                    },
                )?;
                let after = get_conn(&tx, memory_id)?;
                append_event(
                    &tx,
                    memory_id,
                    "add",
                    None,
                    Some(&after),
                    req.source_session_id,
                    req.source_turn_id,
                    None,
                )?;
                after
            }
        };
        tx.commit()?;
        Ok(record)
    }

    pub fn update(&self, req: MemoryEditReq) -> Result<MemoryRecord> {
        validate_content(&req.fact, &req.source_quote)?;
        let tx = self.conn.unchecked_transaction()?;
        let before = get_conn(&tx, req.memory_id)?;
        if before.status != MemoryStatus::Active {
            return Err(StoreError::Corrupt(format!(
                "cannot update removed memory {}",
                req.memory_id
            )));
        }
        // UI edits have no session/turn: the edited text is a new direct user assertion, so it
        // must not keep pointing at an older conversation quote. Model writes supply both ids.
        validate_source_pair(req.source_session_id, req.source_turn_id)?;
        let source_session_id = req.source_session_id;
        let source_turn_id = req.source_turn_id;
        tx.execute(
            "UPDATE memory
             SET category = :category, fact = :fact, source_quote = :source_quote,
                 source_session_id = :source_session_id, source_turn_id = :source_turn_id,
                 updated_at = :updated_at
             WHERE memory_id = :memory_id",
            named_params! {
                ":category": req.category,
                ":fact": req.fact.trim(),
                ":source_quote": req.source_quote.trim(),
                ":source_session_id": source_session_id,
                ":source_turn_id": source_turn_id,
                ":updated_at": now(),
                ":memory_id": req.memory_id,
            },
        )?;
        let after = get_conn(&tx, req.memory_id)?;
        append_event(
            &tx,
            req.memory_id,
            "update",
            Some(&before),
            Some(&after),
            req.source_session_id,
            req.source_turn_id,
            None,
        )?;
        tx.commit()?;
        Ok(after)
    }

    pub fn remove(&self, req: MemoryRemoveReq) -> Result<MemoryRecord> {
        let tx = self.conn.unchecked_transaction()?;
        let before = get_conn(&tx, req.memory_id)?;
        if before.status == MemoryStatus::Removed {
            tx.commit()?;
            return Ok(before);
        }
        tx.execute(
            "UPDATE memory
             SET status = 'removed', updated_at = :updated_at
             WHERE memory_id = :memory_id",
            named_params! {
                ":updated_at": now(),
                ":memory_id": req.memory_id,
            },
        )?;
        let after = get_conn(&tx, req.memory_id)?;
        append_event(
            &tx,
            req.memory_id,
            "remove",
            Some(&before),
            Some(&after),
            req.source_session_id,
            req.source_turn_id,
            None,
        )?;
        tx.commit()?;
        Ok(after)
    }

    /// Reverses the latest not-yet-undone event in one scope by appending an `undo` event.
    pub fn undo_last(&self, req: MemoryUndoReq) -> Result<Option<MemoryRecord>> {
        validate_scope(req.scope, req.workspace_id)?;
        let tx = self.conn.unchecked_transaction()?;
        let event: Option<(MemoryEventId, String, Option<Json<MemoryRecord>>)> = tx
            .query_row(
                "SELECT e.event_id, e.action, e.before_json
                 FROM memory_events e
                 JOIN memory m ON m.memory_id = e.memory_id
                 WHERE e.action != 'undo'
                   AND m.scope = :scope
                   AND ((:workspace_id IS NULL AND m.workspace_id IS NULL)
                        OR m.workspace_id = :workspace_id)
                   AND NOT EXISTS (
                     SELECT 1 FROM memory_events u
                     WHERE u.action = 'undo' AND u.reverts_event_id = e.event_id
                   )
                 ORDER BY e.id DESC
                 LIMIT 1",
                named_params! {
                    ":scope": req.scope,
                    ":workspace_id": req.workspace_id,
                },
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((event_id, action, before)) = event else {
            tx.commit()?;
            return Ok(None);
        };

        let memory_id: MemoryId = tx.query_row(
            "SELECT memory_id FROM memory_events WHERE event_id = :event_id",
            named_params! { ":event_id": event_id },
            |r| r.get(0),
        )?;
        let current = get_conn(&tx, memory_id)?;
        let mut restored = match (action.as_str(), before) {
            ("add", None) => MemoryRecord {
                status: MemoryStatus::Removed,
                ..current.clone()
            },
            ("update" | "remove", Some(Json(before))) => before,
            _ => {
                return Err(StoreError::Corrupt(format!(
                    "memory event {event_id} has invalid undo snapshot"
                )));
            }
        };
        restored.updated_at = now();
        write_snapshot(&tx, &restored)?;
        append_event(
            &tx,
            memory_id,
            "undo",
            Some(&current),
            Some(&restored),
            req.source_session_id,
            req.source_turn_id,
            Some(event_id),
        )?;
        tx.commit()?;
        Ok(Some(restored))
    }
}

const COLS: &str = "memory_id, scope, workspace_id, category, fact, source_quote,
                     source_session_id, source_turn_id, status, created_at, updated_at";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryRecord> {
    Ok(MemoryRecord {
        memory_id: r.get("memory_id")?,
        scope: r.get("scope")?,
        workspace_id: r.get("workspace_id")?,
        category: r.get("category")?,
        fact: r.get("fact")?,
        source_quote: r.get("source_quote")?,
        source_session_id: r.get("source_session_id")?,
        source_turn_id: r.get("source_turn_id")?,
        status: r.get("status")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
    })
}

fn get_conn(conn: &Connection, memory_id: MemoryId) -> Result<MemoryRecord> {
    conn.query_row(
        &format!("SELECT {COLS} FROM memory WHERE memory_id = :memory_id"),
        named_params! { ":memory_id": memory_id },
        row,
    )
    .optional()?
    .ok_or_else(|| StoreError::NotFound {
        kind: "memory",
        id: memory_id.to_string(),
    })
}

fn append_event(
    conn: &Connection,
    memory_id: MemoryId,
    action: &str,
    before: Option<&MemoryRecord>,
    after: Option<&MemoryRecord>,
    source_session_id: Option<zlogic_protocol::SessionId>,
    source_turn_id: Option<zlogic_protocol::TurnId>,
    reverts_event_id: Option<MemoryEventId>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO memory_events (
           event_id, memory_id, action, before_json, after_json,
           source_session_id, source_turn_id, reverts_event_id, created_at
         ) VALUES (
           :event_id, :memory_id, :action, :before_json, :after_json,
           :source_session_id, :source_turn_id, :reverts_event_id, :created_at
         )",
        named_params! {
            ":event_id": MemoryEventId::new(),
            ":memory_id": memory_id,
            ":action": action,
            ":before_json": before.map(|v| Json(v.clone())),
            ":after_json": after.map(|v| Json(v.clone())),
            ":source_session_id": source_session_id,
            ":source_turn_id": source_turn_id,
            ":reverts_event_id": reverts_event_id,
            ":created_at": now(),
        },
    )?;
    Ok(())
}

fn write_snapshot(conn: &Connection, record: &MemoryRecord) -> Result<()> {
    conn.execute(
        "UPDATE memory SET
           scope = :scope, workspace_id = :workspace_id, category = :category,
           fact = :fact, source_quote = :source_quote,
           source_session_id = :source_session_id, source_turn_id = :source_turn_id,
           status = :status, created_at = :created_at, updated_at = :updated_at
         WHERE memory_id = :memory_id",
        named_params! {
            ":scope": record.scope,
            ":workspace_id": record.workspace_id,
            ":category": record.category,
            ":fact": record.fact,
            ":source_quote": record.source_quote,
            ":source_session_id": record.source_session_id,
            ":source_turn_id": record.source_turn_id,
            ":status": record.status,
            ":created_at": record.created_at,
            ":updated_at": record.updated_at,
            ":memory_id": record.memory_id,
        },
    )?;
    Ok(())
}

fn validate_write(
    scope: MemoryScope,
    workspace_id: Option<zlogic_protocol::WorkspaceId>,
    fact: &str,
    source_quote: &str,
) -> Result<()> {
    validate_scope(scope, workspace_id)?;
    validate_content(fact, source_quote)
}

fn validate_content(fact: &str, source_quote: &str) -> Result<()> {
    if fact.trim().is_empty() || source_quote.trim().chars().count() < 2 {
        return Err(StoreError::Corrupt(
            "memory fact must not be empty and source_quote must contain at least 2 characters"
                .into(),
        ));
    }
    Ok(())
}

fn validate_source_pair(
    source_session_id: Option<zlogic_protocol::SessionId>,
    source_turn_id: Option<zlogic_protocol::TurnId>,
) -> Result<()> {
    if source_session_id.is_some() == source_turn_id.is_some() {
        Ok(())
    } else {
        Err(StoreError::Corrupt(
            "memory source_session_id and source_turn_id must be supplied together".into(),
        ))
    }
}

/// Conservative near-duplicate key: ignore casing, spacing and punctuation, but never attempt
/// semantic guesses that could merge two different user statements.
fn normalize_fact(fact: &str) -> String {
    let normalized = fact
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    if normalized.is_empty() {
        fact.trim().to_lowercase()
    } else {
        normalized
    }
}

fn validate_scope(
    scope: MemoryScope,
    workspace_id: Option<zlogic_protocol::WorkspaceId>,
) -> Result<()> {
    match (scope, workspace_id) {
        (MemoryScope::Global, None) | (MemoryScope::Workspace, Some(_)) => Ok(()),
        (MemoryScope::Global, Some(_)) => Err(StoreError::Corrupt(
            "global memory must not have workspace_id".into(),
        )),
        (MemoryScope::Workspace, None) => Err(StoreError::Corrupt(
            "workspace memory requires workspace_id".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Db, EntryKind, NewEntry, NewSession};
    use zlogic_objects::MemoryObjectStore;
    use zlogic_protocol::{MemoryCategory, SessionId, TurnId, WorkspaceId};

    fn add(scope: MemoryScope, workspace_id: Option<WorkspaceId>, fact: &str) -> MemoryAddReq {
        MemoryAddReq {
            scope,
            workspace_id,
            category: MemoryCategory::Preference,
            fact: fact.into(),
            source_quote: format!("please remember: {fact}"),
            source_session_id: Some(SessionId::new()),
            source_turn_id: Some(TurnId::new()),
        }
    }

    #[test]
    fn stores_scoped_memory_and_audit_event_atomically() {
        let db = Db::open_in_memory().unwrap();
        let ws = WorkspaceId::new();
        let record = db
            .memories()
            .add(add(MemoryScope::Workspace, Some(ws), "use concise replies"))
            .unwrap();

        assert_eq!(record.workspace_id, Some(ws));
        assert_eq!(
            db.memories()
                .list(&MemoryListReq {
                    scope: MemoryScope::Workspace,
                    workspace_id: Some(ws),
                    include_removed: false,
                })
                .unwrap(),
            vec![record.clone()]
        );
        let events: i64 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM memory_events WHERE memory_id = ?1",
                [record.memory_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(events, 1);
    }

    #[test]
    fn normalized_duplicate_refreshes_instead_of_adding_a_second_row() {
        let db = Db::open_in_memory().unwrap();
        let ws = WorkspaceId::new();
        let first = db
            .memories()
            .add(add(MemoryScope::Workspace, Some(ws), "Use concise replies"))
            .unwrap();
        let second = db
            .memories()
            .add(add(
                MemoryScope::Workspace,
                Some(ws),
                " USE concise replies! ",
            ))
            .unwrap();
        assert_eq!(first.memory_id, second.memory_id);
        let count: i64 = db
            .conn()
            .query_row("SELECT count(*) FROM memory", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn remove_is_soft_and_hidden_from_active_list() {
        let db = Db::open_in_memory().unwrap();
        let record = db
            .memories()
            .add(add(MemoryScope::Global, None, "prefer Rust"))
            .unwrap();
        let removed = db
            .memories()
            .remove(MemoryRemoveReq {
                memory_id: record.memory_id,
                source_session_id: Some(SessionId::new()),
                source_turn_id: Some(TurnId::new()),
            })
            .unwrap();
        assert_eq!(removed.status, MemoryStatus::Removed);
        assert_eq!(removed.source_session_id, record.source_session_id);
        assert_eq!(removed.source_turn_id, record.source_turn_id);
        assert!(
            db.memories()
                .list(&MemoryListReq {
                    scope: MemoryScope::Global,
                    workspace_id: None,
                    include_removed: false,
                })
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn provenance_accepts_user_text_but_not_assistant_text() {
        let db = Db::open_in_memory().unwrap();
        let objects = MemoryObjectStore::new();
        let session = db
            .sessions()
            .create(NewSession::root(WorkspaceId::new()))
            .unwrap()
            .session_id;
        let turn = TurnId::new();
        db.entries()
            .append(NewEntry::new(
                session,
                turn,
                1,
                EntryKind::User,
                serde_json::json!({ "type": "text", "text": "remember blue" }),
            ))
            .unwrap();
        db.entries()
            .append(NewEntry::new(
                session,
                turn,
                1,
                EntryKind::AssistantText,
                serde_json::json!({ "type": "text", "text": "invented red" }),
            ))
            .unwrap();

        assert!(
            db.entries()
                .user_turn_contains(session, turn, "remember blue", &objects)
                .unwrap()
        );
        assert!(
            !db.entries()
                .user_turn_contains(session, turn, "invented red", &objects)
                .unwrap()
        );
        // Structural metadata is not user-authored text and must never satisfy provenance.
        assert!(
            !db.entries()
                .user_turn_contains(session, turn, "text", &objects)
                .unwrap()
        );
    }

    #[test]
    fn provenance_reads_user_text_offloaded_to_the_object_store() {
        let db = Db::open_in_memory().unwrap();
        let objects = MemoryObjectStore::new();
        let session = db
            .sessions()
            .create(NewSession::root(WorkspaceId::new()))
            .unwrap()
            .session_id;
        let turn = TurnId::new();
        let quote = "remember this durable preference";
        let text = format!("{} {quote}", "x".repeat(crate::entry::INLINE_LIMIT));
        let record = db
            .entries()
            .append_with_offload(
                NewEntry::new(
                    session,
                    turn,
                    1,
                    EntryKind::User,
                    serde_json::json!({ "type": "text", "text": text }),
                ),
                &objects,
            )
            .unwrap();

        assert!(record.is_offloaded());
        assert!(
            db.entries()
                .user_turn_contains(session, turn, quote, &objects)
                .unwrap()
        );
    }

    #[test]
    fn manual_updates_reject_an_unusable_source_quote() {
        let db = Db::open_in_memory().unwrap();
        let record = db
            .memories()
            .add(add(MemoryScope::Global, None, "prefer Rust"))
            .unwrap();

        let error = db
            .memories()
            .update(MemoryEditReq {
                memory_id: record.memory_id,
                category: MemoryCategory::Preference,
                fact: "prefer examples".into(),
                source_quote: "x".into(),
                source_session_id: None,
                source_turn_id: None,
            })
            .unwrap_err();
        assert!(error.to_string().contains("at least 2 characters"));
    }

    #[test]
    fn manual_updates_replace_the_quote_and_clear_conversation_provenance() {
        let db = Db::open_in_memory().unwrap();
        let record = db
            .memories()
            .add(add(MemoryScope::Global, None, "prefer Rust"))
            .unwrap();
        assert!(record.source_session_id.is_some());

        let updated = db
            .memories()
            .update(MemoryEditReq {
                memory_id: record.memory_id,
                category: MemoryCategory::Preference,
                fact: "prefer TypeScript examples".into(),
                source_quote: "prefer TypeScript examples".into(),
                source_session_id: None,
                source_turn_id: None,
            })
            .unwrap();

        assert_eq!(updated.source_quote, "prefer TypeScript examples");
        assert_eq!(updated.source_session_id, None);
        assert_eq!(updated.source_turn_id, None);
    }

    #[test]
    fn memory_sources_are_either_fully_attributed_or_user_managed() {
        let db = Db::open_in_memory().unwrap();
        let mut req = add(MemoryScope::Global, None, "prefer Rust");
        req.source_turn_id = None;

        let error = db.memories().add(req).unwrap_err();
        assert!(error.to_string().contains("must be supplied together"));
    }

    #[test]
    fn undo_reverses_remove_then_add_without_deleting_history() {
        let db = Db::open_in_memory().unwrap();
        let session = SessionId::new();
        let turn = TurnId::new();
        let record = db
            .memories()
            .add(add(MemoryScope::Global, None, "prefer Rust"))
            .unwrap();
        db.memories()
            .remove(MemoryRemoveReq {
                memory_id: record.memory_id,
                source_session_id: Some(session),
                source_turn_id: Some(turn),
            })
            .unwrap();
        let req = || MemoryUndoReq {
            scope: MemoryScope::Global,
            workspace_id: None,
            source_session_id: None,
            source_turn_id: None,
        };

        let restored = db.memories().undo_last(req()).unwrap().unwrap();
        assert_eq!(restored.status, MemoryStatus::Active);
        let removed_again = db.memories().undo_last(req()).unwrap().unwrap();
        assert_eq!(removed_again.status, MemoryStatus::Removed);
        assert!(db.memories().undo_last(req()).unwrap().is_none());

        let events: i64 = db
            .conn()
            .query_row("SELECT count(*) FROM memory_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(events, 4, "add + remove + two undo events");
    }
}
