//! The `session_entry` table — the single source of truth for conversation history.
//! # One row per part; turn and round are columns, not storage units
//! Storing whole turns would give up streaming appends and crash safety, and rewind,
//! compaction and edit history only ever needed a `turn_seq` **column**.
//! # Interactions live here too
//! A permission prompt and its answer are two entries, not rows in a side table. The
//! timeline needs them in place with full detail, and keeping them in two places invites the
//! two copies to disagree. `build_context` filters them out on the way to the model.
//! Modelling them as request + response (rather than one mutable row with a status) keeps
//! entries append-only and immutable, which is what makes replay equal live rendering.
//! # Large payloads go to the object store
//! Anything over [`INLINE_LIMIT`] is written to the object store; the entry keeps a reference.
//! This is the direct answer to "don't let state.db grow to gigabytes" — a large file read in, a
//! full build log.
//! # References live in `entry_object`, not in a column
//! One entry can point at several objects: three image attachments, a tool result with both a
//! large stdout and a captured diff. And the ids that appear inside `data` are there for their
//! *meaning* — a tool result knows which object is stdout and which is the diff.
//! Enumerating them is a separate job, and it cannot be done from `data`: when `data` itself is
//! offloaded, finding an entry's references would require fetching that object first. Garbage
//! collection would have to read every offloaded payload to learn what it points at. The join
//! table breaks that circularity — and its index on `object_id` answers the question GC actually
//! asks, "is anything still referencing this?"

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, named_params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use zlogic_objects::{ObjectId, ObjectRef, ObjectRole, ObjectStore};
use zlogic_protocol::message::Source;
use zlogic_protocol::{EntryId, RoundId, SessionId, TurnId, define_enum_wire, impl_enum_sql};

use crate::{Json, Result, StoreError, now};

/// Payloads larger than this go to the object store.
/// 8 KiB because SQLite's default page is 4 KiB: a row spilling past one page moves onto
/// overflow pages, which makes sequential scans markedly more expensive. Text entries below
/// that — nearly all thinking and assistant text — are fastest kept inline.
pub const INLINE_LIMIT: usize = 8 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    User,
    Thinking,
    AssistantText,
    ToolCall,
    ToolResult,
    /// The currently loaded definition of a skill. Context projection keeps only the newest
    /// effective row for each qualified skill name.
    SkillLoad,
    /// Explicit removal of one loaded skill from session context state.
    SkillUnload,
    /// The names of deferred tools loaded via `load_tool`. Session state, like [`EntryKind::SkillLoad`]:
    /// a loaded tool stays loaded on later turns. Never replayed to the model — the loaded
    /// definitions travel in the request's `tools`, not in the message history.
    ToolLoad,
    /// A prompt shown to the user (permission, choice, env passthrough).
    InteractionRequest,
    /// The answer to an [`EntryKind::InteractionRequest`]. Pending == a request with no
    /// response after it.
    InteractionResponse,
    /// Notices and other timeline-only records.
    Event,
    Compaction,
    Steering,
    /// A machine-generated background-task completion, replayed to the model as user-role input.
    TaskUpdate,
}

define_enum_wire!(EntryKind {
    User => "user",
    Thinking => "thinking",
    AssistantText => "assistant_text",
    ToolCall => "tool_call",
    ToolResult => "tool_result",
    SkillLoad => "skill_load",
    SkillUnload => "skill_unload",
    ToolLoad => "tool_load",
    InteractionRequest => "interaction_request",
    InteractionResponse => "interaction_response",
    Event => "event",
    Compaction => "compaction",
    Steering => "steering",
    TaskUpdate => "task_update",
});

impl_enum_sql!(EntryKind);

impl EntryKind {
    /// Whether this kind is ever replayed to the model.
    /// Interactions and notices exist for the timeline only. `build_context` uses this so
    /// the rule lives in one place instead of being re-derived per call site.
    pub fn goes_to_model(self) -> bool {
        !matches!(
            self,
            EntryKind::InteractionRequest
                | EntryKind::InteractionResponse
                | EntryKind::Event
                | EntryKind::ToolLoad
        )
    }
}

#[derive(Debug, Clone)]
pub struct NewEntry {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub turn_seq: i64,
    pub round_id: Option<RoundId>,
    pub round_seq: Option<u32>,
    pub kind: EntryKind,
    pub data: Value,
    /// Provider-native replayable payload, byte for byte.
    pub native: Option<Value>,
    /// Which model produced this — the raw-replay gate's criterion.
    pub source: Option<Source>,
    /// UI-only projection: a tool result's outcome status and its display cards.
    /// Kept out of `data` on purpose — see the `V4` migration in [`crate::schema`].
    pub display: Option<Value>,
    /// Objects this entry references. **Declared explicitly** rather than scraped out of `data`:
    /// scanning JSON for things that look like object ids would both miss nested shapes and pick
    /// up ids that a tool result merely mentions as text.
    pub objects: Vec<ObjectRef>,
}

impl NewEntry {
    pub fn new(
        session_id: SessionId,
        turn_id: TurnId,
        turn_seq: i64,
        kind: EntryKind,
        data: Value,
    ) -> Self {
        Self {
            session_id,
            turn_id,
            turn_seq,
            round_id: None,
            round_seq: None,
            kind,
            data,
            native: None,
            source: None,
            display: None,
            objects: Vec::new(),
        }
    }

    pub fn in_round(mut self, round_id: RoundId) -> Self {
        self.round_id = Some(round_id);
        self
    }

    pub fn with_round_seq(mut self, round_seq: u32) -> Self {
        self.round_seq = Some(round_seq);
        self
    }

    pub fn with_native(mut self, native: Value) -> Self {
        self.native = Some(native);
        self
    }

    pub fn from_model(mut self, source: Source) -> Self {
        self.source = Some(source);
        self
    }

    /// Records the UI-only projection (tool status + display cards).
    pub fn with_display(mut self, display: Value) -> Self {
        self.display = Some(display);
        self
    }

