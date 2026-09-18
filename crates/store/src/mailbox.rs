//! The `mailbox` table — the single antechamber for user input.
//! # One submission is only ever in one place
//! Undelivered: here. Delivered: in `session_entry`. **Delivery moves it; it does not copy
//! it and flip a flag.**
//! That is why there is no `delivered` column. With one, you eventually get "entry written,
//! flag not yet updated" (the message is delivered twice) or "flag updated, entry write
//! failed" (the message is lost). A move inside one transaction makes both states
//! unrepresentable.

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, named_params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use zlogic_protocol::{SessionId, SubmissionId, define_enum_wire, impl_enum_sql};

use crate::entry::{EntryRecord, EntryStore, NewEntry};
use crate::{Json, Result, StoreError, now};

/// How the submission asked to be delivered. Evaluated **at delivery time**, not at submit
/// time: a `Steer` that lands after the turn already ended degrades into starting a new one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// Inject into the running turn at a safe checkpoint, without interrupting it.
    Steer,
    /// Wait for the live turn to finish, then start a new turn.
    Queue,
}

define_enum_wire!(Delivery {
    Steer => "steer",
    Queue => "queue",
});

impl_enum_sql!(Delivery);

#[derive(Debug, Clone, PartialEq)]
pub struct MailboxRecord {
    pub submission_id: SubmissionId,
    pub session_id: SessionId,
    pub client_request_id: String,
    pub parts: Value,
    pub model_ref: Option<String>,
    pub thinking: Option<Value>,
    pub delivery: Delivery,
    pub created_at: DateTime<Utc>,
}

pub struct MailboxStore<'a> {
    conn: &'a Connection,
}

