//! The `session` table.
//! Holds identity and mutable session settings. Two things it deliberately does **not** hold:
//! - **Lock state.** That is [`crate::lock::SessionLockStore`].
//! - **The workspace directory.** A session stores `workspace_id` — a pointer — and only records
//!   `exec_cwd` when it deviates from wherever that workspace currently points. The root is
//!   injected per turn by the engine. Storing the path here would mean migrating every session row
//!   when a workspace moves, and any row that was missed would quietly keep running in the old
//!   directory.
//! This is also why `core` never sees a workspace: it is handed a root and a session, and the
//! mapping between them is the engine's business.

use chrono::{DateTime, Utc};
use rusqlite::types::{FromSql, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{Connection, OptionalExtension, named_params};
use serde::{Deserialize, Serialize};
use zlogic_protocol::{SessionId, WorkspaceId, define_enum_wire, impl_enum_sql};

use crate::{Result, StoreError, now};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    Chat,
    Task,
}

define_enum_wire!(SessionKind {
    Chat => "chat",
    Task => "task",
});

impl_enum_sql!(SessionKind);

/// Where a title came from, which decides whether a higher layer may replace it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TitleSource {
    /// Deterministic draft from the first user input. Replaceable.
    Draft,
    /// LLM refinement, or the model setting it through a tool. Protected.
    Model,
    /// The user renamed it. Protected from every automatic path.
    User,
}

define_enum_wire!(TitleSource {
    Draft => "draft",
    Model => "model",
    User => "user",
});

impl_enum_sql!(TitleSource);

impl TitleSource {
    /// Whether `incoming` is allowed to overwrite a title that came from `self`.
    /// Living here rather than at the call sites means no caller can accidentally clobber a
    /// name the user typed.
    pub fn can_be_replaced_by(self, incoming: TitleSource) -> bool {
        match self {
            TitleSource::Draft => true,
            TitleSource::Model => incoming == TitleSource::User,
            TitleSource::User => false,
        }
    }
}

/// The chain of agent profile names from the root down to this session.
/// A materialised path: `main/researcher/reviewer`. Stored '/'-joined so that a whole
/// sub-tree is a `LIKE 'main/researcher/%'` prefix scan and depth is `len()` with no
/// recursive query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentPath(pub Vec<String>);

impl AgentPath {
    pub const SEP: char = '/';

    pub fn root(agent: impl Into<String>) -> Self {
        Self(vec![agent.into()])
    }

    /// This path extended by one child agent.
    pub fn child(&self, agent: impl Into<String>) -> Self {
        let mut v = self.0.clone();
        v.push(agent.into());
        Self(v)
    }

    /// The leaf — this session's own agent.
    pub fn leaf(&self) -> &str {
        self.0.last().map(String::as_str).unwrap_or("main")
    }

    /// 0 for a root session.
    pub fn depth(&self) -> usize {
        self.0.len().saturating_sub(1)
    }

    pub fn as_string(&self) -> String {
        self.0.join(&Self::SEP.to_string())
    }

    /// The `LIKE` pattern matching every strict descendant.
    pub fn descendant_pattern(&self) -> String {
        format!("{}{}%", self.as_string(), Self::SEP)
    }

    fn parse(s: &str) -> Self {
        Self(
            s.split(Self::SEP)
                .filter(|p| !p.is_empty())
                .map(str::to_string)
                .collect(),
        )
    }
}

impl std::fmt::Display for AgentPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.as_string())
    }
}

impl ToSql for AgentPath {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_string()))
    }
}

impl FromSql for AgentPath {
    fn column_result(v: ValueRef<'_>) -> FromSqlResult<Self> {
        Ok(Self::parse(v.as_str()?))
    }
}

#[derive(Debug, Clone)]
pub struct NewSession {
    pub workspace_id: WorkspaceId,
    pub kind: SessionKind,
    /// Only set when tools should run somewhere other than the workspace root.
    pub exec_cwd: Option<String>,
    /// Agent profile name; `main` for a root session.
    pub agent: Option<String>,
    pub parent_session_id: Option<SessionId>,
    pub model_ref: Option<String>,
    pub effort: Option<String>,
}

impl NewSession {
    pub fn root(workspace_id: WorkspaceId) -> Self {
        Self {
            workspace_id,
            kind: SessionKind::Chat,
            exec_cwd: None,
            agent: None,
            parent_session_id: None,
            model_ref: None,
            effort: None,
        }
    }

