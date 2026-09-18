//! The `usage_event` table.
//! Two rules that differ from every other table here:
//! 1. **No session foreign key.** Usage must outlive the session it came from, or
//!    "how much did this month cost" shrinks whenever someone tidies up their session list.
//! 2. **`purpose` is required.** Compaction's trigger reads only `main`, so sub-agents,
//!    title refinement and approval calls are excluded structurally rather than by
//!    maintaining a list of what counts as the main conversation.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, named_params};
use serde_json::Value;
use zlogic_protocol::stream::ToolStats;
use zlogic_protocol::usage::{CostSource, Purpose, TokenUsage};
use zlogic_protocol::{RoundId, SessionId, TurnId, UsageId};

use crate::{Json, Result, now};

#[derive(Debug, Clone, PartialEq)]
pub struct UsageRecord {
    pub usage_id: UsageId,
    pub session_id: SessionId,
    pub turn_id: Option<TurnId>,
    pub round_id: Option<RoundId>,
    pub purpose: Purpose,
    pub model_ref: Option<String>,
    pub tokens: TokenUsage,
    pub cost: Option<f64>,
    pub currency: Option<String>,
    pub cost_source: Option<CostSource>,
    pub created_at: DateTime<Utc>,
    pub request_started_at: Option<DateTime<Utc>>,
    pub first_token_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UsageRow {
    pub record: UsageRecord,
    pub workspace_id: Option<zlogic_protocol::WorkspaceId>,
}

#[derive(Debug, Clone, Default)]
pub struct UsageQuery {
    pub workspace_id: Option<zlogic_protocol::WorkspaceId>,
    pub session_id: Option<SessionId>,
    pub self_only: bool,
    pub session_kind: Option<crate::SessionKind>,
    pub turn_id: Option<TurnId>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub utc_offset_minutes: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UsageAggregatePart {
    pub key: String,
    pub label: Option<String>,
    pub calls: u32,
    pub aux_calls: u32,
    pub estimated_calls: u32,
    pub sessions: u32,
    pub turns: u32,
    pub tokens: TokenUsage,
    pub max_input_tokens: u64,
    pub cost: Option<f64>,
    pub currency: String,
    pub cost_source: Option<CostSource>,
    pub ttft_sum_ms: u64,
    pub ttft_n: u32,
    pub response_sum_ms: u64,
    pub response_n: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TurnEnvelope {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageAggregate {
    pub total: Vec<UsageAggregatePart>,
    pub current_context_tokens: Option<u64>,
    pub by_model: Vec<UsageAggregatePart>,
    pub by_provider: Vec<UsageAggregatePart>,
    pub by_day: Vec<UsageAggregatePart>,
    pub by_session: Vec<UsageAggregatePart>,
    pub by_workspace: Vec<UsageAggregatePart>,
    pub by_aux_purpose: Vec<UsageAggregatePart>,
    pub by_cost_source: Vec<UsageAggregatePart>,
    pub latest_turn: Vec<UsageAggregatePart>,
    pub turn_envelopes: Vec<TurnEnvelope>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolUsageAggregate {
    pub name: String,
    pub stats: ToolStats,
    pub duration_ms: u64,
}

#[derive(Debug, Clone)]
pub struct NewUsage {
    pub session_id: SessionId,
    pub turn_id: Option<TurnId>,
    pub round_id: Option<RoundId>,
    pub purpose: Purpose,
    pub model_ref: Option<String>,
    pub tokens: TokenUsage,
    pub cost: Option<f64>,
    pub currency: Option<String>,
    pub cost_source: Option<CostSource>,
    /// Display-only and diagnostic material (the raw provider usage blob).
    /// **No columns for these** — they are never queried, only shown.
    pub metadata: Option<Value>,
    /// Round-level timing (RFC3339 UTC, schema V14). `None` both on legacy rows and on the
    /// aux paths, which never take part in the main TTFT / response aggregates.
    pub request_started_at: Option<DateTime<Utc>>,
    pub first_token_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
}

impl NewUsage {
    pub fn new(session_id: SessionId, purpose: Purpose, tokens: TokenUsage) -> Self {
        Self {
            session_id,
            turn_id: None,
            round_id: None,
            purpose,
            model_ref: None,
            tokens,
            cost: None,
            currency: None,
            cost_source: None,
            metadata: None,
            request_started_at: None,
            first_token_at: None,
            completed_at: None,
        }
    }

    pub fn in_round(mut self, turn_id: TurnId, round_id: RoundId) -> Self {
        self.turn_id = Some(turn_id);
        self.round_id = Some(round_id);
        self
    }

    /// A call that belongs to **no turn**: a session title, a commit message, a task draft.
    /// They still need a `round_id` — that is what [`UsageStore::upsert_round`] deduplicates a
    /// provider's repeated cumulative reports by — but inventing a `turn_id` for them puts a
    /// reference to a turn that never existed in the table, and two reports read it:
    /// `COUNT(DISTINCT turn_id)` counts one extra turn per call, and the "latest turn" panel can
    /// select the phantom and then show a turn whose only content is a title call.
    pub fn in_detached_round(mut self, round_id: RoundId) -> Self {
        self.turn_id = None;
        self.round_id = Some(round_id);
        self
    }

    pub fn with_cost(mut self, amount: f64, currency: &str, source: CostSource) -> Self {
        self.cost = Some(amount);
        self.currency = Some(currency.to_string());
        self.cost_source = Some(source);
        self
    }

    /// Attaches the round's wall-clock timing. `None` everywhere keeps the aux paths working
    /// unchanged — they simply do not call this.
    pub fn with_timing(
        mut self,
        request_started_at: Option<DateTime<Utc>>,
        first_token_at: Option<DateTime<Utc>>,
        completed_at: Option<DateTime<Utc>>,
    ) -> Self {
        self.request_started_at = request_started_at;
        self.first_token_at = first_token_at;
        self.completed_at = completed_at;
        self
    }

    pub fn with_metadata(mut self, m: Value) -> Self {
        self.metadata = Some(m);
        self
    }
}

pub struct UsageStore<'a> {
    conn: &'a Connection,
}

impl<'a> UsageStore<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub fn record(&self, new: NewUsage) -> Result<UsageId> {
        let id = UsageId::new();
        self.conn.execute(
            "INSERT INTO usage_event
                (usage_id, session_id, turn_id, round_id, purpose, model_ref,
                 input_tokens, output_tokens, cache_read, cache_write, reasoning,
                 cost, currency, cost_source, metadata, created_at,
                 request_started_at, first_token_at, completed_at)
             VALUES (:usage_id, :session_id, :turn_id, :round_id, :purpose, :model_ref,
                     :input, :output, :cache_read, :cache_write, :reasoning,
                     :cost, :currency, :cost_source, :metadata, :created_at,
                     :request_started_at, :first_token_at, :completed_at)",
            named_params! {
                ":usage_id": id,
                ":session_id": new.session_id,
                ":turn_id": new.turn_id,
                ":round_id": new.round_id,
                ":purpose": new.purpose,
                ":model_ref": new.model_ref,
                ":input": new.tokens.input as i64,
                ":output": new.tokens.output as i64,
                ":cache_read": new.tokens.cache_read.map(|v| v as i64),
                ":cache_write": new.tokens.cache_write.map(|v| v as i64),
                ":reasoning": new.tokens.reasoning.map(|v| v as i64),
                ":cost": new.cost,
                ":currency": new.currency,
                ":cost_source": new.cost_source,
                ":metadata": new.metadata.map(Json),
                ":created_at": now(),
                ":request_started_at": new.request_started_at,
                ":first_token_at": new.first_token_at,
                ":completed_at": new.completed_at,
            },
        )?;
        Ok(id)
    }

    /// Records a round's usage, replacing any earlier report for the same round.
    /// Providers report **cumulative** figures, not deltas, and they report more than once
    /// per round. Accumulating would multiply the numbers several times over.
    pub fn upsert_round(&self, new: NewUsage) -> Result<()> {
        if let Some(round) = new.round_id {
            self.conn.execute(
                "DELETE FROM usage_event WHERE round_id = :round AND purpose = :purpose",
                named_params! { ":round": round, ":purpose": new.purpose },
            )?;
        }
        self.record(new)?;
        Ok(())
    }

    /// **The only input to compaction's trigger**: this session's most recent main-conversation
    /// `input_tokens`.
    /// `None` means "not known yet", which is different from `0` ("the context is empty").
    pub fn last_main_input_tokens(&self, session_id: SessionId) -> Result<Option<u64>> {
        let v: Option<i64> = self
            .conn
            .query_row(
                "SELECT input_tokens FROM usage_event
                 WHERE session_id = :session_id AND purpose = 'main'
                 ORDER BY created_at DESC, id DESC LIMIT 1",
                named_params! { ":session_id": session_id },
                |r| r.get(0),
            )
            .optional()?;
        Ok(v.map(|v| v as u64))
    }

    /// The most recent context-bearing round for this concrete session.
    /// Root conversations write `main`; child conversations write `agent:<profile>`. Keeping this
    /// session-scoped prevents a child's large context from compacting its parent while still
    /// allowing the child to use the same proactive threshold.
    pub fn last_conversation_input_tokens(&self, session_id: SessionId) -> Result<Option<u64>> {
        let v: Option<i64> = self
            .conn
            .query_row(
                "SELECT input_tokens FROM usage_event
                 WHERE session_id = :session_id
                   AND (purpose = 'main' OR purpose LIKE 'agent:%')
                 ORDER BY created_at DESC, id DESC LIMIT 1",
                named_params! { ":session_id": session_id },
                |r| r.get(0),
            )
            .optional()?;
        Ok(v.map(|v| v as u64))
    }

    pub fn total_for_session(&self, session_id: SessionId) -> Result<TokenUsage> {
        self.sum(
            "session_id = :session_id",
            named_params! { ":session_id": session_id },
        )
    }

    pub fn total_for_turn(&self, turn_id: TurnId) -> Result<TokenUsage> {
        self.sum("turn_id = :turn_id", named_params! { ":turn_id": turn_id })
    }

    /// The whole sub-agent tree (root plus every child session).
    pub fn total_for_tree(&self, root: SessionId) -> Result<TokenUsage> {
        self.sum(
            "session_id IN (SELECT session_id FROM session WHERE root_session_id = :root)",
            named_params! { ":root": root },
        )
    }

    pub fn cost_by_currency_for_tree(&self, root: SessionId) -> Result<Vec<(String, f64)>> {
        self.cost_by_currency(
            "session_id IN (SELECT session_id FROM session WHERE root_session_id = :a)",
            named_params! { ":a": root },
        )
    }

    pub fn cost_by_currency_since(&self, since: DateTime<Utc>) -> Result<Vec<(String, f64)>> {
        self.cost_by_currency(
            "created_at >= :a",
            named_params! { ":a": since.to_rfc3339() },
        )
    }

    fn cost_by_currency(
        &self,
        filter: &str,
        params: &[(&str, &dyn rusqlite::ToSql)],
    ) -> Result<Vec<(String, f64)>> {
        let sql = format!(
            "SELECT COALESCE(NULLIF(TRIM(currency), ''), 'USD') AS c, SUM(cost)
             FROM usage_event WHERE cost IS NOT NULL AND {filter} GROUP BY c"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params, |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn cost_for_session(&self, session_id: SessionId) -> Result<f64> {
        let v: Option<f64> = self.conn.query_row(
            "SELECT SUM(cost) FROM usage_event WHERE session_id = :session_id",
            named_params! { ":session_id": session_id },
            |r| r.get(0),
        )?;
        Ok(v.unwrap_or(0.0))
    }

    pub fn aggregate(&self, q: &UsageQuery) -> Result<UsageAggregate> {
        let current_context_tokens = q
            .session_id
            .map(|session_id| self.last_main_input_tokens(session_id))
            .transpose()?
            .flatten();

        // One scan feeds every dimension below. Asking each dimension for its own grouped query
        // needed a full pass over `usage_event` for the measures and a second one for the distinct
        // session/turn counts, so a report cost roughly sixteen passes before anything was merged —
        // seconds of connection time per summary on a database with a few tens of thousands of rows.
        let rows = self.composite_rows(q)?;

        // Read off the rows already in hand rather than scanning again: the latest turn is one
        // (session, turn) of them, and every measure folds from a group into that turn unchanged.
        let latest_turn = match self.latest_turn_id(q)? {
            Some(turn_id) => self.dimension(
                rows.iter().filter(|row| row.turn_id == Some(turn_id)),
                TURN_DIMENSION,
            ),
            None => Vec::new(),
        };

        Ok(UsageAggregate {
            total: self.dimension(rows.iter(), TOTAL_DIMENSION),
            current_context_tokens,
            by_model: self.dimension(rows.iter(), MODEL_DIMENSION),
            by_provider: self.dimension(rows.iter(), PROVIDER_DIMENSION),
            by_day: self.dimension(rows.iter(), DAY_DIMENSION),
            by_session: self.dimension(rows.iter(), SESSION_DIMENSION),
            by_workspace: self.dimension(rows.iter(), WORKSPACE_DIMENSION),
            by_aux_purpose: self.dimension(rows.iter(), AUX_PURPOSE_DIMENSION),
            by_cost_source: self.dimension(rows.iter(), COST_SOURCE_DIMENSION),
            latest_turn,
            turn_envelopes: turn_envelopes_of(&rows),
        })
    }

    /// One row per `(session, turn, purpose, model, cost source, currency, day, workspace)` group of
    /// the events this query selects.
    /// Every report dimension is a superset-grouping of this key, so a single scan answers all of
    /// them; the folding in [`UsageStore::dimension`] is where a dimension such as "total" or
    /// "by model" gets its numbers. The identity columns are part of the key on purpose: that is
    /// what makes any dimension's distinct session/turn count derivable here, exactly, without the
    /// second query per dimension that the previous shape needed for it.
    ///
    /// The group count collapses hard — a round shares its turn, model, purpose, currency and day
    /// with its neighbours — so this stays a small vector even on a large `usage_event`.
    fn composite_rows(&self, q: &UsageQuery) -> Result<Vec<CompositeRow>> {
        let sql = format!(
            "SELECT u.session_id AS k_session,
                    u.turn_id AS k_turn,
                    u.purpose AS k_purpose,
                    u.model_ref AS k_model,
                    u.cost_source AS k_cost_source,
                    {CURRENCY_EXPR} AS k_currency,
                    strftime('%Y-%m-%d', datetime(u.created_at, :timezone)) AS k_day,
                    s.workspace_id AS k_workspace,
                    NULLIF(TRIM(s.title), '') AS k_session_title,
                    w.name AS k_workspace_name,
                    {COMPOSITE_SELECT}
             FROM usage_event u
             LEFT JOIN session s ON s.session_id = u.session_id
             LEFT JOIN workspaces w ON w.workspace_id = s.workspace_id
             WHERE {AGGREGATE_FILTER}
               AND :timezone IS NOT NULL
             GROUP BY k_session, k_turn, k_purpose, k_model, k_cost_source, k_currency, k_day, k_workspace"
        );
        let timezone = format!("{:+} minutes", q.utc_offset_minutes);
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map(
            named_params! {
                ":since": q.since,
                ":until": q.until,
                ":workspace": q.workspace_id,
                ":session": q.session_id,
                ":self_only": q.self_only,
                ":turn": q.turn_id,
                ":timezone": timezone,
                ":session_kind": q.session_kind,
            },
            composite_row,
        )?;
        rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
    }

    /// Folds composite rows into one dimension: a part per `(key, currency, cost source)` cost
    /// fragment, ordered by key.
    ///
    /// The distinct session and turn counts belong to the key rather than to a fragment — a session
    /// that paid in two currencies is still one session — so exactly one part per key carries them
    /// and its siblings stay at zero. Summing the parts therefore restores the key's distinct count,
    /// which is what the caller's merge relies on.
    ///
    /// The rows come in as an iterator so a dimension that only covers part of the report (the
    /// latest turn) can select them without the scan being repeated.
    fn dimension<'r>(
        &self,
        rows: impl Iterator<Item = &'r CompositeRow>,
        dim: Dimension,
    ) -> Vec<UsageAggregatePart> {
        let mut fragments: HashMap<(String, String, String), FragmentAcc> = HashMap::new();
        let mut distinct: HashMap<String, (HashSet<SessionId>, HashSet<TurnId>)> = HashMap::new();

        for row in rows {
            let Some(key) = (dim.key)(row) else {
                continue;
            };
            fragments
                .entry((
                    key.clone(),
                    row.currency.clone(),
                    cost_source_rank(row.cost_source),
                ))
                .or_insert_with(|| FragmentAcc::new((dim.label)(row), row))
                .add(row);
            let seen = distinct.entry(key).or_default();
            seen.0.insert(row.session_id);
            if let Some(turn_id) = row.turn_id {
                seen.1.insert(turn_id);
            }
        }

        let mut fragments: Vec<_> = fragments.into_iter().collect();
        fragments.sort_by(|a, b| a.0.cmp(&b.0));

        let mut parts = Vec::with_capacity(fragments.len());
        let mut previous: Option<String> = None;
        for ((key, ..), fragment) in fragments {
            let first_of_key = previous.as_deref() != Some(key.as_str());
            let (sessions, turns) = match (first_of_key, distinct.get(&key)) {
                (true, Some((sessions, turns))) => (sessions.len() as u32, turns.len() as u32),
                _ => (0, 0),
            };
            previous = Some(key.clone());
            parts.push(fragment.into_part(key, sessions, turns));
        }
        parts
    }

    fn latest_turn_id(&self, q: &UsageQuery) -> Result<Option<TurnId>> {
        self.conn
            .query_row(
                &format!(
                    "SELECT u.turn_id
                     FROM usage_event u
                     LEFT JOIN session s ON s.session_id = u.session_id
                     WHERE {AGGREGATE_FILTER}
                       AND (:session IS NULL OR u.session_id = :session)
                       AND u.turn_id IS NOT NULL
                     ORDER BY u.created_at DESC, u.id DESC
                     LIMIT 1"
                ),
                named_params! {
                    ":since": q.since,
                    ":until": q.until,
                    ":workspace": q.workspace_id,
                    ":session": q.session_id,
                    ":self_only": q.self_only,
                    ":turn": q.turn_id,
                    ":session_kind": q.session_kind,
                },
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn aggregate_tools(&self, q: &UsageQuery) -> Result<Vec<ToolUsageAggregate>> {
        let sql = "SELECT json_extract(e.data, '$.name') AS name,
                    COUNT(*) AS total,
                    SUM(CASE WHEN json_extract(e.display, '$.status') = 'completed' THEN 1 ELSE 0 END) AS succeeded,
                    SUM(CASE WHEN json_extract(e.display, '$.status') IN ('error', 'precheck_failed') THEN 1 ELSE 0 END) AS failed,
                    SUM(CASE WHEN json_extract(e.display, '$.status') = 'denied' THEN 1 ELSE 0 END) AS denied,
                    SUM(CASE WHEN json_extract(e.display, '$.status') = 'timeout' THEN 1 ELSE 0 END) AS timed_out,
                    SUM(CASE WHEN json_extract(e.display, '$.status') = 'cancelled' THEN 1 ELSE 0 END) AS cancelled,
                    SUM(CASE WHEN json_extract(e.display, '$.status') = 'precheck_failed' THEN 1 ELSE 0 END) AS precheck_failed,
                    COALESCE(SUM(CAST(json_extract(e.display, '$.duration_ms') AS INTEGER)), 0) AS duration_ms
             FROM session_entry e
             JOIN session s ON s.session_id = e.session_id
             WHERE e.kind = 'tool_result'
               AND e.display IS NOT NULL
               AND json_extract(e.data, '$.name') IS NOT NULL
               AND (:since IS NULL OR e.created_at >= :since)
               AND (:until IS NULL OR e.created_at <= :until)
               AND (:workspace IS NULL OR s.workspace_id = :workspace)
               AND (:session IS NULL
                    OR (:self_only = 1 AND e.session_id = :session)
                    OR (:self_only = 0 AND s.root_session_id = :session))
               AND (:turn IS NULL OR e.turn_id = :turn)
               AND (:session_kind IS NULL OR s.kind = :session_kind)
             GROUP BY name
             ORDER BY duration_ms DESC, name";
        let mut statement = self.conn.prepare(sql)?;
        let rows = statement.query_map(
            named_params! {
                ":since": q.since,
                ":until": q.until,
                ":workspace": q.workspace_id,
                ":session": q.session_id,
                ":self_only": q.self_only,
                ":turn": q.turn_id,
                ":session_kind": q.session_kind,
            },
            |row| {
                Ok(ToolUsageAggregate {
                    name: row.get("name")?,
                    stats: ToolStats {
                        total: row.get::<_, i64>("total")? as u32,
                        succeeded: row.get::<_, i64>("succeeded")? as u32,
                        failed: row.get::<_, i64>("failed")? as u32,
                        denied: row.get::<_, i64>("denied")? as u32,
                        timed_out: row.get::<_, i64>("timed_out")? as u32,
                        cancelled: row.get::<_, i64>("cancelled")? as u32,
                        precheck_failed: row.get::<_, i64>("precheck_failed")? as u32,
                    },
                    duration_ms: row.get::<_, i64>("duration_ms")? as u64,
                })
            },
        )?;
        rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
    }

    pub fn list_for_session(&self, session_id: SessionId) -> Result<Vec<UsageRecord>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {COLS} FROM usage_event
             WHERE session_id = :session_id ORDER BY created_at, id"
        ))?;
        Ok(st
            .query_map(named_params! { ":session_id": session_id }, map_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    fn sum(&self, where_clause: &str, p: impl rusqlite::Params) -> Result<TokenUsage> {
        let sql = format!(
            "SELECT COALESCE(SUM(input_tokens),0), COALESCE(SUM(output_tokens),0),
                    SUM(cache_read), SUM(cache_write), SUM(reasoning)
             FROM usage_event WHERE {where_clause}"
        );
        Ok(self.conn.query_row(&sql, p, |r| {
            Ok(TokenUsage {
                input: r.get::<_, i64>(0)? as u64,
                output: r.get::<_, i64>(1)? as u64,
                cache_read: r.get::<_, Option<i64>>(2)?.map(|v| v as u64),
                cache_write: r.get::<_, Option<i64>>(3)?.map(|v| v as u64),
                reasoning: r.get::<_, Option<i64>>(4)?.map(|v| v as u64),
            })
        })?)
    }
}

const AGGREGATE_FILTER: &str = "
    (:since IS NULL OR u.created_at >= :since)
    AND (:until IS NULL OR u.created_at <= :until)
    AND (:workspace IS NULL OR s.workspace_id = :workspace)
    AND (:session IS NULL
         OR (:self_only = 1 AND u.session_id = :session)
         OR (:self_only = 0 AND s.root_session_id = :session))
    AND (:turn IS NULL OR u.turn_id = :turn)
    AND (:session_kind IS NULL OR s.kind = :session_kind)
";

/// The currency a cost fragment is reported in: one of the two columns a fragment is keyed by.
const CURRENCY_EXPR: &str = "UPPER(COALESCE(NULLIF(trim(u.currency), ''), 'USD'))";

/// The measures that add up from one composite group into a coarser grouping.
/// `sessions` and `turns` are deliberately absent: they are distinct counts, and a session that
/// appears in two cost fragments must still be counted once, so they come from the session/turn
/// columns of the composite key instead — see [`UsageStore::dimension`].
const COMPOSITE_SELECT: &str = "
    COUNT(*) AS calls,
    SUM(CASE WHEN u.purpose <> 'main' AND u.purpose NOT LIKE 'agent:%' THEN 1 ELSE 0 END) AS aux_calls,
    SUM(CASE WHEN u.cost_source = 'estimated' THEN 1 ELSE 0 END) AS estimated_calls,
    COALESCE(SUM(u.input_tokens), 0) AS input_tokens,
    COALESCE(MAX(u.input_tokens), 0) AS max_input_tokens,
    COALESCE(SUM(u.output_tokens), 0) AS output_tokens,
    SUM(u.cache_read) AS cache_read,
    SUM(u.cache_write) AS cache_write,
    SUM(u.reasoning) AS reasoning,
    SUM(u.cost) AS cost,
    -- Timing aggregates, counting **only main rounds** (auxiliary calls and estimated records do not
    -- take part; see engine/usage.rs for the definition). SUM / COUNT stay exact when the groups are
    -- folded into a dimension, so unlike COUNT(DISTINCT) they carry no shard caveat.
    COALESCE(SUM(CASE
        WHEN u.purpose = 'main'
         AND u.request_started_at IS NOT NULL AND u.first_token_at IS NOT NULL
        THEN CAST(ROUND((julianday(u.first_token_at) - julianday(u.request_started_at)) * 86400000.0) AS INTEGER)
        ELSE 0 END), 0) AS ttft_sum_ms,
    SUM(CASE
        WHEN u.purpose = 'main'
         AND u.request_started_at IS NOT NULL AND u.first_token_at IS NOT NULL
        THEN 1 ELSE 0 END) AS ttft_n,
    COALESCE(SUM(CASE
        WHEN u.purpose = 'main'
         AND u.request_started_at IS NOT NULL AND u.completed_at IS NOT NULL
        THEN CAST(ROUND((julianday(u.completed_at) - julianday(u.request_started_at)) * 86400000.0) AS INTEGER)
        ELSE 0 END), 0) AS response_sum_ms,
    SUM(CASE
        WHEN u.purpose = 'main'
         AND u.request_started_at IS NOT NULL AND u.completed_at IS NOT NULL
        THEN 1 ELSE 0 END) AS response_n,
    -- Turn envelopes come out of the same pass. Both ends are required per round, exactly as the
    -- per-(session, turn) query this replaced required them.
    MAX(CASE
        WHEN u.request_started_at IS NOT NULL AND u.completed_at IS NOT NULL
        THEN julianday(u.completed_at) END) AS envelope_end_jd,
    MIN(CASE
        WHEN u.request_started_at IS NOT NULL AND u.completed_at IS NOT NULL
        THEN julianday(u.request_started_at) END) AS envelope_start_jd
";

/// A report dimension: which composite rows take part, the key they group under, and the label that
/// key is displayed with. `None` from `key` excludes the row, which is how the dimensions that only
/// report rows carrying a model or a workspace are expressed.
struct Dimension {
    key: fn(&CompositeRow) -> Option<String>,
    label: fn(&CompositeRow) -> Option<String>,
}

/// One `(session, turn, purpose, model, cost source, currency, day, workspace)` group of
/// `usage_event`, carrying every measure that can be folded into a coarser grouping.
struct CompositeRow {
    session_id: SessionId,
    turn_id: Option<TurnId>,
    purpose: String,
    model_ref: Option<String>,
    cost_source: Option<CostSource>,
    currency: String,
    day: String,
    workspace_id: Option<zlogic_protocol::WorkspaceId>,
    session_title: Option<String>,
    workspace_name: Option<String>,
    calls: u32,
    aux_calls: u32,
    estimated_calls: u32,
    input_tokens: u64,
    output_tokens: u64,
    max_input_tokens: u64,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
    reasoning: Option<u64>,
    cost: Option<f64>,
    ttft_sum_ms: u64,
    ttft_n: u32,
    response_sum_ms: u64,
    response_n: u32,
    envelope_end_jd: Option<f64>,
    envelope_start_jd: Option<f64>,
}

/// One `(key, currency, cost source)` cost fragment while it is being filled.
#[derive(Default)]
struct FragmentAcc {
    label: Option<String>,
    currency: String,
    cost_source: Option<CostSource>,
    calls: u32,
    aux_calls: u32,
    estimated_calls: u32,
    tokens: TokenUsage,
    max_input_tokens: u64,
    cost: Option<f64>,
    ttft_sum_ms: u64,
    ttft_n: u32,
    response_sum_ms: u64,
    response_n: u32,
}

impl FragmentAcc {
    fn new(label: Option<String>, row: &CompositeRow) -> Self {
        Self {
            label,
            currency: row.currency.clone(),
            cost_source: row.cost_source,
            ..Self::default()
        }
    }

    fn add(&mut self, row: &CompositeRow) {
        self.calls += row.calls;
        self.aux_calls += row.aux_calls;
        self.estimated_calls += row.estimated_calls;
        self.tokens.input += row.input_tokens;
        self.tokens.output += row.output_tokens;
        self.tokens.cache_read = add_optional(self.tokens.cache_read, row.cache_read);
        self.tokens.cache_write = add_optional(self.tokens.cache_write, row.cache_write);
        self.tokens.reasoning = add_optional(self.tokens.reasoning, row.reasoning);
        self.max_input_tokens = self.max_input_tokens.max(row.max_input_tokens);
        self.cost = add_optional_f64(self.cost, row.cost);
        self.ttft_sum_ms += row.ttft_sum_ms;
        self.ttft_n += row.ttft_n;
        self.response_sum_ms += row.response_sum_ms;
        self.response_n += row.response_n;
    }

    fn into_part(self, key: String, sessions: u32, turns: u32) -> UsageAggregatePart {
        UsageAggregatePart {
            key,
            label: self.label,
            calls: self.calls,
            aux_calls: self.aux_calls,
            estimated_calls: self.estimated_calls,
            sessions,
            turns,
            tokens: self.tokens,
            max_input_tokens: self.max_input_tokens,
            cost: self.cost,
            currency: self.currency,
            cost_source: self.cost_source,
            ttft_sum_ms: self.ttft_sum_ms,
            ttft_n: self.ttft_n,
            response_sum_ms: self.response_sum_ms,
            response_n: self.response_n,
        }
    }
}

/// `SUM(...)` semantics for a nullable column: a fragment in which nothing was reported stays
/// `None`, because downstream "not reported" and "zero" are different answers.
fn add_optional(acc: Option<u64>, add: Option<u64>) -> Option<u64> {
    match (acc, add) {
        (None, None) => None,
        (acc, add) => Some(acc.unwrap_or(0) + add.unwrap_or(0)),
    }
}

fn add_optional_f64(acc: Option<f64>, add: Option<f64>) -> Option<f64> {
    match (acc, add) {
        (None, None) => None,
        (acc, add) => Some(acc.unwrap_or(0.0) + add.unwrap_or(0.0)),
    }
}

/// Orders the cost fragments of one key the way the SQL used to, by the wire name of the source.
/// The order only has to be stable: the caller sums the fragments of a key either way.
fn cost_source_rank(source: Option<CostSource>) -> String {
    source
        .map(|source| source.as_str().to_owned())
        .unwrap_or_default()
}

/// Mirrors the SQL test `purpose <> 'main' AND purpose NOT LIKE 'agent:%'` that decides whether a
/// stored purpose is an auxiliary call, applied to the same value read back as text.
fn is_aux_purpose(purpose: &str) -> bool {
    purpose != "main" && !purpose.starts_with("agent:")
}

fn composite_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CompositeRow> {
    Ok(CompositeRow {
        session_id: row.get("k_session")?,
        turn_id: row.get("k_turn")?,
        purpose: row.get("k_purpose")?,
        model_ref: row.get("k_model")?,
        cost_source: row.get("k_cost_source")?,
        currency: row.get("k_currency")?,
        day: row.get("k_day")?,
        workspace_id: row.get("k_workspace")?,
        session_title: row.get("k_session_title")?,
        workspace_name: row.get("k_workspace_name")?,
        calls: row.get::<_, i64>("calls")? as u32,
        aux_calls: row.get::<_, i64>("aux_calls")? as u32,
        estimated_calls: row.get::<_, i64>("estimated_calls")? as u32,
        input_tokens: row.get::<_, i64>("input_tokens")? as u64,
        output_tokens: row.get::<_, i64>("output_tokens")? as u64,
        max_input_tokens: row.get::<_, i64>("max_input_tokens")? as u64,
        cache_read: row
            .get::<_, Option<i64>>("cache_read")?
            .map(|value| value as u64),
        cache_write: row
            .get::<_, Option<i64>>("cache_write")?
            .map(|value| value as u64),
        reasoning: row
            .get::<_, Option<i64>>("reasoning")?
            .map(|value| value as u64),
        cost: row.get("cost")?,
        ttft_sum_ms: row.get::<_, i64>("ttft_sum_ms")? as u64,
        ttft_n: row.get::<_, i64>("ttft_n")? as u32,
        response_sum_ms: row.get::<_, i64>("response_sum_ms")? as u64,
        response_n: row.get::<_, i64>("response_n")? as u32,
        envelope_end_jd: row.get("envelope_end_jd")?,
        envelope_start_jd: row.get("envelope_start_jd")?,
    })
}

/// The span of every `(session, turn)` that recorded both ends: from its earliest request to its
/// latest completion. A composite group carries its own extremes, and the extremes of those are the
/// same answer as the extremes over the whole turn.
fn turn_envelopes_of(rows: &[CompositeRow]) -> Vec<TurnEnvelope> {
    let mut spans: HashMap<(SessionId, TurnId), (f64, f64)> = HashMap::new();
    for row in rows {
        let (Some(turn_id), Some(end), Some(start)) =
            (row.turn_id, row.envelope_end_jd, row.envelope_start_jd)
        else {
            continue;
        };
        let span = spans
            .entry((row.session_id, turn_id))
            .or_insert((end, start));
        span.0 = span.0.max(end);
        span.1 = span.1.min(start);
    }
    spans
        .into_iter()
        .map(|((session_id, turn_id), (end, start))| TurnEnvelope {
            session_id,
            turn_id,
            duration_ms: ((end - start) * 86_400_000.0).round() as i64 as u64,
        })
        .collect()
}

fn key_total(_: &CompositeRow) -> Option<String> {
    Some(String::new())
}

fn label_none(_: &CompositeRow) -> Option<String> {
    None
}

fn key_turn(row: &CompositeRow) -> Option<String> {
    row.turn_id.map(|turn_id| turn_id.to_string())
}

fn key_model(row: &CompositeRow) -> Option<String> {
    row.model_ref.clone()
}

fn key_provider(row: &CompositeRow) -> Option<String> {
    row.model_ref.as_deref().map(|model_ref| {
        model_ref
            .split_once(':')
            .map_or(model_ref, |(provider, _)| provider)
            .to_owned()
    })
}

fn key_day(row: &CompositeRow) -> Option<String> {
    Some(row.day.clone())
}

fn key_session(row: &CompositeRow) -> Option<String> {
    Some(row.session_id.to_string())
}

fn label_session(row: &CompositeRow) -> Option<String> {
    row.session_title.clone()
}

fn key_workspace(row: &CompositeRow) -> Option<String> {
    row.workspace_id
        .map(|workspace_id| workspace_id.to_string())
}

fn label_workspace(row: &CompositeRow) -> Option<String> {
    row.workspace_name.clone().or_else(|| {
        row.workspace_id
            .map(|workspace_id| workspace_id.to_string())
    })
}

fn key_aux_purpose(row: &CompositeRow) -> Option<String> {
    is_aux_purpose(&row.purpose).then(|| row.purpose.clone())
}

fn key_cost_source(row: &CompositeRow) -> Option<String> {
    Some(
        row.cost_source
            .map(|source| source.as_str())
            .unwrap_or("unknown")
            .to_owned(),
    )
}

const TOTAL_DIMENSION: Dimension = Dimension {
    key: key_total,
    label: label_none,
};
const TURN_DIMENSION: Dimension = Dimension {
    key: key_turn,
    label: label_none,
};
const MODEL_DIMENSION: Dimension = Dimension {
    key: key_model,
    label: label_none,
};
const PROVIDER_DIMENSION: Dimension = Dimension {
    key: key_provider,
    label: label_none,
};
const DAY_DIMENSION: Dimension = Dimension {
    key: key_day,
    label: label_none,
};
const SESSION_DIMENSION: Dimension = Dimension {
    key: key_session,
    label: label_session,
};
const WORKSPACE_DIMENSION: Dimension = Dimension {
    key: key_workspace,
    label: label_workspace,
};
const AUX_PURPOSE_DIMENSION: Dimension = Dimension {
    key: key_aux_purpose,
    label: label_none,
};
const COST_SOURCE_DIMENSION: Dimension = Dimension {
    key: key_cost_source,
    label: label_none,
};

const COLS: &str = "usage_id, session_id, turn_id, round_id, purpose, model_ref,
     input_tokens, output_tokens, cache_read, cache_write, reasoning,
     cost, currency, cost_source, created_at,
     request_started_at, first_token_at, completed_at";

fn map_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<UsageRecord> {
    Ok(UsageRecord {
        usage_id: r.get("usage_id")?,
        session_id: r.get("session_id")?,
        turn_id: r.get("turn_id")?,
        round_id: r.get("round_id")?,
        purpose: r.get("purpose")?,
        model_ref: r.get("model_ref")?,
        tokens: TokenUsage {
            input: r.get::<_, i64>("input_tokens")? as u64,
            output: r.get::<_, i64>("output_tokens")? as u64,
            cache_read: r.get::<_, Option<i64>>("cache_read")?.map(|v| v as u64),
            cache_write: r.get::<_, Option<i64>>("cache_write")?.map(|v| v as u64),
            reasoning: r.get::<_, Option<i64>>("reasoning")?.map(|v| v as u64),
        },
        cost: r.get("cost")?,
        currency: r.get("currency")?,
        cost_source: r.get("cost_source")?,
        created_at: r.get("created_at")?,
        request_started_at: r.get("request_started_at")?,
        first_token_at: r.get("first_token_at")?,
        completed_at: r.get("completed_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Db, NewSession, TitleSource};
    use zlogic_protocol::WorkspaceId;

    fn setup() -> (Db, SessionId) {
        let db = Db::open_in_memory().unwrap();
        let s = db
            .sessions()
            .create(NewSession::root(WorkspaceId::new()))
            .unwrap();
        (db, s.session_id)
    }

    fn usage(sid: SessionId, purpose: Purpose, input: u64) -> NewUsage {
        NewUsage::new(
            sid,
            purpose,
            TokenUsage {
                input,
                output: 10,
                cache_read: Some(5),
                ..Default::default()
            },
        )
    }

    fn timed(sid: SessionId, turn: TurnId, start: &str, first: &str, completed: &str) -> NewUsage {
        usage(sid, Purpose::Main, 100)
            .in_round(turn, RoundId::new())
            .with_timing(
                Some(start.parse().unwrap()),
                Some(first.parse().unwrap()),
                Some(completed.parse().unwrap()),
            )
    }

    #[test]
    fn timing_round_trips_through_the_store() {
        let (db, sid) = setup();
        let turn = TurnId::new();
        db.usage()
            .record(timed(
                sid,
                turn,
                "2026-07-30T08:00:00Z",
                "2026-07-30T08:00:00.500Z",
                "2026-07-30T08:00:02Z",
            ))
            .unwrap();
        let r = &db.usage().list_for_session(sid).unwrap()[0];
        assert_eq!(
            r.request_started_at.unwrap().to_rfc3339(),
            "2026-07-30T08:00:00+00:00"
        );
        assert_eq!(
            (r.first_token_at.unwrap() - r.request_started_at.unwrap()).num_milliseconds(),
            500
        );
        assert_eq!(
            (r.completed_at.unwrap() - r.request_started_at.unwrap()).num_milliseconds(),
            2_000
        );
    }

    #[test]
    fn round_timing_feeds_the_aggregates() {
        let (db, sid) = setup();
        let u = db.usage();
        let turn1 = TurnId::new();
        let turn2 = TurnId::new();
        u.record(timed(
            sid,
            turn1,
            "2026-07-30T08:00:00Z",
            "2026-07-30T08:00:00.500Z",
            "2026-07-30T08:00:02Z",
        ))
        .unwrap();
        u.record(timed(
            sid,
            turn1,
            "2026-07-30T08:00:03Z",
            "2026-07-30T08:00:03.250Z",
            "2026-07-30T08:00:08Z",
        ))
        .unwrap();
        u.record(timed(
            sid,
            turn2,
            "2026-07-30T09:00:00Z",
            "2026-07-30T09:00:00.100Z",
            "2026-07-30T09:00:01Z",
        ))
        .unwrap();

        u.record(usage(sid, Purpose::Title, 10).with_timing(
            Some("2026-07-30T10:00:00Z".parse().unwrap()),
            Some("2026-07-30T10:00:00.100Z".parse().unwrap()),
            Some("2026-07-30T10:00:01Z".parse().unwrap()),
        ))
        .unwrap();
        u.record(usage(sid, Purpose::Main, 200)).unwrap();

        let aggregate = db.usage().aggregate(&UsageQuery::default()).unwrap();
        let total = &aggregate.total[0];
        assert_eq!(total.ttft_n, 3);
        assert_eq!(
            total.ttft_sum_ms,
            500 + 250 + 100,
            "aux does not take part, and neither does a missing column"
        );
        assert_eq!(total.response_n, 3);
        assert_eq!(total.response_sum_ms, 2_000 + 5_000 + 1_000);

        let envelope = |turn: TurnId| {
            aggregate
                .turn_envelopes
                .iter()
                .find(|e| e.turn_id == turn)
                .unwrap()
                .duration_ms
        };
        assert_eq!(aggregate.turn_envelopes.len(), 2);
        assert_eq!(envelope(turn1), 8_000, "min(start) → max(completed)");
        assert_eq!(envelope(turn2), 1_000);

        let by_session = &aggregate.by_session[0];
        assert_eq!(by_session.ttft_sum_ms, total.ttft_sum_ms);
        assert_eq!(by_session.response_sum_ms, total.response_sum_ms);
    }

    #[test]
    fn records_and_aggregates() {
        let (db, sid) = setup();
        let u = db.usage();
        u.record(usage(sid, Purpose::Main, 100)).unwrap();
        u.record(usage(sid, Purpose::Main, 200)).unwrap();

        let t = u.total_for_session(sid).unwrap();
        assert_eq!(t.input, 300);
        assert_eq!(t.output, 20);
        assert_eq!(t.cache_read, Some(10));
    }

    #[test]
    fn aggregate_separates_chat_and_task_sessions() {
        let db = Db::open_in_memory().unwrap();
        let workspace_id = WorkspaceId::new();
        let chat = db
            .sessions()
            .create(NewSession::root(workspace_id))
            .unwrap();
        let task = db
            .sessions()
            .create(NewSession::task(workspace_id))
            .unwrap();
        db.usage()
            .record(usage(chat.session_id, Purpose::Main, 100))
            .unwrap();
        db.usage()
            .record(usage(task.session_id, Purpose::Main, 900))
            .unwrap();

        let input_for = |kind| {
            db.usage()
                .aggregate(&UsageQuery {
                    workspace_id: Some(workspace_id),
                    session_kind: Some(kind),
                    ..Default::default()
                })
                .unwrap()
                .total
                .iter()
                .map(|part| part.tokens.input)
                .sum::<u64>()
        };
        assert_eq!(input_for(crate::SessionKind::Chat), 100);
        assert_eq!(input_for(crate::SessionKind::Task), 900);
    }

    #[test]
    fn session_aggregate_uses_title_without_falling_back_to_id() {
        let (db, sid) = setup();
        db.usage().record(usage(sid, Purpose::Main, 100)).unwrap();

        let untitled = db.usage().aggregate(&UsageQuery::default()).unwrap();
        assert_eq!(untitled.by_session[0].key, sid.to_string());
        assert_eq!(untitled.by_session[0].label, None);

        db.sessions()
            .set_title(sid, "visible title", TitleSource::User)
            .unwrap();
        let titled = db.usage().aggregate(&UsageQuery::default()).unwrap();
        assert_eq!(titled.by_session[0].label.as_deref(), Some("visible title"));
    }

    /// Auxiliary calls must not displace the compaction signal.
    #[test]
    fn compaction_signal_ignores_auxiliary_calls() {
        let (db, sid) = setup();
        let u = db.usage();
        u.record(usage(sid, Purpose::Main, 90_000)).unwrap();
        u.record(usage(sid, Purpose::Title, 300)).unwrap();
        u.record(usage(sid, Purpose::Approval, 500)).unwrap();
        u.record(usage(sid, Purpose::Agent("researcher".into()), 1_000))
            .unwrap();

        assert_eq!(
            u.last_main_input_tokens(sid).unwrap(),
            Some(90_000),
            "if an auxiliary call displaced this, compaction would not fire when it should"
        );

        // A later main round may be smaller after compaction. Context is the latest main input,
        // not the session's historical high-water mark.
        u.record(usage(sid, Purpose::Main, 12_000)).unwrap();
        assert_eq!(u.last_main_input_tokens(sid).unwrap(), Some(12_000));
    }

    #[test]
    fn no_main_usage_yet_is_none_not_zero() {
        let (db, sid) = setup();
        assert_eq!(db.usage().last_main_input_tokens(sid).unwrap(), None);
    }

    /// Providers report cumulative figures. Accumulating would multiply them.
    #[test]
    fn round_usage_is_overwritten_not_accumulated() {
        let (db, sid) = setup();
        let u = db.usage();
        let turn = TurnId::new();
        let round = RoundId::new();
        for input in [100, 250, 400] {
            u.upsert_round(usage(sid, Purpose::Main, input).in_round(turn, round))
                .unwrap();
        }
        assert_eq!(u.total_for_session(sid).unwrap().input, 400);
        assert_eq!(u.list_for_session(sid).unwrap().len(), 1);
    }

    /// Deleting a session must not make historical spend shrink.
    #[test]
    fn usage_survives_session_deletion() {
        let (db, sid) = setup();
        db.usage().record(usage(sid, Purpose::Main, 100)).unwrap();
        db.sessions().delete(sid).unwrap();

        let n: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM usage_event", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "deliberately no foreign key");
        assert_eq!(db.usage().total_for_session(sid).unwrap().input, 100);
    }

    #[test]
    fn tree_total_includes_sub_agents() {
        let db = Db::open_in_memory().unwrap();
        let root = db
            .sessions()
            .create(NewSession::root(WorkspaceId::new()))
            .unwrap();
        let child = db
            .sessions()
            .create(NewSession::child(root.session_id, "researcher"))
            .unwrap();

        db.usage()
            .record(usage(root.session_id, Purpose::Main, 100))
            .unwrap();
        db.usage()
            .record(usage(
                child.session_id,
                Purpose::Agent("researcher".into()),
                400,
            ))
            .unwrap();

        assert_eq!(
            db.usage().total_for_session(root.session_id).unwrap().input,
            100
        );
        assert_eq!(
            db.usage().total_for_tree(root.session_id).unwrap().input,
            500
        );
        assert_eq!(
            db.usage()
                .last_conversation_input_tokens(root.session_id)
                .unwrap(),
            Some(100)
        );
        assert_eq!(
            db.usage()
                .last_conversation_input_tokens(child.session_id)
                .unwrap(),
            Some(400),
            "a child must drive its own compaction threshold"
        );
    }

    #[test]
    fn self_only_scopes_to_one_session_not_the_whole_tree() {
        let db = Db::open_in_memory().unwrap();
        let root = db
            .sessions()
            .create(NewSession::root(WorkspaceId::new()))
            .unwrap();
        let child = db
            .sessions()
            .create(NewSession::child(root.session_id, "researcher"))
            .unwrap();
        db.usage()
            .record(usage(root.session_id, Purpose::Main, 100))
            .unwrap();
        db.usage()
            .record(usage(
                child.session_id,
                Purpose::Agent("researcher".into()),
                400,
            ))
            .unwrap();

        let child_as_root = db
            .usage()
            .aggregate(&UsageQuery {
                session_id: Some(child.session_id),
                self_only: false,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            child_as_root
                .total
                .iter()
                .map(|part| part.tokens.input)
                .sum::<u64>(),
            0,
            "a non-root id matches nothing under the root-scoped filter"
        );

        let child_self = db
            .usage()
            .aggregate(&UsageQuery {
                session_id: Some(child.session_id),
                self_only: true,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            child_self
                .total
                .iter()
                .map(|part| part.tokens.input)
                .sum::<u64>(),
            400
        );

        let root_self = db
            .usage()
            .aggregate(&UsageQuery {
                session_id: Some(root.session_id),
                self_only: true,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            root_self
                .total
                .iter()
                .map(|part| part.tokens.input)
                .sum::<u64>(),
            100
        );
    }

    /// A title / commit-message / task-draft call belongs to no turn, and must not invent one:
    /// the phantom inflated `COUNT(DISTINCT turn_id)` and could be selected as the "latest turn",
    /// producing a panel whose entire content is one auxiliary call.
    #[test]
    fn a_call_with_no_turn_neither_counts_as_a_turn_nor_becomes_the_latest_one() {
        let (db, sid) = setup();
        let real_turn = TurnId::new();
        db.usage()
            .record(usage(sid, Purpose::Main, 100).in_round(real_turn, RoundId::new()))
            .unwrap();
        // Recorded after the conversational round, which is when a title refine actually lands.
        db.usage()
            .record(usage(sid, Purpose::Title, 20).in_detached_round(RoundId::new()))
            .unwrap();

        let aggregate = db
            .usage()
            .aggregate(&UsageQuery {
                session_id: Some(sid),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            aggregate.total.iter().map(|part| part.turns).sum::<u32>(),
            1,
            "the title call is not a turn"
        );
        assert_eq!(
            aggregate
                .latest_turn
                .iter()
                .map(|part| part.tokens.input)
                .sum::<u64>(),
            100,
            "the latest turn is the conversation's, not the title call's"
        );
        // Still counted where it belongs: spend and the aux breakdown.
        assert_eq!(
            aggregate.total.iter().map(|part| part.calls).sum::<u32>(),
            2
        );
        assert_eq!(aggregate.by_aux_purpose[0].key, "title");
    }

    #[test]
    fn session_report_latest_turn_is_the_root_turn_not_a_later_child_turn() {
        let db = Db::open_in_memory().unwrap();
        let root = db
            .sessions()
            .create(NewSession::root(WorkspaceId::new()))
            .unwrap();
        let child = db
            .sessions()
            .create(NewSession::child(root.session_id, "reviewer"))
            .unwrap();
        let root_turn = TurnId::new();
        db.usage()
            .record(usage(root.session_id, Purpose::Main, 100).in_round(root_turn, RoundId::new()))
            .unwrap();
        db.usage()
            .record(
                usage(child.session_id, Purpose::Agent("reviewer".into()), 900)
                    .in_round(TurnId::new(), RoundId::new()),
            )
            .unwrap();

        let aggregate = db
            .usage()
            .aggregate(&UsageQuery {
                session_id: Some(root.session_id),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            aggregate
                .latest_turn
                .iter()
                .map(|part| part.tokens.input)
                .sum::<u64>(),
            100
        );
        assert_eq!(
            aggregate
                .total
                .iter()
                .map(|part| part.tokens.input)
                .sum::<u64>(),
            1_000,
            "the session total still includes the whole agent tree"
        );
    }

    #[test]
    fn cost_and_provenance_round_trip_as_typed_values() {
        let (db, sid) = setup();
        db.usage()
            .record(usage(sid, Purpose::Main, 10).with_cost(
                0.0123,
                "USD",
                CostSource::ProviderReported,
            ))
            .unwrap();

        let r = &db.usage().list_for_session(sid).unwrap()[0];
        assert_eq!(r.cost_source, Some(CostSource::ProviderReported));
        assert_eq!(r.purpose, Purpose::Main);
        assert!((db.usage().cost_for_session(sid).unwrap() - 0.0123).abs() < 1e-9);
    }

    #[test]
    fn agent_purpose_keeps_its_name_through_the_database() {
        let (db, sid) = setup();
        db.usage()
            .record(usage(sid, Purpose::Agent("reviewer".into()), 5))
            .unwrap();
        let raw: String = db
            .conn()
            .query_row("SELECT purpose FROM usage_event", [], |r| r.get(0))
            .unwrap();
        assert_eq!(raw, "agent:reviewer");
        assert_eq!(
            db.usage().list_for_session(sid).unwrap()[0].purpose,
            Purpose::Agent("reviewer".into())
        );
    }

    #[test]
    fn per_turn_totals_are_scoped_to_the_turn() {
        let (db, sid) = setup();
        let t1 = TurnId::new();
        let t2 = TurnId::new();
        db.usage()
            .record(usage(sid, Purpose::Main, 100).in_round(t1, RoundId::new()))
            .unwrap();
        db.usage()
            .record(usage(sid, Purpose::Main, 50).in_round(t2, RoundId::new()))
            .unwrap();
        assert_eq!(db.usage().total_for_turn(t1).unwrap().input, 100);
    }

    #[test]
    fn report_aggregation_happens_in_sql_with_exact_distinct_counts() {
        let (db, sid) = setup();
        let first_turn = TurnId::new();
        let latest_turn = TurnId::new();

        let mut first = usage(sid, Purpose::Main, 100).in_round(first_turn, RoundId::new());
        first.model_ref = Some("openai:gpt:2026".into());
        first = first.with_cost(1.0, "USD", CostSource::LocalPricing);
        db.usage().record(first).unwrap();

        let mut second = usage(sid, Purpose::Main, 250).in_round(latest_turn, RoundId::new());
        second.model_ref = Some("deepseek:v4".into());
        second = second.with_cost(7.0, "CNY", CostSource::ProviderReported);
        db.usage().record(second).unwrap();

        db.usage().record(usage(sid, Purpose::Title, 20)).unwrap();
        db.conn()
            .execute(
                "UPDATE usage_event SET created_at = '2026-07-30T23:30:00Z'",
                [],
            )
            .unwrap();

        let query = UsageQuery {
            session_id: Some(sid),
            utc_offset_minutes: 480,
            ..Default::default()
        };
        let aggregate = db.usage().aggregate(&query).unwrap();
        let total_calls: u32 = aggregate.total.iter().map(|part| part.calls).sum();
        let total_sessions: u32 = aggregate.total.iter().map(|part| part.sessions).sum();
        let total_turns: u32 = aggregate.total.iter().map(|part| part.turns).sum();
        let total_input: u64 = aggregate.total.iter().map(|part| part.tokens.input).sum();

        assert_eq!(total_calls, 3);
        assert_eq!(
            total_sessions, 1,
            "cost fragments must not duplicate DISTINCT"
        );
        assert_eq!(total_turns, 2, "cost fragments must not duplicate DISTINCT");
        assert_eq!(total_input, 370);
        assert_eq!(aggregate.by_model.len(), 2);
        let providers: Vec<&str> = aggregate
            .by_provider
            .iter()
            .map(|group| group.key.as_str())
            .collect();
        assert_eq!(providers, ["deepseek", "openai"]);
        assert_eq!(aggregate.by_day[0].key, "2026-07-31");
        assert_eq!(aggregate.by_aux_purpose[0].key, "title");
        assert_eq!(
            aggregate.current_context_tokens,
            Some(250),
            "session context comes from the latest main usage, not MAX(input)"
        );
        assert_eq!(
            aggregate
                .latest_turn
                .iter()
                .map(|part| part.tokens.input)
                .sum::<u64>(),
            250,
            "SQL selects the latest non-null turn before aggregating it"
        );

        assert!(db.usage().aggregate_tools(&query).unwrap().is_empty());
    }

    #[test]
    fn usage_history_rejects_an_unqualified_model_ref() {
        let (db, sid) = setup();
        let mut row = usage(sid, Purpose::Main, 10);
        row.model_ref = Some("gpt-5".into());
        assert!(db.usage().record(row).is_err());
    }
}