impl<'a> MailboxStore<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    /// Stores a submission. Idempotent on `client_request_id`: resubmitting returns the row
    /// that already exists rather than creating a second one.
    pub fn submit(
        &self,
        session_id: SessionId,
        client_request_id: &str,
        parts: &Value,
        delivery: Delivery,
    ) -> Result<MailboxRecord> {
        self.submit_with_model(session_id, client_request_id, parts, None, None, delivery)
    }

    /// Stores a submission together with the provider-qualified model and the thinking intent
    /// selected at submit time. Both are pinned here so a later session change cannot reroute
    /// input the user already wrote.
    pub fn submit_with_model(
        &self,
        session_id: SessionId,
        client_request_id: &str,
        parts: &Value,
        model_ref: Option<&str>,
        thinking: Option<&Value>,
        delivery: Delivery,
    ) -> Result<MailboxRecord> {
        if let Some(existing) = self.find_by_client_id(session_id, client_request_id)? {
            return Ok(existing);
        }
        let id = SubmissionId::new();
        self.conn.execute(
            "INSERT INTO mailbox
                (submission_id, session_id, client_request_id, parts, model_ref, thinking, delivery, created_at)
             VALUES (
                :submission_id, :session_id, :client_request_id, :parts, :model_ref,
                :thinking, :delivery, :created_at
             )",
            named_params! {
                ":submission_id": id,
                ":session_id": session_id,
                ":client_request_id": client_request_id,
                ":parts": Json(parts),
                ":model_ref": model_ref,
                ":thinking": thinking.map(Json),
                ":delivery": delivery,
                ":created_at": now(),
            },
        )?;
        self.get(id)
    }

    pub fn get(&self, submission_id: SubmissionId) -> Result<MailboxRecord> {
        self.find(submission_id)?.ok_or(StoreError::NotFound {
            kind: "submission",
            id: submission_id.to_string(),
        })
    }

    pub fn find(&self, submission_id: SubmissionId) -> Result<Option<MailboxRecord>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {COLS} FROM mailbox WHERE submission_id = :submission_id"),
                named_params! { ":submission_id": submission_id },
                map_row,
            )
            .optional()?)
    }

    fn find_by_client_id(
        &self,
        session_id: SessionId,
        client_request_id: &str,
    ) -> Result<Option<MailboxRecord>> {
        Ok(self
            .conn
            .query_row(
                &format!(
                    "SELECT {COLS} FROM mailbox
                     WHERE session_id = :session_id AND client_request_id = :client_request_id"
                ),
                named_params! {
                    ":session_id": session_id,
                    ":client_request_id": client_request_id,
                },
                map_row,
            )
            .optional()?)
    }

    /// Everything undelivered for this session, FIFO.
    pub fn pending(&self, session_id: SessionId) -> Result<Vec<MailboxRecord>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {COLS} FROM mailbox WHERE session_id = :session_id ORDER BY id"
        ))?;
        Ok(st
            .query_map(named_params! { ":session_id": session_id }, map_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Sessions that have anything waiting, ordered by each session's oldest mailbox row.
    /// Startup recovery uses this to resume durable input without knowing whether it came from a
    /// user submission or a background task. Session locks still decide which host may run it.
    pub fn pending_sessions(&self) -> Result<Vec<SessionId>> {
        let mut statement = self.conn.prepare(
            "SELECT session_id
               FROM mailbox
              GROUP BY session_id
              ORDER BY MIN(id)",
        )?;
        Ok(statement
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Changes the delivery strategy. This and [`MailboxStore::cancel`] are the only two
    /// mutations a mailbox row allows.
    pub fn retarget(&self, submission_id: SubmissionId, delivery: Delivery) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE mailbox SET delivery = :delivery WHERE submission_id = :submission_id",
            named_params! { ":delivery": delivery, ":submission_id": submission_id },
        )?;
        Ok(n == 1)
    }

    pub fn cancel(&self, submission_id: SubmissionId) -> Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM mailbox WHERE submission_id = :submission_id",
            named_params! { ":submission_id": submission_id },
        )?;
        Ok(n == 1)
    }

    /// **Delivery is a move.** Appending the entry and deleting the mailbox row happen in
    /// one transaction.
    pub fn deliver(&self, submission_id: SubmissionId, entry: NewEntry) -> Result<EntryRecord> {
        let tx = crate::tx_or_join(self.conn)?;
        let rec = EntryStore::new(self.conn).append(entry)?;
        let n = self.conn.execute(
            "DELETE FROM mailbox WHERE submission_id = :submission_id",
            named_params! { ":submission_id": submission_id },
        )?;
        if n != 1 {
            // The row is gone, so somebody else delivered it first. Roll back rather than
            // deliver a second time.
            // Inside a caller's transaction there is nothing of ours to roll back separately;
            // reporting the error lets them decide, and their rollback covers our insert.
            if let Some(tx) = tx {
                tx.rollback()?;
            }
            return Err(StoreError::NotFound {
                kind: "submission",
                id: submission_id.to_string(),
            });
        }
        if let Some(tx) = tx {
            tx.commit()?;
        }
        Ok(rec)
    }

    /// Delivers a submission whose message is more than one part.
    /// A submission can hold text plus attachments while an entry holds one part, so the parts are
    /// appended together with the move: **one row in, several entries out, one transaction**. Doing
    /// it as two transactions would allow half a message in the history with nothing left in the
    /// mailbox to redeliver.
    /// `first` is separate from `rest` so that "at least one entry" is not a runtime check.
    /// Returns the entry that anchors the message.
    pub fn deliver_all(
        &self,
        submission_id: SubmissionId,
        first: NewEntry,
        rest: Vec<NewEntry>,
    ) -> Result<EntryRecord> {
        let tx = crate::tx_or_join(self.conn)?;
        // Joins this transaction rather than opening its own.
        let anchor = self.deliver(submission_id, first)?;
        let entries = EntryStore::new(self.conn);
        for entry in rest {
            entries.append(entry)?;
        }
        if let Some(tx) = tx {
            tx.commit()?;
        }
        Ok(anchor)
    }

    /// Moves a delivered submission back into the mailbox.
    /// Used when rewind removes a steered user entry: the payload returns to pending
    /// automatically, with no flag to forget.
    pub fn restore(
        &self,
        session_id: SessionId,
        client_request_id: &str,
        parts: &Value,
        delivery: Delivery,
        entry_id: zlogic_protocol::EntryId,
    ) -> Result<MailboxRecord> {
        let tx = crate::tx_or_join(self.conn)?;
        self.conn.execute(
            "DELETE FROM session_entry WHERE entry_id = :entry_id",
            named_params! { ":entry_id": entry_id },
        )?;
        let rec = self.submit(session_id, client_request_id, parts, delivery)?;
        if let Some(tx) = tx {
            tx.commit()?;
        }
        Ok(rec)
    }
}

const COLS: &str = "submission_id, session_id, client_request_id, parts, model_ref, thinking, delivery, created_at";