    /// A scheduler-owned root session. Each Job gets one and every scheduled run is spawned below
    /// it, so task history never shares context or mailboxes with a user's chat session.
    pub fn task(workspace_id: WorkspaceId) -> Self {
        Self {
            kind: SessionKind::Task,
            ..Self::root(workspace_id)
        }
    }

    /// A sub-agent's session.
    /// Created by the spawner **before** the sub-agent runs, so its entries and usage have
    /// somewhere to go. Workspace, root and agent path are all inherited from the parent.
    pub fn child(parent: SessionId, agent: impl Into<String>) -> Self {
        Self {
            // Replaced with the parent's during `create`.
            workspace_id: WorkspaceId::new(),
            // Replaced with the parent's during `create`.
            kind: SessionKind::Chat,
            exec_cwd: None,
            agent: Some(agent.into()),
            parent_session_id: Some(parent),
            model_ref: None,
            effort: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionRecord {
    pub session_id: SessionId,
    pub workspace_id: WorkspaceId,
    pub kind: SessionKind,
    /// Set only when it deviates from the workspace root — see the module docs.
    pub exec_cwd: Option<String>,
    pub agent: String,
    pub agent_paths: AgentPath,
    pub parent_session_id: Option<SessionId>,
    pub root_session_id: SessionId,
    pub title: Option<String>,
    pub title_source: Option<TitleSource>,
    pub model_ref: Option<String>,
    pub effort: Option<String>,
    /// `COUNT(DISTINCT turn_seq)` of conversation (`kind != 'event'`) entries, maintained by
    /// [`crate::entry::EntryStore`]. Read straight out of the row — no per-list aggregation.
    pub turn_count: u32,
    /// Last message time of the last**completed** turn (the live turn's in-flight entries don't
    /// count), maintained by the entry and lock stores. `None` for a session that never finished
    /// a turn.
    pub last_message_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub archived_at: Option<DateTime<Utc>>,
}

impl SessionRecord {
    pub fn is_root(&self) -> bool {
        self.parent_session_id.is_none()
    }

    pub fn is_task_session(&self) -> bool {
        self.kind == SessionKind::Task
    }

    /// Where tools should run, given the root the engine resolved for this turn.
    /// The single place the "NULL means the workspace root" rule is applied, so no caller has to
    /// remember it.
    pub fn exec_cwd_or<'a>(&'a self, root: &'a std::path::Path) -> &'a std::path::Path {
        self.exec_cwd.as_deref().map_or(root, std::path::Path::new)
    }

    /// Whether tools run somewhere other than the workspace root.
    pub fn has_deviated_cwd(&self) -> bool {
        self.exec_cwd.is_some()
    }
}

#[derive(Debug, Clone)]
pub struct SessionQuery {
    pub workspace_id: WorkspaceId,
    pub include_sub_agents: bool,
    pub include_archived: bool,
    /// Scheduler-owned task context trees are addressed from Jobs, never from the chat sidebar.
    pub include_task_sessions: bool,
}

impl SessionQuery {
    pub fn of(workspace_id: WorkspaceId) -> Self {
        Self {
            workspace_id,
            include_sub_agents: false,
            include_archived: false,
            include_task_sessions: false,
        }
    }

    pub fn include_sub_agents(mut self, yes: bool) -> Self {
        self.include_sub_agents = yes;
        self
    }

    pub fn include_archived(mut self, yes: bool) -> Self {
        self.include_archived = yes;
        self
    }

    pub fn include_task_sessions(mut self, yes: bool) -> Self {
        self.include_task_sessions = yes;
        self
    }
}

pub struct SessionStore<'a> {
    conn: &'a Connection,
}

impl<'a> SessionStore<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub fn create(&self, new: NewSession) -> Result<SessionRecord> {
        let id = SessionId::new();
        let ts = now();
        let agent = new.agent.unwrap_or_else(|| "main".into());

        // A child inherits its parent's root and workspace, and extends its agent path.
        let (root, workspace_id, kind, agent_paths) = match &new.parent_session_id {
            Some(parent) => {
                let p = self.get(*parent)?;
                (
                    p.root_session_id,
                    p.workspace_id,
                    p.kind,
                    p.agent_paths.child(&agent),
                )
            }
            None => (id, new.workspace_id, new.kind, AgentPath::root(&agent)),
        };

        self.conn.execute(
            "INSERT INTO session (session_id, workspace_id, kind, exec_cwd, agent, agent_paths,
                                  parent_session_id, root_session_id, model_ref, effort,
                                  created_at, updated_at)
             VALUES (:session_id, :workspace_id, :kind, :exec_cwd, :agent, :agent_paths,
                     :parent_session_id, :root_session_id, :model_ref, :effort, :ts, :ts)",
            named_params! {
                ":session_id": id,
                ":workspace_id": workspace_id,
                ":kind": kind,
                ":exec_cwd": new.exec_cwd,
                ":agent": agent,
                ":agent_paths": agent_paths,
                ":parent_session_id": new.parent_session_id,
                ":root_session_id": root,
                ":model_ref": new.model_ref,
                ":effort": new.effort,
                ":ts": ts,
            },
        )?;
        self.get(id)
    }

    pub fn get(&self, session_id: SessionId) -> Result<SessionRecord> {
        self.find(session_id)?
            .ok_or(StoreError::NoSuchSession(session_id))
    }

    pub fn find(&self, session_id: SessionId) -> Result<Option<SessionRecord>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {COLS} FROM session WHERE session_id = :session_id"),
                named_params! { ":session_id": session_id },
                map_row,
            )
            .optional()?)
    }

    /// Top-level, non-archived sessions in a workspace — what the session list shows.
    /// Sub-agent sessions are excluded: they are not addressable by the UI, only reachable
    /// by drilling into the parent timeline's tool block.
    pub fn list(&self, workspace_id: WorkspaceId) -> Result<Vec<SessionRecord>> {
        self.query(&SessionQuery::of(workspace_id))
    }

    /// Lists with the filters spelled out.
    /// The two flags are **not** conveniences on top of `list`: the UI exposes both
    /// ("show archived", "show sub-agent transcripts"), so hard-coding them in SQL made those
    /// options unsatisfiable — the request field existed and did nothing.
    pub fn query(&self, q: &SessionQuery) -> Result<Vec<SessionRecord>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {COLS} FROM session
             WHERE workspace_id = :workspace_id
               AND (:include_sub_agents OR parent_session_id IS NULL)
               AND (:include_archived   OR archived_at IS NULL)
               AND (:include_task_sessions OR kind = 'chat')
             ORDER BY updated_at DESC"
        ))?;
        let rows = st.query_map(
            named_params! {
                ":workspace_id": q.workspace_id,
                ":include_sub_agents": q.include_sub_agents,
                ":include_archived": q.include_archived,
                ":include_task_sessions": q.include_task_sessions,
            },
            map_row,
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn children(&self, parent: SessionId) -> Result<Vec<SessionRecord>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {COLS} FROM session
             WHERE parent_session_id = :parent ORDER BY created_at"
        ))?;
        let rows = st.query_map(named_params! { ":parent": parent }, map_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Every session in the tree rooted at `root` (including `root` itself).
    pub fn tree(&self, root: SessionId) -> Result<Vec<SessionRecord>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {COLS} FROM session WHERE root_session_id = :root ORDER BY created_at"
        ))?;
        let rows = st.query_map(named_params! { ":root": root }, map_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Strict descendants by agent path — a prefix scan, no recursion.
    pub fn descendants_by_path(
        &self,
        root: SessionId,
        path: &AgentPath,
    ) -> Result<Vec<SessionRecord>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {COLS} FROM session
             WHERE root_session_id = :root AND agent_paths LIKE :pattern
             ORDER BY created_at"
        ))?;
        let rows = st.query_map(
            named_params! { ":root": root, ":pattern": path.descendant_pattern() },
            map_row,
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Records a deviation from the workspace root — entering a worktree.
    pub fn set_exec_cwd(&self, session_id: SessionId, exec_cwd: &str) -> Result<()> {
        self.update_one(
            "UPDATE session SET exec_cwd = :value, updated_at = :ts WHERE session_id = :session_id",
            session_id,
            exec_cwd,
        )
    }

    /// Back to the workspace root — leaving a worktree.
    /// Clearing rather than writing the root's path is what keeps the row correct if the workspace
    /// is later pointed somewhere else.
    pub fn clear_exec_cwd(&self, session_id: SessionId) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE session SET exec_cwd = NULL, updated_at = :ts WHERE session_id = :session_id",
            named_params! { ":ts": now(), ":session_id": session_id },
        )?;
        if n == 0 {
            return Err(StoreError::NoSuchSession(session_id));
        }
        Ok(())
    }

    pub fn set_model_ref(&self, session_id: SessionId, model_ref: &str) -> Result<()> {
        self.update_one(
            "UPDATE session SET model_ref = :value, updated_at = :ts WHERE session_id = :session_id",
            session_id,
            model_ref,
        )
    }

    pub fn set_effort(&self, session_id: SessionId, effort: &str) -> Result<()> {
        self.update_one(
            "UPDATE session SET effort = :value, updated_at = :ts WHERE session_id = :session_id",
            session_id,
            effort,
        )
    }

    /// Sets the title if `source` outranks whatever is there. Returns whether it wrote.
    pub fn set_title(
        &self,
        session_id: SessionId,
        title: &str,
        source: TitleSource,
    ) -> Result<bool> {
        let current = self.get(session_id)?;
        if let Some(existing) = current.title_source
            && !existing.can_be_replaced_by(source)
        {
            return Ok(false);
        }
        self.conn.execute(
            "UPDATE session SET title = :title, title_source = :source, updated_at = :ts
             WHERE session_id = :session_id",
            named_params! {
                ":title": title,
                ":source": source,
                ":ts": now(),
                ":session_id": session_id,
            },
        )?;
        Ok(true)
    }

    pub fn clear_title(&self, session_id: SessionId) -> Result<()> {
        self.conn.execute(
            "UPDATE session SET title = NULL, title_source = NULL, updated_at = :ts
             WHERE session_id = :session_id",
            named_params! { ":ts": now(), ":session_id": session_id },
        )?;
        Ok(())
    }

    pub fn archive(&self, session_id: SessionId) -> Result<()> {
        self.conn.execute(
            "UPDATE session SET archived_at = :ts, updated_at = :ts WHERE session_id = :session_id",
            named_params! { ":ts": now(), ":session_id": session_id },
        )?;
        Ok(())
    }

    /// Deletes the session plus its entries, mailbox rows and lock (foreign key cascade).
    /// **Usage rows are kept on purpose** — they have no foreign key, because deleting a
    /// session must not make historical spend reporting shrink.
    pub fn delete(&self, session_id: SessionId) -> Result<()> {
        self.conn.execute(
            "DELETE FROM session WHERE session_id = :session_id",
            named_params! { ":session_id": session_id },
        )?;
        Ok(())
    }

    fn update_one(&self, sql: &str, session_id: SessionId, value: &str) -> Result<()> {
        let n = self.conn.execute(
            sql,
            named_params! { ":value": value, ":ts": now(), ":session_id": session_id },
        )?;
        if n == 0 {
            return Err(StoreError::NoSuchSession(session_id));
        }
        Ok(())
    }
}