    /// Declares an object this entry references.
    pub fn references(mut self, obj: ObjectRef) -> Self {
        self.objects.push(obj);
        self
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct EntryRecord {
    pub entry_id: EntryId,
    pub session_id: SessionId,
    pub seq: i64,
    pub turn_seq: i64,
    pub turn_id: TurnId,
    pub round_id: Option<RoundId>,
    pub round_seq: Option<u32>,
    pub kind: EntryKind,
    /// `Value::Null` when the payload was offloaded; use [`EntryStore::load_data`].
    pub data: Value,
    pub native: Option<Value>,
    pub source: Option<Source>,
    /// UI-only projection: a tool result's outcome status and its display cards.
    /// `None` on every other kind, and on tool results written before the `display` column
    /// existed — which is why the timeline treats it as optional rather than required.
    pub display: Option<Value>,
    pub is_final: bool,
    /// Every object this entry references, in insertion order.
    pub objects: Vec<ObjectRef>,
    pub created_at: DateTime<Utc>,
}

impl EntryRecord {
    /// The object holding this entry's own payload, if it was offloaded.
    pub fn payload_object(&self) -> Option<&ObjectId> {
        self.objects
            .iter()
            .find(|o| o.role == ObjectRole::Payload)
            .map(|o| &o.object_id)
    }

    pub fn is_offloaded(&self) -> bool {
        self.payload_object().is_some()
    }

    pub fn objects_with_role(&self, role: ObjectRole) -> impl Iterator<Item = &ObjectId> {
        self.objects
            .iter()
            .filter(move |o| o.role == role)
            .map(|o| &o.object_id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ConversationStats {
    pub turn_count: u32,
    pub last_message_at: Option<DateTime<Utc>>,
}

/// A permission prompt still waiting for an answer.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingInteraction {
    pub request: EntryRecord,
    /// The `interaction_id` carried inside `request.data`.
    pub interaction_id: String,
}

pub struct EntryStore<'a> {
    conn: &'a Connection,
}

fn user_visible_text_contains(value: &Value, needle: &str) -> bool {
    let Value::Object(fields) = value else {
        return false;
    };
    matches!(
        (
            fields.get("type").and_then(Value::as_str),
            fields.get("text").and_then(Value::as_str),
        ),
        (Some("text"), Some(text)) if text.contains(needle)
    )
}

impl<'a> EntryStore<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    /// Appends one entry. `seq` is allocated by the store, never by the caller.
    pub fn append(&self, new: NewEntry) -> Result<EntryRecord> {
        self.append_inner(new, false, false)
    }

    /// Appends, offloading `data` to the object store when it exceeds [`INLINE_LIMIT`].
    /// The offloaded payload is recorded as an [`ObjectRole::Payload`] reference, which is how
    /// [`EntryStore::load_data`] finds it again and how GC knows it is live.
    pub fn append_with_offload(
        &self,
        new: NewEntry,
        objects: &dyn ObjectStore,
    ) -> Result<EntryRecord> {
        let (new, offloaded) = self.prepare_offload(new, objects)?;
        self.append_inner(new, offloaded, false)
    }

    /// Appends a `turn_end` event and, in the **same transaction**, stamps `is_final` on this
    /// turn's last text-like entry (`assistant_text` ∪ `thinking`).
    pub fn append_turn_end(&self, new: NewEntry, objects: &dyn ObjectStore) -> Result<EntryRecord> {
        let (new, offloaded) = self.prepare_offload(new, objects)?;
        self.append_inner(new, offloaded, true)
    }

    /// Offload decision + object-store write, shared by the two offload-capable appends.
    fn prepare_offload(
        &self,
        mut new: NewEntry,
        objects: &dyn ObjectStore,
    ) -> Result<(NewEntry, bool)> {
        let encoded = serde_json::to_vec(&new.data)?;
        if encoded.len() <= INLINE_LIMIT {
            return Ok((new, false));
        }
        let id = objects.put(&encoded)?;
        new.objects.push(ObjectRef::payload(id));
        Ok((new, true))
    }

    /// Reads the payload back, from the row or from the object store.
    pub fn load_data(&self, rec: &EntryRecord, objects: &dyn ObjectStore) -> Result<Value> {
        match rec.payload_object() {
            None => Ok(rec.data.clone()),
            Some(id) => Ok(serde_json::from_slice(&objects.get(id)?)?),
        }
    }

    pub fn payload_objects(&self, entries: &[EntryRecord]) -> Result<HashMap<EntryId, ObjectId>> {
        let mut out = HashMap::new();
        for chunk in entries.chunks(500) {
            let placeholders = std::iter::repeat_n("?", chunk.len())
                .collect::<Vec<_>>()
                .join(", ");
            let mut st = self.conn.prepare(&format!(
                "SELECT entry_id, object_id FROM entry_object
                 WHERE entry_id IN ({placeholders}) AND role = 'payload'"
            ))?;
            let rows = st.query_map(
                rusqlite::params_from_iter(chunk.iter().map(|e| &e.entry_id)),
                |r| Ok((r.get::<_, EntryId>(0)?, r.get::<_, ObjectId>(1)?)),
            )?;
            for row in rows {
                let (entry_id, object_id) = row?;
                out.insert(entry_id, object_id);
            }
        }
        Ok(out)
    }

    fn append_inner(
        &self,
        new: NewEntry,
        offloaded: bool,
        mark_final: bool,
    ) -> Result<EntryRecord> {
        let entry_id = EntryId::new();

        // Offloaded rows keep a null placeholder inline; the content is in the object store.
        let data = if offloaded { Value::Null } else { new.data };

        // At most one payload reference, or `load_data` would have to pick between two truths.
        if new
            .objects
            .iter()
            .filter(|o| o.role == ObjectRole::Payload)
            .count()
            > 1
        {
            return Err(StoreError::Corrupt(
                "an entry cannot have two payload objects".into(),
            ));
        }

        // The entry and its references land together: a reference without its entry would be a
        // dangling row, and an entry whose references were lost would leak objects forever.
        let tx = crate::tx_or_join(self.conn)?;

        let created_at = now();

        // Maintains `session.turn_count` / `session.last_message_at` (the columns that replaced
        // the old `GROUP BY session_entry … LEFT JOIN session_locks` read). Runs **before** the
        // insert: the "first conversation entry of this turn_seq" test would otherwise already see
        // the row we are about to add, and never count the turn.
        // - `event` entries are timeline-only notices: they touch neither column.
        // - A live turn's in-flight entry does **not** advance `last_message_at` — only completed
        //   turns anchor the list position. That reproduces the old JOIN's exclusion without the
        //   join; the turn ending (lock release / steal) recomputes it. See
        //   [`recompute_session_stats`].
        if !matches!(new.kind, EntryKind::Event) {
            self.conn.execute(
                "UPDATE session SET
                     turn_count = turn_count + CASE WHEN NOT EXISTS (
                         SELECT 1 FROM session_entry e
                         WHERE e.session_id = :session_id AND e.turn_seq = :turn_seq
                           AND e.kind != 'event'
                     ) THEN 1 ELSE 0 END,
                     last_message_at = CASE WHEN EXISTS (
                         SELECT 1 FROM session_locks l
                         WHERE l.session_id = :session_id AND l.turn_id = :turn_id
                     ) THEN last_message_at
                     WHEN last_message_at IS NULL OR :created_at > last_message_at THEN :created_at
                     ELSE last_message_at END
                   WHERE session_id = :session_id",
                named_params! {
                    ":session_id": new.session_id,
                    ":turn_seq": new.turn_seq,
                    ":turn_id": new.turn_id,
                    ":created_at": created_at,
                },
            )?;
        }

        // `seq` is computed inside the INSERT so it shares this transaction. A read-then-write
        // would leave a window for another writer to slip in.
        self.conn.execute(
            "INSERT INTO session_entry
                (entry_id, session_id, seq, turn_seq, turn_id, round_id, round_seq, kind,
                 data, native, source, display, created_at)
             VALUES (
                 :entry_id, :session_id,
                 (SELECT COALESCE(MAX(seq), 0) + 1 FROM session_entry
                   WHERE session_id = :session_id),
                 :turn_seq, :turn_id, :round_id, :round_seq, :kind,
                 :data, :native, :source, :display, :created_at)",
            named_params! {
                ":entry_id": entry_id,
                ":session_id": new.session_id,
                ":turn_seq": new.turn_seq,
                ":turn_id": new.turn_id,
                ":round_id": new.round_id,
                ":round_seq": new.round_seq,
                ":kind": new.kind,
                ":data": Json(data),
                ":native": new.native.map(Json),
                ":source": new.source.map(Json),
                ":display": new.display.map(Json),
                ":created_at": created_at,
            },
        )?;

        for obj in &new.objects {
            let kind = obj
                .kind
                .clone()
                .unwrap_or_else(|| derived_asset_kind(&obj).to_string());
            self.conn.execute(
                "INSERT OR IGNORE INTO entry_object (entry_id, object_id, role, ref_key, kind, label, meta)
                 VALUES (:entry_id, :object_id, :role, :ref_key, :kind, :label, :meta)",
                named_params! {
                    ":entry_id": entry_id,
                    ":object_id": obj.object_id,
                    ":role": obj.role,
                    ":ref_key": obj.ref_key,
                    ":kind": kind,
                    ":label": obj.label,
                    ":meta": obj.meta.clone(),
                },
            )?;
        }

        if mark_final {
            self.conn.execute(
                "UPDATE session_entry SET is_final = 1
                  WHERE entry_id = (
                    SELECT entry_id FROM session_entry
                     WHERE session_id = :session_id AND turn_seq = :turn_seq
                       AND kind IN ('assistant_text', 'thinking')
                     ORDER BY seq DESC LIMIT 1
                  )",
                named_params! {
                    ":session_id": new.session_id,
                    ":turn_seq": new.turn_seq,
                },
            )?;
        }

        if let Some(tx) = tx {
            tx.commit()?;
        }

        self.get(entry_id)?.ok_or(StoreError::NotFound {
            kind: "entry",
            id: entry_id.to_string(),
        })
    }

    /// Every object referenced by one entry.
    fn objects_of(&self, entry_id: EntryId) -> Result<Vec<ObjectRef>> {
        let mut st = self.conn.prepare(
            "SELECT object_id, role, ref_key, kind, label, meta FROM entry_object
             WHERE entry_id = :entry_id ORDER BY id",
        )?;
        Ok(st
            .query_map(named_params! { ":entry_id": entry_id }, |r| {
                Ok(ObjectRef {
                    object_id: r.get("object_id")?,
                    role: r.get("role")?,
                    ref_key: r.get("ref_key")?,
                    kind: r.get("kind")?,
                    label: r.get("label")?,
                    meta: r.get("meta")?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn get(&self, entry_id: EntryId) -> Result<Option<EntryRecord>> {
        let bare = self
            .conn
            .query_row(
                &format!("SELECT {COLS} FROM session_entry WHERE entry_id = :entry_id"),
                named_params! { ":entry_id": entry_id },
                map_row,
            )
            .optional()?;
        match bare {
            None => Ok(None),
            Some(mut rec) => {
                rec.objects = self.objects_of(rec.entry_id)?;
                Ok(Some(rec))
            }
        }
    }

    pub fn list(&self, session_id: SessionId) -> Result<Vec<EntryRecord>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {COLS} FROM session_entry WHERE session_id = :session_id ORDER BY seq"
        ))?;
        let rows: Vec<EntryRecord> = st
            .query_map(named_params! { ":session_id": session_id }, map_row)?
            .collect::<rusqlite::Result<_>>()?;
        self.attach_objects(rows)
    }

    pub fn last(&self, session_id: SessionId) -> Result<Option<EntryRecord>> {
        let bare = self
            .conn
            .query_row(
                &format!(
                    "SELECT {COLS} FROM session_entry
                     WHERE session_id = :session_id ORDER BY seq DESC LIMIT 1"
                ),
                named_params! { ":session_id": session_id },
                map_row,
            )
            .optional()?;
        match bare {
            None => Ok(None),
            Some(mut rec) => {
                rec.objects = self.objects_of(rec.entry_id)?;
                Ok(Some(rec))
            }
        }
    }

    pub fn last_turn_of(&self, session_id: SessionId) -> Result<Option<TurnId>> {
        Ok(self
            .conn
            .query_row(
                "SELECT turn_id FROM session_entry
                 WHERE session_id = :session_id ORDER BY seq DESC LIMIT 1",
                named_params! { ":session_id": session_id },
                |r| r.get(0),
            )
            .optional()?)
    }

    /// One entry kind for a session, in conversation order.
    /// State lookups such as the active skill set must not scan an entire long conversation just
    /// to find a handful of typed records.
    pub fn list_kind(&self, session_id: SessionId, kind: EntryKind) -> Result<Vec<EntryRecord>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {COLS} FROM session_entry
             WHERE session_id = :session_id AND kind = :kind
             ORDER BY seq"
        ))?;
        let rows: Vec<EntryRecord> = st
            .query_map(
                named_params! { ":session_id": session_id, ":kind": kind },
                map_row,
            )?
            .collect::<rusqlite::Result<_>>()?;
        self.attach_objects(rows)
    }

    /// The append-only state transition log for loaded skills.
    pub fn list_skill_state(&self, session_id: SessionId) -> Result<Vec<EntryRecord>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {COLS} FROM session_entry
             WHERE session_id = :session_id AND kind IN (:load, :unload)
             ORDER BY seq"
        ))?;
        let rows: Vec<EntryRecord> = st
            .query_map(
                named_params! {
                    ":session_id": session_id,
                    ":load": EntryKind::SkillLoad,
                    ":unload": EntryKind::SkillUnload,
                },
                map_row,
            )?
            .collect::<rusqlite::Result<_>>()?;
        self.attach_objects(rows)
    }

    /// Whether an exact quote occurs in user-authored input for this turn.
    /// Memory writes use this as a provenance gate. Only `user` and `steering` entries count:
    /// accepting assistant/tool text would let the model cite its own invention as user source.
    pub fn user_turn_contains(
        &self,
        session_id: SessionId,
        turn_id: zlogic_protocol::TurnId,
        quote: &str,
        objects: &dyn ObjectStore,
    ) -> Result<bool> {
        if quote.trim().chars().count() < 2 {
            return Ok(false);
        }
        let mut st = self.conn.prepare(&format!(
            "SELECT {COLS} FROM session_entry
             WHERE session_id = :session_id AND turn_id = :turn_id
               AND kind IN ('user', 'steering')
             ORDER BY seq"
        ))?;
        let rows: Vec<EntryRecord> = st
            .query_map(
                named_params! { ":session_id": session_id, ":turn_id": turn_id },
                map_row,
            )?
            .collect::<rusqlite::Result<_>>()?;
        for record in self.attach_objects(rows)? {
            let value = self.load_data(&record, objects)?;
            if user_visible_text_contains(&value, quote) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn list_turn(&self, session_id: SessionId, turn_seq: i64) -> Result<Vec<EntryRecord>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {COLS} FROM session_entry
             WHERE session_id = :session_id AND turn_seq = :turn_seq ORDER BY seq"
        ))?;
        let rows: Vec<EntryRecord> = st
            .query_map(
                named_params! { ":session_id": session_id, ":turn_seq": turn_seq },
                map_row,
            )?
            .collect::<rusqlite::Result<_>>()?;
        self.attach_objects(rows)
    }

    pub fn turn_page(
        &self,
        session_id: SessionId,
        after_turn_seq: Option<i64>,
        offset: u32,
        limit: u32,
    ) -> Result<(Vec<i64>, u64)> {
        let mut where_sql = String::from("session_id = ? AND kind != 'event'");
        let mut where_args: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(session_id)];
        if let Some(after) = after_turn_seq {
            where_sql.push_str(" AND turn_seq > ?");
            where_args.push(Box::new(after));
        }

        let mut st = self.conn.prepare(&format!(
            "SELECT COUNT(DISTINCT turn_seq) FROM session_entry WHERE {where_sql}"
        ))?;
        let total = st.query_row(
            rusqlite::params_from_iter(where_args.iter().map(|b| b.as_ref())),
            |r| r.get::<_, i64>(0),
        )?;

        let mut st = self.conn.prepare(&format!(
            "SELECT DISTINCT turn_seq FROM session_entry
             WHERE {where_sql}
             ORDER BY turn_seq DESC LIMIT ? OFFSET ?"
        ))?;
        where_args.push(Box::new(limit));
        where_args.push(Box::new(offset));
        let seqs = st
            .query_map(
                rusqlite::params_from_iter(where_args.iter().map(|b| b.as_ref())),
                |r| r.get(0),
            )?
            .collect::<rusqlite::Result<Vec<i64>>>()?;
        Ok((seqs, total.max(0) as u64))
    }

    pub fn rows_of_turns(
        &self,
        session_id: SessionId,
        turn_seqs: &[i64],
        kinds: Option<&[EntryKind]>,
        final_only: bool,
    ) -> Result<Vec<EntryRecord>> {
        if turn_seqs.is_empty() || kinds.is_some_and(<[EntryKind]>::is_empty) {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for chunk in turn_seqs.chunks(200) {
            let mut where_sql = String::from("session_id = ?");
            let mut args: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(session_id)];
            let placeholders = vec!["?"; chunk.len()].join(",");
            where_sql.push_str(&format!(" AND turn_seq IN ({placeholders})"));
            args.extend(
                chunk
                    .iter()
                    .map(|&seq| Box::new(seq) as Box<dyn rusqlite::types::ToSql>),
            );
            if let Some(kinds) = kinds {
                let placeholders = vec!["?"; kinds.len()].join(",");
                where_sql.push_str(&format!(" AND kind IN ({placeholders})"));
                args.extend(
                    kinds
                        .iter()
                        .map(|&kind| Box::new(kind) as Box<dyn rusqlite::types::ToSql>),
                );
            }
            if final_only {
                where_sql
                    .push_str(" AND (is_final = 1 OR kind NOT IN ('assistant_text', 'thinking'))");
            }
            let mut st = self.conn.prepare(&format!(
                "SELECT {COLS} FROM session_entry WHERE {where_sql} ORDER BY seq"
            ))?;
            let params = rusqlite::params_from_iter(args.iter().map(|b| b.as_ref()));
            let rows: Vec<EntryRecord> = st
                .query_map(params, map_row)?
                .collect::<rusqlite::Result<_>>()?;
            out.extend(rows);
        }
        self.attach_objects(out)
    }

    pub fn turns_having_kinds(
        &self,
        session_id: SessionId,
        turn_seqs: &[i64],
        kinds: &[EntryKind],
    ) -> Result<HashSet<i64>> {
        if turn_seqs.is_empty() || kinds.is_empty() {
            return Ok(HashSet::new());
        }
        let mut out = HashSet::new();
        for chunk in turn_seqs.chunks(200) {
            let mut where_sql = String::from("session_id = ?");
            let mut args: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(session_id)];
            let placeholders = vec!["?"; chunk.len()].join(",");
            where_sql.push_str(&format!(" AND turn_seq IN ({placeholders})"));
            args.extend(
                chunk
                    .iter()
                    .map(|&seq| Box::new(seq) as Box<dyn rusqlite::types::ToSql>),
            );
            let placeholders = vec!["?"; kinds.len()].join(",");
            where_sql.push_str(&format!(" AND kind IN ({placeholders})"));
            args.extend(
                kinds
                    .iter()
                    .map(|&kind| Box::new(kind) as Box<dyn rusqlite::types::ToSql>),
            );
            let mut st = self.conn.prepare(&format!(
                "SELECT DISTINCT turn_seq FROM session_entry WHERE {where_sql}"
            ))?;
            let rows = st
                .query_map(
                    rusqlite::params_from_iter(args.iter().map(|b| b.as_ref())),
                    |r| r.get::<_, i64>(0),
                )?
                .collect::<rusqlite::Result<Vec<i64>>>()?;
            out.extend(rows);
        }
        Ok(out)
    }

    pub fn last_text_rows_per_turn(
        &self,
        session_id: SessionId,
        turn_seqs: &[i64],
    ) -> Result<Vec<EntryRecord>> {
        if turn_seqs.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for chunk in turn_seqs.chunks(200) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let mut st = self.conn.prepare(&format!(
                "SELECT {COLS} FROM (
                   SELECT {COLS},
                          ROW_NUMBER() OVER (PARTITION BY turn_seq ORDER BY seq DESC) AS rn
                     FROM session_entry
                    WHERE session_id = ? AND turn_seq IN ({placeholders})
                      AND kind IN ('assistant_text', 'thinking')
                 ) WHERE rn = 1 ORDER BY seq"
            ))?;
            let mut args: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(session_id)];
            args.extend(
                chunk
                    .iter()
                    .map(|&seq| Box::new(seq) as Box<dyn rusqlite::types::ToSql>),
            );
            let rows: Vec<EntryRecord> = st
                .query_map(
                    rusqlite::params_from_iter(args.iter().map(|b| b.as_ref())),
                    map_row,
                )?
                .collect::<rusqlite::Result<_>>()?;
            out.extend(rows);
        }
        self.attach_objects(out)
    }

    pub fn search_candidates(&self, session_id: SessionId) -> Result<Vec<EntryRecord>> {
        let mut rows = self.rows_where(session_id, "is_final = 1")?;
        rows.extend(self.rows_where(session_id, "kind IN ('user', 'steering')")?);
        rows.sort_by_key(|rec| rec.seq);
        self.attach_objects(rows)
    }

    fn rows_where(&self, session_id: SessionId, predicate: &str) -> Result<Vec<EntryRecord>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {COLS} FROM session_entry
              WHERE session_id = :session_id AND {predicate}
              ORDER BY seq"
        ))?;
        Ok(st
            .query_map(named_params! { ":session_id": session_id }, map_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn turn_anchors(
        &self,
        session_id: SessionId,
        turn_seqs: &[i64],
    ) -> Result<Vec<(i64, TurnId, DateTime<Utc>)>> {
        if turn_seqs.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for chunk in turn_seqs.chunks(200) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let mut st = self.conn.prepare(&format!(
                "SELECT turn_seq, turn_id, MIN(created_at) AS at
                   FROM session_entry
                  WHERE session_id = ? AND turn_seq IN ({placeholders})
                  GROUP BY turn_seq"
            ))?;
            let mut args: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(session_id)];
            args.extend(
                chunk
                    .iter()
                    .map(|&seq| Box::new(seq) as Box<dyn rusqlite::types::ToSql>),
            );
            let rows = st.query_map(
                rusqlite::params_from_iter(args.iter().map(|b| b.as_ref())),
                |r| {
                    Ok((
                        r.get::<_, i64>("turn_seq")?,
                        r.get::<_, TurnId>("turn_id")?,
                        r.get::<_, DateTime<Utc>>("at")?,
                    ))
                },
            )?;
            out.extend(rows.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        Ok(out)
    }

    pub fn assets_of_turns(
        &self,
        session_id: SessionId,
        turn_seqs: &[i64],
        kinds: &[&str],
    ) -> Result<Vec<TurnAsset>> {
        if turn_seqs.is_empty() || kinds.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for chunk in turn_seqs.chunks(200) {
            let turns = vec!["?"; chunk.len()].join(",");
            let wanted = vec!["?"; kinds.len()].join(",");
            let mut st = self.conn.prepare(&format!(
                "SELECT e.turn_seq AS turn_seq, o.object_id AS object_id,
                        o.kind AS kind, o.label AS label, o.meta AS meta
                   FROM entry_object o
                   JOIN session_entry e ON e.entry_id = o.entry_id
                  WHERE e.session_id = ? AND e.turn_seq IN ({turns}) AND o.kind IN ({wanted})
                  ORDER BY e.turn_seq, e.seq, o.id"
            ))?;
            let mut args: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(session_id)];
            args.extend(
                chunk
                    .iter()
                    .map(|&seq| Box::new(seq) as Box<dyn rusqlite::types::ToSql>),
            );
            args.extend(
                kinds
                    .iter()
                    .map(|kind| Box::new((*kind).to_string()) as Box<dyn rusqlite::types::ToSql>),
            );
            let rows = st.query_map(
                rusqlite::params_from_iter(args.iter().map(|b| b.as_ref())),
                |r| {
                    Ok(TurnAsset {
                        turn_seq: r.get("turn_seq")?,
                        object_id: r.get("object_id")?,
                        kind: r.get("kind")?,
                        label: r.get("label")?,
                        meta: r.get("meta")?,
                    })
                },
            )?;
            out.extend(rows.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        Ok(out)
    }

    pub fn entries_filtered(
        &self,
        session_id: SessionId,
        turn_seq: Option<i64>,
        turn_id: Option<TurnId>,
        kinds: Option<&[EntryKind]>,
        only_final: bool,
        offset: u32,
        limit: u32,
    ) -> Result<(Vec<EntryRecord>, u64)> {
        let mut where_sql = String::from("session_id = ?");
        let mut args: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(session_id)];
        if let Some(seq) = turn_seq {
            where_sql.push_str(" AND turn_seq = ?");
            args.push(Box::new(seq));
        }
        if let Some(id) = turn_id {
            where_sql.push_str(" AND turn_id = ?");
            args.push(Box::new(id));
        }
        if let Some(kinds) = kinds {
            let placeholders = vec!["?"; kinds.len()].join(",");
            where_sql.push_str(&format!(" AND kind IN ({placeholders})"));
            args.extend(
                kinds
                    .iter()
                    .map(|&k| Box::new(k) as Box<dyn rusqlite::types::ToSql>),
            );
        }
        if only_final {
            where_sql.push_str(" AND is_final = 1");
        }

        let mut st = self.conn.prepare(&format!(
            "SELECT COUNT(*) FROM session_entry WHERE {where_sql}"
        ))?;
        let total = st.query_row(
            rusqlite::params_from_iter(args.iter().map(|b| b.as_ref())),
            |r| r.get::<_, i64>(0),
        )?;

        let mut st = self.conn.prepare(&format!(
            "SELECT {COLS} FROM session_entry
             WHERE {where_sql} ORDER BY seq DESC LIMIT ? OFFSET ?"
        ))?;
        args.push(Box::new(limit));
        args.push(Box::new(offset));
        let rows: Vec<EntryRecord> = st
            .query_map(
                rusqlite::params_from_iter(args.iter().map(|b| b.as_ref())),
                map_row,
            )?
            .collect::<rusqlite::Result<_>>()?;
        self.attach_objects(rows)
            .map(|rows| (rows, total.max(0) as u64))
    }
    /// Only the kinds that are replayed to the model, in order.
    /// The complement of this — interactions, notices and tool-load state — is exactly what the
    /// timeline shows (or the state table reads) but the model never sees.
    pub fn list_for_context(&self, session_id: SessionId) -> Result<Vec<EntryRecord>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {COLS} FROM session_entry
             WHERE session_id = :session_id
               AND kind NOT IN (:interaction_request, :interaction_response, :event, :tool_load)
             ORDER BY seq"
        ))?;
        let rows: Vec<EntryRecord> = st
            .query_map(
                named_params! {
                    ":session_id": session_id,
                    ":interaction_request": EntryKind::InteractionRequest,
                    ":interaction_response": EntryKind::InteractionResponse,
                    ":event": EntryKind::Event,
                    ":tool_load": EntryKind::ToolLoad,
                },
                map_row,
            )?
            .collect::<rusqlite::Result<_>>()?;
        self.attach_objects(rows)
    }

    /// Context tail after a compacted prefix.
    /// Compaction records are still needed to materialise the summaries, and every skill state
    /// transition is retained so context projection can select the latest state even when it
    /// originated inside the compacted prefix.
    pub fn list_for_context_after(
        &self,
        session_id: SessionId,
        after_turn: i64,
    ) -> Result<Vec<EntryRecord>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {COLS} FROM session_entry
             WHERE session_id = :session_id
               AND kind NOT IN (:interaction_request, :interaction_response, :event, :tool_load)
               AND (
                    turn_seq > :after_turn
                    OR kind IN (:compaction, :skill_load, :skill_unload)
               )
             ORDER BY seq"
        ))?;
        let rows: Vec<EntryRecord> = st
            .query_map(
                named_params! {
                    ":session_id": session_id,
                    ":after_turn": after_turn,
                    ":interaction_request": EntryKind::InteractionRequest,
                    ":interaction_response": EntryKind::InteractionResponse,
                    ":event": EntryKind::Event,
                    ":tool_load": EntryKind::ToolLoad,
                    ":compaction": EntryKind::Compaction,
                    ":skill_load": EntryKind::SkillLoad,
                    ":skill_unload": EntryKind::SkillUnload,
                },
                map_row,
            )?
            .collect::<rusqlite::Result<_>>()?;
        self.attach_objects(rows)
    }

    /// The newest conversation turn, mirroring [`Self::list_for_context`]'s filter.
    /// Cheap (one index scan, no payload parsing) — the compaction pre-check uses it to answer
    /// "is there anything left to compact?" without materialising the whole history, which is
    /// what `list_for_context` + `plan_range` would cost on every round.
    pub fn max_conversation_turn(&self, session_id: SessionId) -> Result<Option<i64>> {
        // `MAX()` over no rows still returns one row — with NULL — so the column has to be read as
        // an `Option`: reading it as `i64` turns "this session has no entries yet" into a type
        // error, and "no conversation yet" is exactly the state a `/compact` on a fresh session
        // starts from.
        let v: Option<i64> = self
            .conn
            .query_row(
                "SELECT MAX(turn_seq) FROM session_entry
                 WHERE session_id = :session_id
                   AND kind NOT IN (:interaction_request, :interaction_response, :event, :tool_load)
                   AND kind != 'compaction'",
                named_params! {
                    ":session_id": session_id,
                    ":interaction_request": EntryKind::InteractionRequest,
                    ":interaction_response": EntryKind::InteractionResponse,
                    ":event": EntryKind::Event,
                    ":tool_load": EntryKind::ToolLoad,
                },
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten();
        Ok(v)
    }

    /// The next turn number. **Must be called while holding the session lock** — the engine
    /// holds it across the whole turn, so this does not lock again.
    pub fn next_turn_seq(&self, session_id: SessionId) -> Result<i64> {
        let max: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(turn_seq), 0) FROM session_entry WHERE session_id = :session_id",
            named_params! { ":session_id": session_id },
            |r| r.get(0),
        )?;
        Ok(max + 1)
    }

    /// Which turn number a turn id belongs to.
    /// `None` when that turn has no entries yet — a turn whose input was empty and whose first
    /// action is a permission prompt. Callers that need a number anyway should use
    /// [`EntryStore::next_turn_seq`], but they must not *guess* it as "the last one": between two
    /// turns those differ, and an entry filed under the wrong turn number is invisible to rewind.
    pub fn turn_seq_of(&self, session_id: SessionId, turn_id: TurnId) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT turn_seq FROM session_entry
                 WHERE session_id = :session_id AND turn_id = :turn_id LIMIT 1",
                named_params! { ":session_id": session_id, ":turn_id": turn_id },
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Unanswered interaction requests **across every session**, oldest first.
    /// For startup reconciliation: a process that exited while waiting on a prompt left a request
    /// with no response, and nothing else can tell that the waiter is gone. Callers must still
    /// check whether the session has a live lock — a request belonging to another host's running
    /// turn is not orphaned, it is being waited on right now.
    /// The pairing is done in Rust rather than with `json_extract`, same as
    /// [`Self::pending_interactions`]: the id lives inside `data`, and nothing else in this store
    /// relies on SQLite's JSON functions. The scan is bounded by the two interaction kinds and runs
    /// once at startup.
    pub fn unanswered_interactions(&self) -> Result<Vec<PendingInteraction>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {COLS} FROM session_entry
             WHERE kind IN ('interaction_request', 'interaction_response')
             ORDER BY session_id, seq"
        ))?;
        let rows: Vec<EntryRecord> = st
            .query_map([], map_row)?
            .collect::<rusqlite::Result<_>>()?;

        // Keyed by session **and** interaction id: ids are unique in practice, but a response can
        // only ever answer a request in its own session, and relying on global uniqueness here
        // would make one duplicated id silence a real prompt in another session.
        let answered: std::collections::HashSet<(SessionId, String)> = rows
            .iter()
            .filter(|e| e.kind == EntryKind::InteractionResponse)
            .filter_map(|e| interaction_id_of(e).map(|id| (e.session_id, id)))
            .collect();

        Ok(rows
            .into_iter()
            .filter(|e| e.kind == EntryKind::InteractionRequest)
            .filter_map(|e| {
                let id = interaction_id_of(&e)?;
                (!answered.contains(&(e.session_id, id.clone()))).then_some(PendingInteraction {
                    request: e,
                    interaction_id: id,
                })
            })
            .collect())
    }

    /// Interaction requests with no response yet, oldest first.
    /// Cheap despite being a scan: only the live turn can have pending prompts, and a single
    /// turn holds a handful of entries.
    pub fn pending_interactions(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
    ) -> Result<Vec<PendingInteraction>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {COLS} FROM session_entry
             WHERE session_id = :session_id AND turn_id = :turn_id
               AND kind IN ('interaction_request', 'interaction_response')
             ORDER BY seq"
        ))?;
        let rows: Vec<EntryRecord> = self.attach_objects(
            st.query_map(
                named_params! { ":session_id": session_id, ":turn_id": turn_id },
                map_row,
            )?
            .collect::<rusqlite::Result<_>>()?,
        )?;

        let answered: Vec<String> = rows
            .iter()
            .filter(|e| e.kind == EntryKind::InteractionResponse)
            .filter_map(interaction_id_of)
            .collect();

        Ok(rows
            .into_iter()
            .filter(|e| e.kind == EntryKind::InteractionRequest)
            .filter_map(|e| {
                let id = interaction_id_of(&e)?;
                (!answered.contains(&id)).then_some(PendingInteraction {
                    request: e,
                    interaction_id: id,
                })
            })
            .collect())
    }

    pub fn conversation_stats_for(&self, session_id: SessionId) -> Result<ConversationStats> {
        let (turns, last_at) = self
            .conn
            .query_row(
                "SELECT turn_count, last_message_at FROM session
                 WHERE session_id = :session_id",
                named_params! { ":session_id": session_id },
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<DateTime<Utc>>>(1)?)),
            )
            .optional()?
            .unwrap_or_default();
        Ok(ConversationStats {
            turn_count: turns as u32,
            last_message_at: last_at,
        })
    }

    pub fn conversation_stats_for_ids(
        &self,
        ids: &[SessionId],
    ) -> Result<HashMap<SessionId, ConversationStats>> {
        let mut out = HashMap::new();
        for chunk in ids.chunks(500) {
            let placeholders = vec!["?"; chunk.len()].join(", ");
            let mut st = self.conn.prepare(&format!(
                "SELECT session_id, turn_count, last_message_at
                 FROM session WHERE session_id IN ({placeholders})"
            ))?;
            let rows = st.query_map(rusqlite::params_from_iter(chunk.iter().copied()), |r| {
                Ok((
                    r.get::<_, SessionId>("session_id")?,
                    ConversationStats {
                        turn_count: r.get::<_, i64>("turn_count")? as u32,
                        last_message_at: r.get::<_, Option<DateTime<Utc>>>("last_message_at")?,
                    },
                ))
            })?;
            out.extend(rows.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        Ok(out)
    }

    pub fn awaiting_interactions_in(
        &self,
        ids: &[SessionId],
    ) -> Result<std::collections::HashSet<SessionId>> {
        use std::collections::HashSet;

        let mut out = HashSet::new();

        for chunk in ids.chunks(500) {
            let placeholders = std::iter::repeat_n("?", chunk.len())
                .collect::<Vec<_>>()
                .join(", ");

            let sql = format!(
                "SELECT session_id
             FROM session_entry
             WHERE session_id IN ({placeholders})
               AND kind IN ('interaction_request', 'interaction_response')
             GROUP BY session_id
             HAVING
                 MAX(CASE WHEN kind = 'interaction_request' THEN seq END) IS NOT NULL
                 AND (
                     MAX(CASE WHEN kind = 'interaction_response' THEN seq END) IS NULL
                     OR MAX(CASE WHEN kind = 'interaction_request' THEN seq END)
                        > MAX(CASE WHEN kind = 'interaction_response' THEN seq END)
                 )"
            );

            let mut st = self.conn.prepare(&sql)?;

            let rows = st.query_map(rusqlite::params_from_iter(chunk.iter().copied()), |r| {
                r.get::<_, SessionId>(0)
            })?;

            for sid in rows {
                out.insert(sid?);
            }
        }

        Ok(out)
    }

    /// Rewind: drops entries with `turn_seq > keep_through_turn`. Returns how many went.
    pub fn rewind(&self, session_id: SessionId, keep_through_turn: i64) -> Result<usize> {
        // Recomputed in the same commit as the delete: a reader must never observe the rows gone
        // but `turn_count` / `last_message_at` stale.
        let tx = crate::tx_or_join(self.conn)?;
        let n = self.conn.execute(
            "DELETE FROM session_entry
             WHERE session_id = :session_id AND turn_seq > :keep",
            named_params! { ":session_id": session_id, ":keep": keep_through_turn },
        )?;
        self.recompute_stats(session_id)?;
        if let Some(tx) = tx {
            tx.commit()?;
        }
        Ok(n)
    }

    /// Fork support: copies every entry with `turn_seq <= keep_through_turn` into `to`.
    /// Rows are copied verbatim (`seq`, turn, round, kind, data, native, source, display,
    /// created_at) under fresh entry ids, and their `entry_object` references are re-pointed at
    /// the copies — the objects themselves are content-addressed and shared, never duplicated.
    /// `seq` can be kept as-is because the target is a fresh session with no rows of its own.
    /// Usage events are deliberately **not** copied: the spend belongs to the session it
    /// happened in, and a fork that duplicated it would double every /stats total.
    pub fn copy_through(
        &self,
        from: SessionId,
        to: SessionId,
        keep_through_turn: i64,
    ) -> Result<usize> {
        // One transaction: a half-copied fork would render as a session that silently ends
        // mid-conversation.
        let tx = crate::tx_or_join(self.conn)?;
        let old_ids: Vec<EntryId> = {
            let mut st = self.conn.prepare(
                "SELECT entry_id FROM session_entry
                 WHERE session_id = :from AND turn_seq <= :keep ORDER BY seq",
            )?;
            let rows = st.query_map(
                named_params! { ":from": from, ":keep": keep_through_turn },
                |r| r.get(0),
            )?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        for old in &old_ids {
            let new_id = EntryId::new();
            self.conn.execute(
                "INSERT INTO session_entry
                    (entry_id, session_id, seq, turn_seq, turn_id, round_id, round_seq, kind,
                     data, native, source, display, created_at)
                 SELECT :new_id, :to, seq, turn_seq, turn_id, round_id, round_seq, kind,
                        data, native, source, display, created_at
                 FROM session_entry WHERE entry_id = :old",
                named_params! { ":new_id": new_id, ":to": to, ":old": old },
            )?;
            self.conn.execute(
                "INSERT INTO entry_object (entry_id, object_id, role, ref_key)
                 SELECT :new_id, object_id, role, ref_key
                 FROM entry_object WHERE entry_id = :old",
                named_params! { ":new_id": new_id, ":old": old },
            )?;
        }
        // Fork target is a fresh session with no locks, so the full rebuild == the maintained
        // columns. Must land in the same commit as the copy.
        self.recompute_stats(to)?;
        if let Some(tx) = tx {
            tx.commit()?;
        }
        Ok(old_ids.len())
    }

    /// Re-derives a session's `turn_count` / `last_message_at` from `session_entry`, taking the
    /// current live lock (if any) into account.
    /// The columns are maintained incrementally on [`EntryStore::append`] and on lock steal /
    /// release; this authoritative rebuild is used by `rewind`, fork copy and by callers that
    /// insert rows directly. It is safe to call for sessions with no rows (writes the defaults).
    pub fn recompute_stats(&self, session_id: SessionId) -> Result<()> {
        self.conn.execute(
            "UPDATE session SET
                 turn_count = (
                   SELECT COUNT(DISTINCT e.turn_seq) FROM session_entry e
                   WHERE e.session_id = :session_id AND e.kind != 'event'
                 ),
                 last_message_at = (
                   SELECT MAX(e.created_at) FROM session_entry e
                   WHERE e.session_id = :session_id AND e.kind != 'event'
                     AND NOT EXISTS (
                       SELECT 1 FROM session_locks l
                       WHERE l.session_id = :session_id AND l.turn_id = e.turn_id
                     )
                 )
               WHERE session_id = :session_id",
            named_params! { ":session_id": session_id },
        )?;
        Ok(())
    }

    /// Every object still referenced by this session — what a cleanup pass must keep.
    /// One index scan over the join table. Deriving this from `data` would mean parsing every
    /// entry's JSON, and for offloaded entries fetching the payload back just to read its
    /// references.
    pub fn referenced_objects(&self, session_id: SessionId) -> Result<Vec<ObjectId>> {
        let mut st = self.conn.prepare(
            "SELECT DISTINCT eo.object_id
             FROM entry_object eo
             JOIN session_entry se ON se.entry_id = eo.entry_id
             WHERE se.session_id = :session_id",
        )?;
        Ok(st
            .query_map(named_params! { ":session_id": session_id }, |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Every object referenced anywhere. What a global GC sweep keeps.
    pub fn all_referenced_objects(&self) -> Result<Vec<ObjectId>> {
        let mut st = self
            .conn
            .prepare("SELECT DISTINCT object_id FROM entry_object")?;
        Ok(st
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// **The question GC asks.** Answered by an index on `object_id`, not by a scan.
    pub fn is_object_referenced(&self, object_id: &ObjectId) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM entry_object WHERE object_id = :object_id",
            named_params! { ":object_id": object_id },
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    fn attach_objects(&self, mut rows: Vec<EntryRecord>) -> Result<Vec<EntryRecord>> {
        for rec in &mut rows {
            rec.objects = self.objects_of(rec.entry_id)?;
        }
        Ok(rows)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TurnAsset {
    pub turn_seq: i64,
    pub object_id: String,
    pub kind: String,
    pub label: Option<String>,
    pub meta: Option<String>,
}

/// Pulls `interaction_id` out of an interaction entry's payload.
fn interaction_id_of(e: &EntryRecord) -> Option<String> {
    e.data.get("interaction_id")?.as_str().map(str::to_string)
}

fn derived_asset_kind(obj: &ObjectRef) -> &'static str {
    match obj.role {
        ObjectRole::Diff => "diff",
        ObjectRole::Output if obj.ref_key.as_deref() == Some("widget") => "widget",
        ObjectRole::Output if obj.ref_key.is_some() => "file",
        ObjectRole::Output => "output",
        ObjectRole::Attachment => "attachment",
        ObjectRole::Skill => "skill",
        ObjectRole::Payload => "payload",
    }
}

const COLS: &str = "entry_id, session_id, seq, turn_seq, turn_id, round_id, round_seq, kind,
     data, native, source, display, is_final, created_at";

fn map_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<EntryRecord> {
    Ok(EntryRecord {
        entry_id: r.get("entry_id")?,
        session_id: r.get("session_id")?,
        seq: r.get("seq")?,
        turn_seq: r.get("turn_seq")?,
        turn_id: r.get("turn_id")?,
        round_id: r.get("round_id")?,
        round_seq: r.get("round_seq")?,
        // An unknown kind fails the read. Skipping it silently would leave a hole in the
        // history, and a missing tool_result is the next round's 400.
        kind: r.get("kind")?,
        data: r.get::<_, Json<Value>>("data")?.0,
        native: r.get::<_, Option<Json<Value>>>("native")?.map(|j| j.0),
        source: r.get::<_, Option<Json<Source>>>("source")?.map(|j| j.0),
        display: r.get::<_, Option<Json<Value>>>("display")?.map(|j| j.0),
        is_final: r.get("is_final")?,
        // Filled in by `attach_objects`: they come from a second table.
        objects: Vec::new(),
        created_at: r.get("created_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Db, NewSession};
    use serde_json::json;
    use zlogic_objects::MemoryObjectStore;
    use zlogic_protocol::WorkspaceId;

    fn setup() -> (Db, SessionId) {
        let db = Db::open_in_memory().unwrap();
        let s = db
            .sessions()
            .create(NewSession::root(WorkspaceId::new()))
            .unwrap();
        (db, s.session_id)
    }

    fn entry(sid: SessionId, turn: i64, kind: EntryKind, data: Value) -> NewEntry {
        NewEntry::new(sid, TurnId::new(), turn, kind, data)
    }

    #[test]
    fn the_display_projection_round_trips_without_touching_data() {
        let (db, sid) = setup();
        let part = json!({ "type": "tool_result", "call_id": "c1", "name": "edit",
                           "content": "changed a.rs", "is_error": false });
        let display = json!({
            "status": "denied",
            "cards": [{ "type": "diff", "path": "a.rs" }],
        });

        let rec = db
            .entries()
            .append(
                entry(sid, 1, EntryKind::ToolResult, part.clone())
                    .in_round(RoundId::new())
                    .with_round_seq(3)
                    .with_display(display.clone()),
            )
            .unwrap();

        assert_eq!(rec.display, Some(display));
        assert_eq!(rec.round_seq, Some(3), "round_seq survives the round trip");
        assert_eq!(rec.data, part);

        let read = db.entries().get(rec.entry_id).unwrap().unwrap();
        assert_eq!(read.display, rec.display);
        assert_eq!(read.round_seq, rec.round_seq);
    }

    #[test]
    fn copy_through_copies_the_kept_turns_and_repoints_object_references() {
        let (db, from) = setup();
        let to = db
            .sessions()
            .create(NewSession::root(WorkspaceId::new()))
            .unwrap()
            .session_id;

        db.entries()
            .append(entry(
                from,
                1,
                EntryKind::User,
                json!({"type":"text","text":"one"}),
            ))
            .unwrap();
        db.entries()
            .append(
                entry(
                    from,
                    1,
                    EntryKind::ToolResult,
                    json!({"type":"tool_result"}),
                )
                .references(crate::ObjectRef::payload(
                    format!("sha256:{}", "ab".repeat(32)).parse().unwrap(),
                )),
            )
            .unwrap();
        db.entries()
            .append(entry(
                from,
                2,
                EntryKind::User,
                json!({"type":"text","text":"two"}),
            ))
            .unwrap();

        let copied = db.entries().copy_through(from, to, 1).unwrap();
        assert_eq!(copied, 2, "turn 2 stays behind");

        let originals = db.entries().list(from).unwrap();
        let copies = db.entries().list(to).unwrap();
        assert_eq!(originals.len(), 3, "the source is untouched");
        assert_eq!(copies.len(), 2);
        for (a, b) in originals.iter().take(2).zip(&copies) {
            assert_ne!(a.entry_id, b.entry_id, "fresh ids");
            assert_eq!(a.seq, b.seq);
            assert_eq!(a.turn_seq, b.turn_seq);
            assert_eq!(a.kind, b.kind);
            assert_eq!(a.data, b.data);
            assert_eq!(a.round_id, b.round_id);
            assert_eq!(a.round_seq, b.round_seq);
        }
        assert_eq!(
            db.entries().referenced_objects(to).unwrap(),
            db.entries().referenced_objects(from).unwrap(),
        );
    }

    #[test]
    fn entries_without_a_projection_read_back_as_none() {
        let (db, sid) = setup();
        let rec = db
            .entries()
            .append(entry(sid, 1, EntryKind::AssistantText, json!("hi")))
            .unwrap();
        assert_eq!(rec.display, None);
    }

    #[test]
    fn seq_is_allocated_by_the_store_and_is_dense() {
        let (db, sid) = setup();
        for i in 0..5 {
            let r = db
                .entries()
                .append(entry(sid, 1, EntryKind::AssistantText, json!({ "i": i })))
                .unwrap();
            assert_eq!(r.seq, i + 1);
        }
    }

    /// Many entries share one turn_seq, so it must not be unique.
    #[test]
    fn many_entries_share_one_turn_seq() {
        let (db, sid) = setup();
        let e = db.entries();
        e.append(entry(sid, 1, EntryKind::User, json!("hi")))
            .unwrap();
        e.append(entry(sid, 1, EntryKind::Thinking, json!("…")))
            .unwrap();
        e.append(entry(sid, 1, EntryKind::AssistantText, json!("ok")))
            .unwrap();
        assert_eq!(e.list_turn(sid, 1).unwrap().len(), 3);
    }

    #[test]
    fn native_and_source_survive_verbatim() {
        let (db, sid) = setup();
        let native = json!({ "type": "thinking", "signature": "EqoBCk+/=" });
        let source = Source::new("anthropic", "claude-opus-5");

        db.entries()
            .append(
                entry(sid, 1, EntryKind::Thinking, json!("display projection"))
                    .with_native(native.clone())
                    .from_model(source.clone()),
            )
            .unwrap();

        let got = &db.entries().list(sid).unwrap()[0];
        assert_eq!(
            got.native.as_ref(),
            Some(&native),
            "raw must come back byte for byte"
        );
        assert_eq!(
            got.source.as_ref(),
            Some(&source),
            "typed, not a loose blob"
        );
    }

    #[test]
    fn kind_round_trips_through_its_wire_name() {
        let (db, sid) = setup();
        db.entries()
            .append(entry(sid, 1, EntryKind::AssistantText, json!("x")))
            .unwrap();
        let raw: String = db
            .conn()
            .query_row("SELECT kind FROM session_entry", [], |r| r.get(0))
            .unwrap();
        assert_eq!(raw, "assistant_text");
    }

    /// An unknown kind must be loud: a silently dropped tool_result is the next 400.
    #[test]
    fn unknown_kind_is_reported_not_silently_skipped() {
        let (db, sid) = setup();
        db.entries()
            .append(entry(sid, 1, EntryKind::User, json!("a")))
            .unwrap();
        db.conn()
            .execute("UPDATE session_entry SET kind = 'from_the_future'", [])
            .unwrap();
        assert!(db.entries().list(sid).is_err());
    }

    // ── interactions as entries ────────────────────────────────────────────

    #[test]
    fn interactions_are_entries_and_show_up_in_the_timeline() {
        let (db, sid) = setup();
        let turn = TurnId::new();
        let e = db.entries();

        let req = json!({
            "interaction_id": "i-1",
            "body": { "type": "permission", "tool": "shell", "args_preview": "rm -rf build" }
        });
        e.append(NewEntry::new(
            sid,
            turn,
            1,
            EntryKind::InteractionRequest,
            req,
        ))
        .unwrap();

        let all = e.list(sid).unwrap();
        assert_eq!(all.len(), 1, "the timeline sees it in place");
        assert_eq!(all[0].kind, EntryKind::InteractionRequest);
    }

    #[test]
    fn pending_means_a_request_with_no_response() {
        let (db, sid) = setup();
        let turn = TurnId::new();
        let e = db.entries();

        for id in ["i-1", "i-2"] {
            e.append(NewEntry::new(
                sid,
                turn,
                1,
                EntryKind::InteractionRequest,
                json!({ "interaction_id": id, "body": {} }),
            ))
            .unwrap();
        }
        assert_eq!(e.pending_interactions(sid, turn).unwrap().len(), 2);

        e.append(NewEntry::new(
            sid,
            turn,
            1,
            EntryKind::InteractionResponse,
            json!({ "interaction_id": "i-1", "decision": { "type": "allow", "scope": "once" } }),
        ))
        .unwrap();

        let pending = e.pending_interactions(sid, turn).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].interaction_id, "i-2");
    }

    #[test]
    fn awaiting_interactions_in_pairs_by_session_in_seq_order() {
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
        let c = db
            .sessions()
            .create(NewSession::root(ws))
            .unwrap()
            .session_id;
        let e = db.entries();
        let turn = TurnId::new();

        e.append(NewEntry::new(
            a,
            turn,
            1,
            EntryKind::InteractionRequest,
            json!({ "interaction_id": "i-a", "body": {} }),
        ))
        .unwrap();
        e.append(NewEntry::new(
            b,
            turn,
            1,
            EntryKind::InteractionRequest,
            json!({ "interaction_id": "i-b", "body": {} }),
        ))
        .unwrap();
        e.append(NewEntry::new(
            b,
            turn,
            1,
            EntryKind::InteractionResponse,
            json!({ "interaction_id": "i-b" }),
        ))
        .unwrap();
        e.append(NewEntry::new(c, turn, 1, EntryKind::User, json!("hi")))
            .unwrap();

        let awaiting = e.awaiting_interactions_in(&[a, b, c]).unwrap();
        assert_eq!(awaiting.len(), 1);
        assert!(awaiting.contains(&a));
        assert!(!awaiting.contains(&b), "answered must not be reported");
        assert!(!awaiting.contains(&c));

        let a_only = e.awaiting_interactions_in(&[b, c]).unwrap();
        assert!(a_only.is_empty());

        e.append(NewEntry::new(
            a,
            turn,
            1,
            EntryKind::InteractionResponse,
            json!({ "interaction_id": "i-a" }),
        ))
        .unwrap();
        assert!(e.awaiting_interactions_in(&[a]).unwrap().is_empty());
    }

    /// Interactions never reach the model; everything else does.
    #[test]
    fn context_excludes_interactions_and_notices() {
        let (db, sid) = setup();
        let turn = TurnId::new();
        let e = db.entries();

        e.append(NewEntry::new(sid, turn, 1, EntryKind::User, json!("hi")))
            .unwrap();
        e.append(NewEntry::new(
            sid,
            turn,
            1,
            EntryKind::InteractionRequest,
            json!({ "interaction_id": "i-1" }),
        ))
        .unwrap();
        e.append(NewEntry::new(
            sid,
            turn,
            1,
            EntryKind::InteractionResponse,
            json!({ "interaction_id": "i-1" }),
        ))
        .unwrap();
        e.append(NewEntry::new(
            sid,
            turn,
            1,
            EntryKind::Event,
            json!("notice"),
        ))
        .unwrap();
        e.append(NewEntry::new(
            sid,
            turn,
            1,
            EntryKind::AssistantText,
            json!("ok"),
        ))
        .unwrap();

        assert_eq!(
            e.list(sid).unwrap().len(),
            5,
            "the timeline gets everything"
        );
        let ctx: Vec<EntryKind> = e
            .list_for_context(sid)
            .unwrap()
            .iter()
            .map(|x| x.kind)
            .collect();
        assert_eq!(ctx, [EntryKind::User, EntryKind::AssistantText]);
    }

    #[test]
    fn compacted_prefix_query_keeps_state_and_summaries() {
        let (db, sid) = setup();
        let turn = TurnId::new();
        let entries = db.entries();
        entries
            .append(NewEntry::new(
                sid,
                turn,
                1,
                EntryKind::User,
                json!("covered"),
            ))
            .unwrap();
        entries
            .append(NewEntry::new(
                sid,
                turn,
                1,
                EntryKind::SkillLoad,
                json!({"type": "skill_load"}),
            ))
            .unwrap();
        entries
            .append(NewEntry::new(
                sid,
                turn,
                1,
                EntryKind::SkillUnload,
                json!({"type": "skill_unload"}),
            ))
            .unwrap();
        entries
            .append(NewEntry::new(sid, turn, 2, EntryKind::User, json!("tail")))
            .unwrap();
        entries
            .append(NewEntry::new(
                sid,
                turn,
                2,
                EntryKind::Compaction,
                json!({"from_turn": 1, "to_turn": 1}),
            ))
            .unwrap();

        let kinds: Vec<_> = entries
            .list_for_context_after(sid, 1)
            .unwrap()
            .into_iter()
            .map(|entry| entry.kind)
            .collect();
        assert_eq!(
            kinds,
            [
                EntryKind::SkillLoad,
                EntryKind::SkillUnload,
                EntryKind::User,
                EntryKind::Compaction
            ]
        );
    }

    #[test]
    fn every_kind_declares_whether_it_reaches_the_model() {
        // Guards against a new kind being added without deciding this.
        for k in EntryKind::ALL {
            let expected = !matches!(
                k,
                EntryKind::InteractionRequest
                    | EntryKind::InteractionResponse
                    | EntryKind::Event
                    | EntryKind::ToolLoad
            );
            assert_eq!(k.goes_to_model(), expected, "{k}");
        }
    }

    // ── offloading ─────────────────────────────────────────────────────────

    #[test]
    fn large_payloads_are_offloaded_and_the_row_stays_small() {
        let (db, sid) = setup();
        let objects = MemoryObjectStore::new();
        let big = json!({ "content": "x".repeat(INLINE_LIMIT * 3) });

        let rec = db
            .entries()
            .append_with_offload(entry(sid, 1, EntryKind::ToolResult, big.clone()), &objects)
            .unwrap();

        assert!(rec.is_offloaded());
        assert_eq!(rec.data, Value::Null);
        // Recorded as a payload reference, which is how it is found again and kept alive.
        assert_eq!(rec.objects.len(), 1);
        assert_eq!(rec.objects[0].role, ObjectRole::Payload);

        let stored: String = db
            .conn()
            .query_row(
                "SELECT data FROM session_entry WHERE entry_id = :id",
                named_params! { ":id": rec.entry_id },
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, "null");

        assert_eq!(db.entries().load_data(&rec, &objects).unwrap(), big);
        assert_eq!(db.entries().referenced_objects(sid).unwrap().len(), 1);
    }

    /// The reason a single column was not enough.
    #[test]
    fn one_entry_can_reference_several_objects() {
        let (db, sid) = setup();
        let objects = MemoryObjectStore::new();
        let stdout = objects.put(b"a very long build log").unwrap();
        let diff = objects.put(b"@@ -1 +1 @@").unwrap();
        let attachment = objects.put(b"screenshot bytes").unwrap();

        let rec = db
            .entries()
            .append(
                entry(sid, 1, EntryKind::ToolResult, json!({ "summary": "built" }))
                    .references(ObjectRef::keyed(
                        stdout.clone(),
                        ObjectRole::Output,
                        "stdout",
                    ))
                    .references(ObjectRef::keyed(
                        diff.clone(),
                        ObjectRole::Diff,
                        "src/main.rs",
                    ))
                    .references(ObjectRef::new(attachment.clone(), ObjectRole::Attachment)),
            )
            .unwrap();

        assert_eq!(rec.objects.len(), 3);
        assert!(
            !rec.is_offloaded(),
            "none of these is the entry's own payload"
        );
        assert_eq!(
            rec.objects_with_role(ObjectRole::Output)
                .collect::<Vec<_>>(),
            [&stdout]
        );
        // The ref_key ties a reference back to its place in `data`.
        let d = rec
            .objects
            .iter()
            .find(|o| o.role == ObjectRole::Diff)
            .unwrap();
        assert_eq!(d.ref_key.as_deref(), Some("src/main.rs"));

        // All three are enumerable for GC without parsing any JSON.
        let mut refs = db.entries().referenced_objects(sid).unwrap();
        refs.sort_by_key(|o| o.to_string());
        let mut want = vec![stdout, diff, attachment];
        want.sort_by_key(|o| o.to_string());
        assert_eq!(refs, want);
    }

    #[test]
    fn asset_kind_is_derived_from_role_and_ref_key() {
        let (db, sid) = setup();
        let objects = MemoryObjectStore::new();
        let log = objects.put(b"build log").unwrap();
        let patch = objects.put(b"@@ -1 +1 @@").unwrap();
        let shot = objects.put(b"png").unwrap();
        let widget = objects.put(b"widget source").unwrap();
        let file = objects.put(b"report bytes").unwrap();

        let rec = db
            .entries()
            .append(
                entry(sid, 1, EntryKind::ToolResult, json!({ "summary": "ran" }))
                    .references(ObjectRef::new(log.clone(), ObjectRole::Output))
                    .references(ObjectRef::keyed(
                        patch.clone(),
                        ObjectRole::Diff,
                        "src/main.rs",
                    ))
                    .references(ObjectRef::new(shot.clone(), ObjectRole::Attachment))
                    .references(ObjectRef::keyed(
                        widget.clone(),
                        ObjectRole::Output,
                        "widget",
                    ))
                    .references(ObjectRef::keyed(
                        file.clone(),
                        ObjectRole::Output,
                        "report.csv",
                    )),
            )
            .unwrap();

        let kind_of = |id: &ObjectId| {
            rec.objects
                .iter()
                .find(|o| &o.object_id == id)
                .and_then(|o| o.kind.clone())
        };
        assert_eq!(kind_of(&log).as_deref(), Some("output"));
        assert_eq!(kind_of(&patch).as_deref(), Some("diff"));
        assert_eq!(kind_of(&shot).as_deref(), Some("attachment"));
        assert_eq!(kind_of(&widget).as_deref(), Some("widget"));
        assert_eq!(kind_of(&file).as_deref(), Some("file"));
    }

    #[test]
    fn explicit_classification_carries_label_and_meta() {
        let (db, sid) = setup();
        let objects = MemoryObjectStore::new();
        let source = objects.put(b"widget source").unwrap();

        db.entries()
            .append(
                entry(sid, 1, EntryKind::ToolResult, json!({ "summary": "chart" })).references(
                    ObjectRef::classified(
                        source.clone(),
                        ObjectRole::Output,
                        "widget",
                        Some("Revenue".into()),
                        Some(json!({ "height": 420, "libraries": ["chart"] }).to_string()),
                    ),
                ),
            )
            .unwrap();

        let got = db
            .entries()
            .assets_of_turns(sid, &[1], &["widget"])
            .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].object_id, source.to_string());
        assert_eq!(got[0].label.as_deref(), Some("Revenue"));
        let meta: Value = serde_json::from_str(got[0].meta.as_deref().unwrap()).unwrap();
        assert_eq!(meta["height"], 420);
        assert_eq!(meta["libraries"][0], "chart");
    }

    #[test]
    fn assets_of_turns_filters_by_kind_and_scopes_to_the_turn() {
        let (db, sid) = setup();
        let other = db
            .sessions()
            .create(NewSession::root(WorkspaceId::new()))
            .unwrap()
            .session_id;
        let objects = MemoryObjectStore::new();
        let first = objects.put(b"first chart").unwrap();
        let second = objects.put(b"second chart").unwrap();
        let elsewhere = objects.put(b"another session's chart").unwrap();
        let log = objects.put(b"build log").unwrap();

        let widget_ref = |id: &ObjectId| {
            ObjectRef::classified(
                id.clone(),
                ObjectRole::Output,
                "widget",
                Some("Chart".into()),
                None,
            )
        };
        db.entries()
            .append(
                entry(sid, 1, EntryKind::ToolResult, json!({ "summary": "a" }))
                    .references(widget_ref(&first))
                    .references(ObjectRef::new(log.clone(), ObjectRole::Output)),
            )
            .unwrap();
        db.entries()
            .append(
                entry(sid, 2, EntryKind::ToolResult, json!({ "summary": "b" }))
                    .references(widget_ref(&second)),
            )
            .unwrap();
        db.entries()
            .append(
                entry(other, 1, EntryKind::ToolResult, json!({ "summary": "c" }))
                    .references(widget_ref(&elsewhere)),
            )
            .unwrap();

        let one = db
            .entries()
            .assets_of_turns(sid, &[1], &["widget"])
            .unwrap();
        assert_eq!(
            one.iter().map(|a| a.object_id.clone()).collect::<Vec<_>>(),
            [first.to_string()]
        );
        assert_eq!(one[0].turn_seq, 1);
        let both = db
            .entries()
            .assets_of_turns(sid, &[1, 2], &["widget"])
            .unwrap();
        assert_eq!(
            both.iter().map(|a| a.object_id.clone()).collect::<Vec<_>>(),
            [first.to_string(), second.to_string()]
        );
        assert_eq!(
            db.entries()
                .assets_of_turns(other, &[1], &["widget"])
                .unwrap()
                .len(),
            1
        );
        assert!(
            db.entries()
                .assets_of_turns(sid, &[], &["widget"])
                .unwrap()
                .is_empty()
        );
        assert!(
            db.entries()
                .assets_of_turns(sid, &[1, 2], &[])
                .unwrap()
                .is_empty()
        );
        assert!(
            db.entries()
                .assets_of_turns(sid, &[1, 2], &["file"])
                .unwrap()
                .is_empty()
        );
        let union = db
            .entries()
            .assets_of_turns(sid, &[1, 2], &["widget", "output"])
            .unwrap();
        assert_eq!(
            union.iter().map(|a| a.kind.as_str()).collect::<Vec<_>>(),
            ["widget", "output", "widget"]
        );
    }

    /// GC's actual question, answered by an index rather than a scan.
    #[test]
    fn object_reference_lookup_is_reverse_indexed() {
        let (db, sid) = setup();
        let objects = MemoryObjectStore::new();
        let live = objects.put(b"referenced").unwrap();
        let orphan = objects.put(b"nobody wants me").unwrap();

        db.entries()
            .append(
                entry(sid, 1, EntryKind::ToolResult, json!({}))
                    .references(ObjectRef::new(live.clone(), ObjectRole::Output)),
            )
            .unwrap();

        assert!(db.entries().is_object_referenced(&live).unwrap());
        assert!(!db.entries().is_object_referenced(&orphan).unwrap());
        assert_eq!(db.entries().all_referenced_objects().unwrap(), [live]);
    }

    /// `load_data` has to find exactly one payload, so two would be ambiguous.
    #[test]
    fn an_entry_cannot_have_two_payload_objects() {
        let (db, sid) = setup();
        let objects = MemoryObjectStore::new();
        let a = objects.put(b"one").unwrap();
        let b = objects.put(b"two").unwrap();

        let err = db
            .entries()
            .append(
                entry(sid, 1, EntryKind::ToolResult, json!({}))
                    .references(ObjectRef::new(a, ObjectRole::Payload))
                    .references(ObjectRef::new(b, ObjectRole::Payload)),
            )
            .unwrap_err();
        assert!(matches!(err, StoreError::Corrupt(_)));
    }

    /// The entry and its references are written together, so neither can exist alone.
    #[test]
    fn references_are_removed_with_their_entry() {
        let (db, sid) = setup();
        let objects = MemoryObjectStore::new();
        let id = objects.put(b"x").unwrap();
        db.entries()
            .append(
                entry(sid, 1, EntryKind::ToolResult, json!({}))
                    .references(ObjectRef::new(id.clone(), ObjectRole::Output)),
            )
            .unwrap();

        db.entries().rewind(sid, 0).unwrap();
        assert!(
            !db.entries().is_object_referenced(&id).unwrap(),
            "a rewound entry must not leave its references behind"
        );

        let n: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM entry_object", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn object_role_round_trips_through_its_wire_name() {
        let (db, sid) = setup();
        let objects = MemoryObjectStore::new();
        let id = objects.put(b"x").unwrap();
        db.entries()
            .append(
                entry(sid, 1, EntryKind::ToolResult, json!({}))
                    .references(ObjectRef::new(id, ObjectRole::Attachment)),
            )
            .unwrap();
        let raw: String = db
            .conn()
            .query_row("SELECT role FROM entry_object", [], |r| r.get(0))
            .unwrap();
        assert_eq!(raw, "attachment");
    }

    #[test]
    fn small_payloads_stay_inline() {
        let (db, sid) = setup();
        let objects = MemoryObjectStore::new();
        let rec = db
            .entries()
            .append_with_offload(
                entry(sid, 1, EntryKind::AssistantText, json!("short")),
                &objects,
            )
            .unwrap();
        assert!(!rec.is_offloaded());
        assert!(rec.objects.is_empty());
        assert!(objects.is_empty());
        assert_eq!(
            db.entries().load_data(&rec, &objects).unwrap(),
            json!("short")
        );
    }

    #[test]
    fn turn_end_stamps_the_last_text_row_as_final() {
        let (db, sid) = setup();
        let e = db.entries();
        let turn = TurnId::new();
        e.append(NewEntry::new(sid, turn, 1, EntryKind::User, json!("hi")))
            .unwrap();
        e.append(NewEntry::new(
            sid,
            turn,
            1,
            EntryKind::AssistantText,
            json!({"type":"text","text":"mid"}),
        ))
        .unwrap();
        e.append(NewEntry::new(
            sid,
            turn,
            1,
            EntryKind::AssistantText,
            json!({"type":"text","text":"final"}),
        ))
        .unwrap();
        let objects = MemoryObjectStore::default();
        let end = NewEntry::new(
            sid,
            turn,
            1,
            EntryKind::Event,
            json!({"type":"turn_end","status":"completed"}),
        );
        e.append_turn_end(end, &objects).unwrap();

        let finals: Vec<_> = e
            .list(sid)
            .unwrap()
            .into_iter()
            .filter(|r| r.is_final)
            .collect();
        assert_eq!(finals.len(), 1, "exactly one final per round");
        assert_eq!(
            finals[0].data["text"], "final",
            "it marks the last text-like row"
        );
        let dup = NewEntry::new(
            sid,
            turn,
            1,
            EntryKind::Event,
            json!({"type":"turn_end","status":"completed"}),
        );
        e.append_turn_end(dup, &objects).unwrap();
        assert_eq!(
            e.list(sid)
                .unwrap()
                .into_iter()
                .filter(|r| r.is_final)
                .count(),
            1
        );
    }

    #[test]
    fn rewind_drops_later_turns_only() {
        let (db, sid) = setup();
        let e = db.entries();
        for t in 1..=4 {
            e.append(entry(sid, t, EntryKind::User, json!(t))).unwrap();
        }
        assert_eq!(e.rewind(sid, 2).unwrap(), 2);
        let left: Vec<i64> = e.list(sid).unwrap().iter().map(|r| r.turn_seq).collect();
        assert_eq!(left, [1, 2]);
    }

    #[test]
    fn next_turn_seq_counts_from_existing_rows() {
        let (db, sid) = setup();
        let e = db.entries();
        assert_eq!(e.next_turn_seq(sid).unwrap(), 1);
        e.append(entry(sid, 1, EntryKind::User, json!("a")))
            .unwrap();
        e.append(entry(sid, 2, EntryKind::User, json!("b")))
            .unwrap();
        assert_eq!(e.next_turn_seq(sid).unwrap(), 3);
    }

    #[test]
    fn seq_is_per_session() {
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
        let e = db.entries();
        e.append(entry(a, 1, EntryKind::User, json!(1))).unwrap();
        assert_eq!(
            e.append(entry(b, 1, EntryKind::User, json!(1)))
                .unwrap()
                .seq,
            1
        );
    }

    #[test]
    fn deleting_a_session_cascades_to_its_entries() {
        let (db, sid) = setup();
        db.entries()
            .append(entry(sid, 1, EntryKind::User, json!("a")))
            .unwrap();
        db.sessions().delete(sid).unwrap();
        let n: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM session_entry", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    // ── conversation stats ────────────────────────────────────────────────

    /// Appends one entry at a **fixed** wall-clock time and with an explicit turn id. The list
    /// ordering key is a time, so the tests that pin it must not depend on when they happen to run.
    fn entry_at(
        db: &Db,
        sid: SessionId,
        turn_seq: i64,
        turn_id: TurnId,
        kind: EntryKind,
        at: &str,
    ) {
        db.conn()
            .execute(
                "INSERT INTO session_entry (entry_id, session_id, seq, turn_seq, turn_id, kind,
                                            data, created_at)
                 VALUES (?1, ?2,
                         (SELECT COALESCE(MAX(seq), 0) + 1 FROM session_entry
                           WHERE session_id = ?2),
                         ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    EntryId::new().to_string(),
                    sid.to_string(),
                    turn_seq,
                    turn_id.to_string(),
                    kind.to_string(),
                    "null",
                    at,
                ],
            )
            .unwrap();
        // The raw insert bypasses [`EntryStore::append`]'s incremental maintenance, so rebuild the
        // `session` columns from the rows — the tests here pin exactly those derived values.
        db.entries().recompute_stats(sid).unwrap();
    }

    /// Conversation stats are about **completed** turns: the live turn (the one `session_locks`
    /// names) contributes no message time, so a session that is currently replying keeps the sort
    /// key of its last finished turn instead of jumping forward on every persisted chunk.
    #[test]
    fn last_message_at_skips_the_live_turn() {
        let (db, sid) = setup();
        let e = db.entries();
        let live = TurnId::new();

        let t1 = "2026-01-01T01:00:00Z";
        entry_at(&db, sid, 1, TurnId::new(), EntryKind::User, t1);
        db.locks().acquire(sid, live, Some("desktop")).unwrap();
        let t2 = "2026-01-01T02:00:00Z";
        entry_at(&db, sid, 2, live, EntryKind::User, t2);
        let stats = e.conversation_stats_for(sid).unwrap();
        assert_eq!(
            stats.turn_count, 2,
            "the round count still counts it -- a live round is a real round too"
        );
        assert_eq!(
            stats.last_message_at,
            Some(
                chrono::DateTime::parse_from_rfc3339(t1)
                    .unwrap()
                    .with_timezone(&Utc)
            ),
            "the time key goes back to the last **completed** turn (turn 1), not to a live turn's message"
        );

        let holder = db.locks().get(sid).unwrap().unwrap().holder_id;
        db.locks().release(sid, holder).unwrap();
        let stats = e.conversation_stats_for(sid).unwrap();
        assert_eq!(
            stats.last_message_at,
            Some(
                chrono::DateTime::parse_from_rfc3339(t2)
                    .unwrap()
                    .with_timezone(&Utc)
            ),
        );
    }

    #[test]
    fn last_message_at_ignores_events() {
        let (db, sid) = setup();
        let turn = TurnId::new();
        db.entries()
            .append(NewEntry::new(
                sid,
                turn,
                1,
                EntryKind::User,
                json!({"type":"text","text":"hi"}),
            ))
            .unwrap();
        let after = now() + chrono::Duration::seconds(10);
        db.conn()
            .execute(
                "UPDATE session_entry SET created_at = ?1 WHERE session_id = ?2 AND kind = 'user'",
                rusqlite::params![after.to_rfc3339(), sid.to_string(),],
            )
            .unwrap();
        // The direct timestamp rewrite bypasses the incremental maintenance; rebuild so the column
        // agrees with the rewritten rows.
        db.entries().recompute_stats(sid).unwrap();
        db.entries()
            .append(NewEntry::new(
                sid,
                turn,
                1,
                EntryKind::Event,
                json!({"level":"warn","code":"vision_reroute","message":{}}),
            ))
            .unwrap();
        let stats = db.entries().conversation_stats_for(sid).unwrap();
        assert_eq!(stats.turn_count, 1);
        assert_eq!(stats.last_message_at, Some(after));
    }

    #[test]
    fn batch_stats_skip_each_sessions_live_turn_only() {
        let db = Db::open_in_memory().unwrap();
        let ws = WorkspaceId::new();
        let finished = db
            .sessions()
            .create(NewSession::root(ws))
            .unwrap()
            .session_id;
        let running = db
            .sessions()
            .create(NewSession::root(ws))
            .unwrap()
            .session_id;

        let t1 = "2026-01-01T01:00:00Z";
        entry_at(&db, finished, 1, TurnId::new(), EntryKind::User, t1);
        let t2 = "2026-01-01T02:00:00Z";
        entry_at(&db, running, 1, TurnId::new(), EntryKind::User, t2);
        let live_turn = TurnId::new();
        db.locks()
            .acquire(running, live_turn, Some("desktop"))
            .unwrap();
        let t3 = "2026-01-01T03:00:00Z";
        entry_at(&db, running, 2, live_turn, EntryKind::User, t3);

        let stats = db
            .entries()
            .conversation_stats_for_ids(&[running, finished])
            .unwrap();
        let ts = |at: &str| {
            Some(
                chrono::DateTime::parse_from_rfc3339(at)
                    .unwrap()
                    .with_timezone(&Utc),
            )
        };
        assert_eq!(stats[&finished].turn_count, 1);
        assert_eq!(stats[&finished].last_message_at, ts(t1));
        assert_eq!(stats[&running].turn_count, 2);
        assert_eq!(stats[&running].last_message_at, ts(t2));

        let holder = db.locks().get(running).unwrap().unwrap().holder_id;
        db.locks().release(running, holder).unwrap();
        let stats = db.entries().conversation_stats_for_ids(&[running]).unwrap();
        assert_eq!(stats[&running].last_message_at, ts(t3));
    }

    #[test]
    fn stats_of_a_session_with_only_a_live_turn_have_no_message_time() {
        let (db, sid) = setup();
        let live = TurnId::new();
        db.locks().acquire(sid, live, Some("desktop")).unwrap();
        entry_at(&db, sid, 1, live, EntryKind::User, "2026-01-01T01:00:00Z");

        let stats = db.entries().conversation_stats_for(sid).unwrap();
        assert_eq!(stats.turn_count, 1);
        assert_eq!(
            stats.last_message_at, None,
            "a live message does not count as a completed turn"
        );
    }
}