fn map_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<MailboxRecord> {
    Ok(MailboxRecord {
        submission_id: r.get("submission_id")?,
        session_id: r.get("session_id")?,
        client_request_id: r.get("client_request_id")?,
        parts: r.get::<_, Json<Value>>("parts")?.0,
        model_ref: r.get("model_ref")?,
        thinking: r
            .get::<_, Option<Json<Value>>>("thinking")?
            .map(|json| json.0),
        delivery: r.get("delivery")?,
        created_at: r.get("created_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::EntryKind;
    use crate::{Db, NewSession};
    use serde_json::json;
    use zlogic_protocol::{TurnId, WorkspaceId};

    fn setup() -> (Db, SessionId) {
        let db = Db::open_in_memory().unwrap();
        let s = db
            .sessions()
            .create(NewSession::root(WorkspaceId::new()))
            .unwrap();
        (db, s.session_id)
    }

    fn user_entry(sid: SessionId, parts: &Value) -> NewEntry {
        NewEntry::new(sid, TurnId::new(), 1, EntryKind::User, parts.clone())
    }

    #[test]
    fn submit_is_idempotent_on_client_request_id() {
        let (db, sid) = setup();
        let m = db.mailbox();
        let a = m
            .submit(sid, "req-1", &json!(["hi"]), Delivery::Queue)
            .unwrap();
        let b = m
            .submit(sid, "req-1", &json!(["hi again"]), Delivery::Steer)
            .unwrap();
        assert_eq!(a.submission_id, b.submission_id);
        assert_eq!(m.pending(sid).unwrap().len(), 1);
    }

    #[test]
    fn selected_model_ref_survives_while_the_submission_is_queued() {
        let (db, sid) = setup();
        let record = db
            .mailbox()
            .submit_with_model(
                sid,
                "req-model",
                &json!(["hi"]),
                Some("openai:gpt:2026"),
                None,
                Delivery::Queue,
            )
            .unwrap();

        assert_eq!(record.model_ref.as_deref(), Some("openai:gpt:2026"));
        assert_eq!(
            db.mailbox().get(record.submission_id).unwrap().model_ref,
            record.model_ref
        );
    }

    #[test]
    fn pinned_thinking_survives_while_the_submission_is_queued() {
        let (db, sid) = setup();
        let thinking = json!({ "mode": "on", "effort": "high" });
        let record = db
            .mailbox()
            .submit_with_model(
                sid,
                "req-thinking",
                &json!(["hi"]),
                None,
                Some(&thinking),
                Delivery::Queue,
            )
            .unwrap();

        assert_eq!(record.thinking.as_ref(), Some(&thinking));
        assert_eq!(
            db.mailbox().get(record.submission_id).unwrap().thinking,
            record.thinking,
            "the thinking pin must travel with the queue just like the model pin"
        );
    }

    #[test]
    fn no_thinking_pin_stays_null_through_the_queue() {
        let (db, sid) = setup();
        let record = db
            .mailbox()
            .submit(sid, "req-plain", &json!(["hi"]), Delivery::Queue)
            .unwrap();
        assert_eq!(record.thinking, None);
        assert_eq!(
            db.mailbox().get(record.submission_id).unwrap().thinking,
            None
        );
    }

    #[test]
    fn mailbox_rejects_an_unqualified_model_ref() {
        let (db, sid) = setup();
        assert!(
            db.mailbox()
                .submit_with_model(
                    sid,
                    "req-bare-model",
                    &json!(["hi"]),
                    Some("gpt-5"),
                    None,
                    Delivery::Queue,
                )
                .is_err()
        );
    }

    #[test]
    fn pending_is_fifo() {
        let (db, sid) = setup();
        let m = db.mailbox();
        for i in 0..3 {
            m.submit(sid, &format!("r{i}"), &json!([i]), Delivery::Queue)
                .unwrap();
        }
        let seen: Vec<Value> = m
            .pending(sid)
            .unwrap()
            .into_iter()
            .map(|r| r.parts)
            .collect();
        assert_eq!(seen, [json!([0]), json!([1]), json!([2])]);
    }

    #[test]
    fn pending_sessions_follow_their_oldest_row() {
        let db = Db::open_in_memory().unwrap();
        let workspace = WorkspaceId::new();
        let first = db
            .sessions()
            .create(NewSession::root(workspace))
            .unwrap()
            .session_id;
        let second = db
            .sessions()
            .create(NewSession::root(workspace))
            .unwrap()
            .session_id;
        let mailbox = db.mailbox();
        mailbox
            .submit(first, "a", &json!(["a"]), Delivery::Queue)
            .unwrap();
        mailbox
            .submit(second, "b", &json!(["b"]), Delivery::Queue)
            .unwrap();
        mailbox
            .submit(first, "c", &json!(["c"]), Delivery::Queue)
            .unwrap();

        assert_eq!(mailbox.pending_sessions().unwrap(), [first, second]);
    }

    #[test]
    fn delivery_moves_the_row_it_does_not_copy_it() {
        let (db, sid) = setup();
        let m = db.mailbox();
        let parts = json!(["hello"]);
        let sub = m.submit(sid, "r1", &parts, Delivery::Queue).unwrap();

        let entry = m
            .deliver(sub.submission_id, user_entry(sid, &parts))
            .unwrap();

        assert!(
            m.pending(sid).unwrap().is_empty(),
            "the antechamber must be empty after delivery"
        );
        assert_eq!(db.entries().list(sid).unwrap().len(), 1);
        assert_eq!(entry.data, parts);
    }

    /// A second delivery must fail **and** leave no extra entry behind.
    #[test]
    fn double_delivery_is_rejected_and_rolls_back() {
        let (db, sid) = setup();
        let m = db.mailbox();
        let parts = json!(["once"]);
        let sub = m.submit(sid, "r1", &parts, Delivery::Queue).unwrap();

        m.deliver(sub.submission_id, user_entry(sid, &parts))
            .unwrap();
        assert!(
            m.deliver(sub.submission_id, user_entry(sid, &parts))
                .is_err()
        );
        assert_eq!(
            db.entries().list(sid).unwrap().len(),
            1,
            "the rollback must undo the append"
        );
    }

    /// A message made of several parts is still one delivery.
    #[test]
    fn a_multi_part_message_is_delivered_as_one_move() {
        let (db, sid) = setup();
        let m = db.mailbox();
        let parts = json!(["text", "attachment"]);
        let sub = m.submit(sid, "r1", &parts, Delivery::Steer).unwrap();

        let anchor = m
            .deliver_all(
                sub.submission_id,
                user_entry(sid, &json!("text")),
                vec![user_entry(sid, &json!("attachment"))],
            )
            .unwrap();

        assert!(m.pending(sid).unwrap().is_empty());
        let entries = db.entries().list(sid).unwrap();
        assert_eq!(entries.len(), 2, "both parts landed");
        assert_eq!(
            entries[0].entry_id, anchor.entry_id,
            "the anchor is the first part"
        );
    }

    /// Half a message in the history with nothing left in the mailbox would be unrecoverable.
    #[test]
    fn a_failed_multi_part_delivery_leaves_neither_half() {
        let (db, sid) = setup();
        let m = db.mailbox();
        let sub = m.submit(sid, "r1", &json!(["a"]), Delivery::Steer).unwrap();
        m.deliver(sub.submission_id, user_entry(sid, &json!("a")))
            .unwrap();

        // The row is already gone, so this delivery cannot succeed.
        assert!(
            m.deliver_all(
                sub.submission_id,
                user_entry(sid, &json!("b")),
                vec![user_entry(sid, &json!("c"))],
            )
            .is_err()
        );
        assert_eq!(
            db.entries().list(sid).unwrap().len(),
            1,
            "the rollback must undo every part, not just the first"
        );
    }

    #[test]
    fn retarget_and_cancel_are_the_only_mutations() {
        let (db, sid) = setup();
        let m = db.mailbox();
        let sub = m.submit(sid, "r1", &json!(["x"]), Delivery::Queue).unwrap();

        assert!(m.retarget(sub.submission_id, Delivery::Steer).unwrap());
        assert_eq!(m.get(sub.submission_id).unwrap().delivery, Delivery::Steer);

        assert!(m.cancel(sub.submission_id).unwrap());
        assert!(m.pending(sid).unwrap().is_empty());
        assert!(
            !m.cancel(sub.submission_id).unwrap(),
            "cancelling twice is a no-op"
        );
    }

    #[test]
    fn delivery_round_trips_through_its_wire_name() {
        let (db, sid) = setup();
        db.mailbox()
            .submit(sid, "r1", &json!([]), Delivery::Steer)
            .unwrap();
        let raw: String = db
            .conn()
            .query_row("SELECT delivery FROM mailbox", [], |r| r.get(0))
            .unwrap();
        assert_eq!(raw, "steer");
    }

    /// Rewinding a steered entry must return the payload to the antechamber, not lose it.
    #[test]
    fn rewind_restores_a_delivered_submission() {
        let (db, sid) = setup();
        let m = db.mailbox();
        let parts = json!(["steered"]);
        let sub = m.submit(sid, "r1", &parts, Delivery::Steer).unwrap();
        let entry = m
            .deliver(sub.submission_id, user_entry(sid, &parts))
            .unwrap();

        m.restore(sid, "r1", &parts, Delivery::Steer, entry.entry_id)
            .unwrap();

        assert!(db.entries().list(sid).unwrap().is_empty());
        let back = m.pending(sid).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].parts, parts);
        assert_eq!(back[0].delivery, Delivery::Steer);
    }

    #[test]
    fn deleting_a_session_clears_its_mailbox() {
        let (db, sid) = setup();
        db.mailbox()
            .submit(sid, "r1", &json!(["x"]), Delivery::Queue)
            .unwrap();
        db.sessions().delete(sid).unwrap();
        let n: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM mailbox", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }
}