const COLS: &str = "session_id, workspace_id, kind, exec_cwd, agent, agent_paths,
     parent_session_id, root_session_id, title, title_source, model_ref, effort,
     turn_count, last_message_at, created_at, updated_at, archived_at";

fn map_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRecord> {
    Ok(SessionRecord {
        session_id: r.get("session_id")?,
        workspace_id: r.get("workspace_id")?,
        kind: r.get("kind")?,
        exec_cwd: r.get("exec_cwd")?,
        agent: r.get("agent")?,
        agent_paths: r.get("agent_paths")?,
        parent_session_id: r.get("parent_session_id")?,
        root_session_id: r.get("root_session_id")?,
        title: r.get("title")?,
        title_source: r.get("title_source")?,
        model_ref: r.get("model_ref")?,
        effort: r.get("effort")?,
        turn_count: r.get::<_, i64>("turn_count")? as u32,
        last_message_at: r.get("last_message_at")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
        archived_at: r.get("archived_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;

    fn db() -> Db {
        Db::open_in_memory().unwrap()
    }

    fn root(db: &Db) -> SessionRecord {
        db.sessions()
            .create(NewSession::root(WorkspaceId::new()))
            .unwrap()
    }

    #[test]
    fn create_fills_derived_fields() {
        let db = db();
        let s = root(&db);
        assert_eq!(
            s.exec_cwd, None,
            "no deviation from the workspace root by default"
        );
        assert_eq!(s.agent, "main");
        assert_eq!(s.agent_paths.as_string(), "main");
        assert_eq!(s.agent_paths.depth(), 0);
        assert_eq!(s.root_session_id, s.session_id, "a root points at itself");
        assert!(s.is_root());
    }

    /// Timestamps are real datetimes, stored as readable text whose lexicographic order
    /// matches chronological order — which is what `ORDER BY updated_at` relies on.
    #[test]
    fn timestamps_are_datetimes_and_sort_correctly_as_text() {
        let db = db();
        let a = root(&db);
        assert!(a.created_at <= Utc::now());

        let b = root(&db);
        let mut raw: Vec<String> = db
            .conn()
            .prepare("SELECT created_at FROM session ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(raw.len(), 2);
        assert!(
            raw[0].contains('-') && raw[0].contains(':'),
            "human-readable: {:?}",
            raw[0]
        );

        let by_text = {
            let mut c = raw.clone();
            c.sort();
            c
        };
        raw.sort_by_key(|_| 0); // keep insertion order for the comparison below
        assert_eq!(by_text[0], raw[0], "text order must match insertion order");
        assert!(a.created_at <= b.created_at);
    }

    #[test]
    fn agent_paths_accumulate_down_the_tree() {
        let db = db();
        let a = root(&db);
        let b = db
            .sessions()
            .create(NewSession::child(a.session_id, "researcher"))
            .unwrap();
        let c = db
            .sessions()
            .create(NewSession::child(b.session_id, "reviewer"))
            .unwrap();

        assert_eq!(b.agent_paths.as_string(), "main/researcher");
        assert_eq!(c.agent_paths.as_string(), "main/researcher/reviewer");
        assert_eq!(c.agent_paths.depth(), 2);
        assert_eq!(c.agent_paths.leaf(), "reviewer");
        // Children inherit the root, so the whole tree is one flat query.
        assert_eq!(c.root_session_id, a.session_id);
        assert_eq!(c.workspace_id, a.workspace_id, "workspace is inherited too");
    }

    #[test]
    fn descendants_are_found_by_path_prefix() {
        let db = db();
        let a = root(&db);
        let b = db
            .sessions()
            .create(NewSession::child(a.session_id, "researcher"))
            .unwrap();
        db.sessions()
            .create(NewSession::child(b.session_id, "reviewer"))
            .unwrap();
        db.sessions()
            .create(NewSession::child(a.session_id, "other"))
            .unwrap();

        let under_researcher = db
            .sessions()
            .descendants_by_path(a.session_id, &b.agent_paths)
            .unwrap();
        assert_eq!(under_researcher.len(), 1);
        assert_eq!(under_researcher[0].agent, "reviewer");

        assert_eq!(db.sessions().tree(a.session_id).unwrap().len(), 4);
    }

    /// Sub-agent sessions are not addressable by the UI.
    #[test]
    fn list_hides_sub_agent_sessions() {
        let db = db();
        let a = root(&db);
        db.sessions()
            .create(NewSession::child(a.session_id, "researcher"))
            .unwrap();

        assert_eq!(db.sessions().list(a.workspace_id).unwrap().len(), 1);
        assert_eq!(db.sessions().children(a.session_id).unwrap().len(), 1);
    }

    #[test]
    fn normal_list_hides_task_session_trees() {
        let db = db();
        let workspace_id = WorkspaceId::new();
        let chat = db
            .sessions()
            .create(NewSession::root(workspace_id))
            .unwrap();
        let task = db
            .sessions()
            .create(NewSession::task(workspace_id))
            .unwrap();
        db.sessions()
            .create(NewSession::child(task.session_id, "reviewer"))
            .unwrap();

        assert!(task.is_task_session());
        assert_eq!(db.sessions().list(workspace_id).unwrap(), vec![chat]);
        let all = db
            .sessions()
            .query(
                &SessionQuery::of(workspace_id)
                    .include_sub_agents(true)
                    .include_task_sessions(true),
            )
            .unwrap();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn title_precedence_protects_user_edits() {
        let db = db();
        let s = root(&db);
        let st = db.sessions();

        assert!(
            st.set_title(s.session_id, "draft", TitleSource::Draft)
                .unwrap()
        );
        assert!(
            st.set_title(s.session_id, "refined", TitleSource::Model)
                .unwrap()
        );
        assert!(
            !st.set_title(s.session_id, "another draft", TitleSource::Draft)
                .unwrap()
        );
        assert_eq!(
            st.get(s.session_id).unwrap().title.as_deref(),
            Some("refined")
        );

        assert!(
            st.set_title(s.session_id, "mine", TitleSource::User)
                .unwrap()
        );
        assert!(
            !st.set_title(s.session_id, "model again", TitleSource::Model)
                .unwrap()
        );
        assert_eq!(st.get(s.session_id).unwrap().title.as_deref(), Some("mine"));
    }

    #[test]
    fn title_source_round_trips_through_its_wire_name() {
        let db = db();
        let s = root(&db);
        db.sessions()
            .set_title(s.session_id, "t", TitleSource::Model)
            .unwrap();
        let raw: String = db
            .conn()
            .query_row("SELECT title_source FROM session", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            raw, "model",
            "stored as an explicit wire name, not a discriminant"
        );
        assert_eq!(
            db.sessions().get(s.session_id).unwrap().title_source,
            Some(TitleSource::Model)
        );
    }

    /// An unknown enum value must be loud, not silently defaulted.
    #[test]
    fn unknown_title_source_is_an_error() {
        let db = db();
        let s = root(&db);
        db.sessions()
            .set_title(s.session_id, "t", TitleSource::Draft)
            .unwrap();
        db.conn()
            .execute("UPDATE session SET title_source = 'from_the_future'", [])
            .unwrap();
        assert!(db.sessions().get(s.session_id).is_err());
    }

    /// The workspace root is injected per turn, so a session only records a *deviation* from it.
    #[test]
    fn exec_cwd_records_only_a_deviation() {
        use std::path::Path;

        let db = db();
        let s = root(&db);
        // No deviation: tools run wherever the workspace currently points.
        assert_eq!(s.exec_cwd_or(Path::new("/proj")), Path::new("/proj"));
        assert!(!s.has_deviated_cwd());

        db.sessions()
            .set_exec_cwd(s.session_id, "/proj/.worktrees/feat")
            .unwrap();
        let s = db.sessions().get(s.session_id).unwrap();
        assert!(s.has_deviated_cwd());
        assert_eq!(
            s.exec_cwd_or(Path::new("/proj")),
            Path::new("/proj/.worktrees/feat")
        );

        db.sessions().clear_exec_cwd(s.session_id).unwrap();
        let s = db.sessions().get(s.session_id).unwrap();
        assert_eq!(
            s.exec_cwd, None,
            "leaving a worktree clears it rather than writing the root"
        );
    }

    /// The point of not storing the root: moving the workspace needs no migration.
    #[test]
    fn moving_the_workspace_needs_no_session_migration() {
        use std::path::Path;

        let db = db();
        let s = root(&db);
        // Same row, different injected root — and it just follows.
        assert_eq!(
            s.exec_cwd_or(Path::new("/old/place")),
            Path::new("/old/place")
        );
        assert_eq!(
            s.exec_cwd_or(Path::new("/new/place")),
            Path::new("/new/place")
        );
    }

    #[test]
    fn missing_session_is_a_typed_error() {
        let db = db();
        let ghost = SessionId::new();
        assert!(matches!(
            db.sessions().get(ghost),
            Err(StoreError::NoSuchSession(_))
        ));
        assert!(db.sessions().find(ghost).unwrap().is_none());
    }

    #[test]
    fn session_rejects_an_unqualified_model_ref() {
        let db = db();
        let session = root(&db);
        assert!(
            db.sessions()
                .set_model_ref(session.session_id, "gpt-5")
                .is_err()
        );
        assert_eq!(
            db.sessions().get(session.session_id).unwrap().model_ref,
            None
        );
    }

    #[test]
    fn session_rejects_a_foreign_effort_name() {
        let db = db();
        let session = root(&db);
        assert!(
            db.sessions()
                .set_effort(session.session_id, "turbo")
                .is_err(),
            "the `session.effort` CHECK lets through only the six wire names"
        );
        assert_eq!(
            db.sessions().get(session.session_id).unwrap().effort,
            None,
            "a failed effort write must leave no trace behind"
        );

        db.sessions()
            .set_effort(session.session_id, "xhigh")
            .unwrap();
        assert_eq!(
            db.sessions()
                .get(session.session_id)
                .unwrap()
                .effort
                .as_deref(),
            Some("xhigh")
        );
    }

    #[test]
    fn agent_path_helpers() {
        let p = AgentPath::root("main").child("researcher");
        assert_eq!(p.as_string(), "main/researcher");
        assert_eq!(p.descendant_pattern(), "main/researcher/%");
        assert_eq!(AgentPath::parse("main/a/b").0, ["main", "a", "b"]);
        // Tolerates stray separators rather than producing empty segments.
        assert_eq!(AgentPath::parse("/main//a/").0, ["main", "a"]);
    }
}
