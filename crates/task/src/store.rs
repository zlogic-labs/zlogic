use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, named_params};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::{
    ConcurrencyPolicy, ExecutorSpec, JobDefinition, JobId, JobOwner, NewJob, NewTask,
    PermissionPolicy, ProcessResult, Schedule, TaskId, TaskResult, TaskRun, TaskState, TaskTrigger,
};

/// Initial schema owned by this crate. Hosts may install it in the shared state database.
/// `task.executor`, `permission_policy`, and `notification_session_id` are deliberately duplicated
/// from `job`: they are the immutable execution snapshot. Deleting a job sets `task.job_id` to NULL
/// and preserves both run history and completion routing.
/// `task.owner_instance` is the engine process whose memory holds the run's future; it is written
/// by the same compare-and-set that claims the row, so a run and its owner cannot disagree.
pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS engine_instance (
  instance_id  TEXT PRIMARY KEY,
  host_kind    TEXT NOT NULL,
  pid          INTEGER NOT NULL,
  started_at   TEXT NOT NULL,
  heartbeat_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_engine_instance_heartbeat
  ON engine_instance(heartbeat_at);

CREATE TABLE IF NOT EXISTS job (
  id                      INTEGER PRIMARY KEY,
  job_id                  TEXT NOT NULL UNIQUE,
  workspace_id            TEXT NOT NULL,
  owner                   TEXT NOT NULL,
  title                   TEXT NOT NULL CHECK (length(trim(title)) > 0),
  executor                TEXT NOT NULL,
  schedule                TEXT NOT NULL,
  enabled                 INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
  concurrency_policy      TEXT NOT NULL CHECK (
                            concurrency_policy IN ('allow', 'forbid', 'replace')
                          ),
  permission_policy       TEXT NOT NULL CHECK (
                            permission_policy IN ('require_interaction', 'deny_requests')
                          ),
  notification_session_id TEXT,
  created_at              TEXT NOT NULL,
  updated_at              TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_job_workspace
  ON job(workspace_id, enabled, updated_at DESC);

CREATE TABLE IF NOT EXISTS task (
  id            INTEGER PRIMARY KEY,
  task_id       TEXT NOT NULL UNIQUE,
  job_id        TEXT REFERENCES job(job_id) ON DELETE SET NULL,
  workspace_id  TEXT NOT NULL,
  executor      TEXT NOT NULL,
  permission_policy TEXT NOT NULL CHECK (
                      permission_policy IN ('require_interaction', 'deny_requests')
                    ),
  notification_session_id TEXT,
  trigger       TEXT NOT NULL,
  turn_scoped   INTEGER NOT NULL DEFAULT 1 CHECK (turn_scoped IN (0, 1)),
  state         TEXT NOT NULL CHECK (
                  state IN (
                    'queued', 'running', 'needs_input', 'succeeded',
                    'failed', 'cancelled', 'interrupted'
                  )
                ),
  attempt       INTEGER NOT NULL DEFAULT 1 CHECK (attempt >= 1),
  scheduled_for TEXT,
  started_at    TEXT,
  finished_at   TEXT,
  result        TEXT,
  error         TEXT,
  owner_instance TEXT,
  created_at    TEXT NOT NULL,
  updated_at    TEXT NOT NULL,
  CHECK (
    (state IN ('succeeded', 'failed', 'cancelled', 'interrupted') AND finished_at IS NOT NULL)
    OR
    (state NOT IN ('succeeded', 'failed', 'cancelled', 'interrupted') AND finished_at IS NULL)
  )
);
CREATE INDEX IF NOT EXISTS idx_task_job ON task(job_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_task_workspace_state
  ON task(workspace_id, state, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_task_schedule
  ON task(state, scheduled_for) WHERE state = 'queued';
CREATE UNIQUE INDEX IF NOT EXISTS idx_task_job_scheduled_once
  ON task(job_id, scheduled_for)
  WHERE job_id IS NOT NULL AND scheduled_for IS NOT NULL;
"#;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("no such {kind}: {id}")]
    NotFound { kind: &'static str, id: String },
    #[error("corrupt task row: {0}")]
    Corrupt(String),
}

pub(crate) type Result<T> = std::result::Result<T, StoreError>;

pub fn install_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)?;
    // `task.owner_instance` arrived after databases were already in the field, and SQLite has no
    // `ADD COLUMN IF NOT EXISTS`. The statement above only ever creates a table that is missing,
    // so an existing one keeps the shape it was created with and the column is added here.
    if !has_column(conn, "task", "owner_instance")? {
        conn.execute_batch("ALTER TABLE task ADD COLUMN owner_instance TEXT")?;
    }
    Ok(())
}

fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let sql = format!("PRAGMA table_info({table})");
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
    for name in rows {
        if name? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

pub struct JobStore<'a> {
    conn: &'a Connection,
}

impl<'a> JobStore<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub fn create(&self, new: NewJob) -> Result<JobDefinition> {
        let title = new.title.trim();
        if title.is_empty() {
            return Err(StoreError::Corrupt("job title must not be empty".into()));
        }
        let job_id = JobId::new();
        let now = Utc::now();
        self.conn.execute(
            "INSERT INTO job (
               job_id, workspace_id, owner, title, executor, schedule, enabled,
               concurrency_policy, permission_policy, notification_session_id,
               created_at, updated_at
             ) VALUES (
               :job_id, :workspace_id, :owner, :title, :executor, :schedule, :enabled,
               :concurrency_policy, :permission_policy, :notification_session_id,
               :created_at, :updated_at
             )",
            named_params! {
                ":job_id": job_id,
                ":workspace_id": new.workspace_id,
                ":owner": json(&new.owner)?,
                ":title": title,
                ":executor": json(&new.executor)?,
                ":schedule": json(&new.schedule)?,
                ":enabled": new.enabled,
                ":concurrency_policy": concurrency_wire(new.concurrency_policy),
                ":permission_policy": permission_wire(new.permission_policy),
                ":notification_session_id": new.notification_session_id,
                ":created_at": now,
                ":updated_at": now,
            },
        )?;
        self.get(job_id)
    }

    pub fn get(&self, job_id: JobId) -> Result<JobDefinition> {
        self.find(job_id)?.ok_or_else(|| StoreError::NotFound {
            kind: "job",
            id: job_id.to_string(),
        })
    }

    pub fn find(&self, job_id: JobId) -> Result<Option<JobDefinition>> {
        Ok(self
            .conn
            .query_row(
                "SELECT job_id, workspace_id, owner, title, executor, schedule, enabled,
                        concurrency_policy, permission_policy, notification_session_id,
                        created_at, updated_at
                   FROM job WHERE job_id = :job_id",
                named_params! { ":job_id": job_id },
                map_job,
            )
            .optional()?)
    }

    pub fn list(&self, workspace_id: zlogic_protocol::WorkspaceId) -> Result<Vec<JobDefinition>> {
        let mut statement = self.conn.prepare(
            "SELECT job_id, workspace_id, owner, title, executor, schedule, enabled,
                    concurrency_policy, permission_policy, notification_session_id,
                    created_at, updated_at
               FROM job
              WHERE workspace_id = :workspace_id
              ORDER BY id DESC",
        )?;
        let rows = statement.query_map(named_params! { ":workspace_id": workspace_id }, map_job)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn list_enabled(&self) -> Result<Vec<JobDefinition>> {
        let mut statement = self.conn.prepare(
            "SELECT job_id, workspace_id, owner, title, executor, schedule, enabled,
                    concurrency_policy, permission_policy, notification_session_id,
                    created_at, updated_at
               FROM job
              WHERE enabled = 1
              ORDER BY id",
        )?;
        let rows = statement.query_map([], map_job)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_enabled(&self, job_id: JobId, enabled: bool) -> Result<JobDefinition> {
        let changed = self.conn.execute(
            "UPDATE job SET enabled = :enabled, updated_at = :updated_at
              WHERE job_id = :job_id",
            named_params! {
                ":enabled": enabled,
                ":updated_at": Utc::now(),
                ":job_id": job_id,
            },
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound {
                kind: "job",
                id: job_id.to_string(),
            });
        }
        self.get(job_id)
    }

    pub fn delete(&self, job_id: JobId) -> Result<()> {
        let changed = self.conn.execute(
            "DELETE FROM job WHERE job_id = :job_id",
            named_params! { ":job_id": job_id },
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound {
                kind: "job",
                id: job_id.to_string(),
            });
        }
        Ok(())
    }
}

pub struct TaskStore<'a> {
    conn: &'a Connection,
}

impl<'a> TaskStore<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub fn create(&self, new: NewTask) -> Result<TaskRun> {
        if new.attempt == 0 {
            return Err(StoreError::Corrupt(
                "task attempt must be at least one".into(),
            ));
        }
        let task_id = TaskId::new();
        let now = Utc::now();
        self.conn.execute(
            "INSERT INTO task (
               task_id, job_id, workspace_id, executor, permission_policy,
               notification_session_id, trigger, turn_scoped, state, attempt,
               scheduled_for, created_at, updated_at
             ) VALUES (
               :task_id, :job_id, :workspace_id, :executor, :permission_policy,
               :notification_session_id, :trigger, :turn_scoped, 'queued', :attempt,
               :scheduled_for, :created_at, :updated_at
             )",
            named_params! {
                ":task_id": task_id,
                ":job_id": new.job_id,
                ":workspace_id": new.workspace_id,
                ":executor": json(&new.executor)?,
                ":permission_policy": permission_wire(new.permission_policy),
                ":notification_session_id": new.notification_session_id,
                ":trigger": json(&new.trigger)?,
                ":turn_scoped": new.turn_scoped,
                ":attempt": new.attempt,
                ":scheduled_for": new.scheduled_for,
                ":created_at": now,
                ":updated_at": now,
            },
        )?;
        self.get(task_id)
    }

    pub fn get(&self, task_id: TaskId) -> Result<TaskRun> {
        self.find(task_id)?.ok_or_else(|| StoreError::NotFound {
            kind: "task",
            id: task_id.to_string(),
        })
    }

    pub fn find(&self, task_id: TaskId) -> Result<Option<TaskRun>> {
        Ok(self
            .conn
            .query_row(
                "SELECT task_id, job_id, workspace_id, executor, permission_policy,
                        notification_session_id, trigger, turn_scoped, state, attempt,
                        scheduled_for, started_at, finished_at, result, error, created_at, updated_at
                   FROM task WHERE task_id = :task_id",
                named_params! { ":task_id": task_id },
                map_task,
            )
            .optional()?)
    }

    pub fn delete(&self, task_id: TaskId) -> Result<bool> {
        let changed = self.conn.execute(
            "DELETE FROM task WHERE task_id = :task_id",
            named_params! { ":task_id": task_id },
        )?;
        Ok(changed > 0)
    }

    pub fn list_for_job(&self, job_id: JobId) -> Result<Vec<TaskRun>> {
        let mut statement = self.conn.prepare(
            "SELECT task_id, job_id, workspace_id, executor, permission_policy,
                    notification_session_id, trigger, turn_scoped, state, attempt,
                    scheduled_for, started_at, finished_at, result, error, created_at, updated_at
               FROM task WHERE job_id = :job_id ORDER BY id DESC",
        )?;
        let rows = statement.query_map(named_params! { ":job_id": job_id }, map_task)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn list_active_for_job(&self, job_id: JobId) -> Result<Vec<TaskRun>> {
        let mut statement = self.conn.prepare(
            "SELECT task_id, job_id, workspace_id, executor, permission_policy,
                    notification_session_id, trigger, turn_scoped, state, attempt,
                    scheduled_for, started_at, finished_at, result, error, created_at, updated_at
               FROM task
              WHERE job_id = :job_id
                AND state IN ('queued', 'running', 'needs_input')
              ORDER BY id DESC",
        )?;
        let rows = statement.query_map(named_params! { ":job_id": job_id }, map_task)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn latest_for_job(&self, job_id: JobId) -> Result<Option<TaskRun>> {
        Ok(self
            .conn
            .query_row(
                "SELECT task_id, job_id, workspace_id, executor, permission_policy,
                        notification_session_id, trigger, turn_scoped, state, attempt,
                        scheduled_for, started_at, finished_at, result, error, created_at, updated_at
                   FROM task WHERE job_id = :job_id ORDER BY id DESC LIMIT 1",
                named_params! { ":job_id": job_id },
                map_task,
            )
            .optional()?)
    }

    /// Runs whose completion is routed to this session.
    /// The notification target is snapshotted onto the run, so this keeps working after the job
    /// that created it is edited or deleted.
    pub fn list_for_session(&self, session_id: zlogic_protocol::SessionId) -> Result<Vec<TaskRun>> {
        let mut statement = self.conn.prepare(
            "SELECT task_id, job_id, workspace_id, executor, permission_policy,
                    notification_session_id, trigger, turn_scoped, state, attempt,
                    scheduled_for, started_at, finished_at, result, error, created_at, updated_at
               FROM task
              WHERE notification_session_id = :session_id
              ORDER BY id DESC",
        )?;
        let rows = statement.query_map(named_params! { ":session_id": session_id }, map_task)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn list_active_for_session(
        &self,
        session_id: zlogic_protocol::SessionId,
    ) -> Result<Vec<TaskRun>> {
        let mut statement = self.conn.prepare(
            "SELECT task_id, job_id, workspace_id, executor, permission_policy,
                    notification_session_id, trigger, turn_scoped, state, attempt,
                    scheduled_for, started_at, finished_at, result, error, created_at, updated_at
               FROM task
              WHERE notification_session_id = :session_id
                AND state IN ('queued', 'running', 'needs_input')
              ORDER BY id DESC",
        )?;
        let rows = statement.query_map(named_params! { ":session_id": session_id }, map_task)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Every active run anywhere in the tree rooted at `root`, including runs a sub-agent started.
    ///
    /// The join is against `session.root_session_id` rather than a recursion: that column is
    /// materialised precisely so a whole tree is one flat query. A service a sub-agent started
    /// belongs to the conversation the user is reading, not to the delegated run that happened to
    /// begin it — scoping to the calling session alone would hide exactly the case a user asks
    /// about.
    pub fn list_active_for_tree(&self, root: zlogic_protocol::SessionId) -> Result<Vec<TaskRun>> {
        let mut statement = self.conn.prepare(
            "SELECT task_id, job_id, workspace_id, executor, permission_policy,
                    notification_session_id, trigger, turn_scoped, state, attempt,
                    scheduled_for, started_at, finished_at, result, error, created_at, updated_at
               FROM task
              WHERE notification_session_id IN (
                        SELECT session_id FROM session WHERE root_session_id = :root)
                AND state IN ('queued', 'running', 'needs_input')
              ORDER BY id DESC",
        )?;
        let rows = statement.query_map(named_params! { ":root": root }, map_task)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn list_active_for_workspace(
        &self,
        workspace_id: zlogic_protocol::WorkspaceId,
        only_job_tasks: bool,
    ) -> Result<Vec<TaskRun>> {
        let mut statement = self.conn.prepare(
            "SELECT task_id, job_id, workspace_id, executor, permission_policy,
                    notification_session_id, trigger, turn_scoped, state, attempt,
                    scheduled_for, started_at, finished_at, result, error, created_at, updated_at
               FROM task
              WHERE workspace_id = :workspace_id
                AND state IN ('queued', 'running', 'needs_input')
                AND (:only_job_tasks = 0 OR job_id IS NOT NULL)
              ORDER BY id DESC",
        )?;
        let rows = statement.query_map(
            named_params! {
                ":workspace_id": workspace_id,
                ":only_job_tasks": if only_job_tasks { 1 } else { 0 },
            },
            map_task,
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Rows that no live engine can finish: a `running` run, or a `needs_input` suspension, whose
    /// owning process is gone. `live_cutoff` is the heartbeat reading the caller decided on.
    ///
    /// `queued` is deliberately **not** here. A queued row is work nobody has started, and one
    /// live scheduler is enough to pick it up — a scheduled run that missed its slot while this
    /// host was down should still run, not be thrown away by whichever host noticed it. Once a row
    /// is claimed its owner is recorded, and a row whose owner is gone becomes interruptible
    /// again; until then it is nobody's to clean up.
    pub fn list_recoverable(&self, live_cutoff: DateTime<Utc>) -> Result<Vec<TaskRun>> {
        let mut statement = self.conn.prepare(
            "SELECT task_id, job_id, workspace_id, executor, permission_policy,
                    notification_session_id, trigger, turn_scoped, state, attempt,
                    scheduled_for, started_at, finished_at, result, error, created_at, updated_at
               FROM task
              WHERE state IN ('running', 'needs_input')
                AND (
                  owner_instance IS NULL
                  OR owner_instance NOT IN (
                    SELECT instance_id FROM engine_instance WHERE heartbeat_at >= :live_cutoff
                  )
                )
              ORDER BY id",
        )?;
        let rows = statement.query_map(
            named_params! { ":live_cutoff": live_cutoff },
            map_task,
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn list_terminal_for_session(
        &self,
        session_id: zlogic_protocol::SessionId,
        offset: u32,
        limit: u32,
    ) -> Result<Vec<TaskRun>> {
        let mut statement = self.conn.prepare(
            "SELECT task_id, job_id, workspace_id, executor, permission_policy,
                    notification_session_id, trigger, turn_scoped, state, attempt,
                    scheduled_for, started_at, finished_at, result, error, created_at, updated_at
               FROM task
              WHERE notification_session_id = :session_id
                AND state IN ('succeeded', 'failed', 'cancelled', 'interrupted')
              ORDER BY id DESC
              LIMIT :limit OFFSET :offset",
        )?;
        let rows = statement.query_map(
            named_params! {
                ":session_id": session_id,
                ":limit": limit,
                ":offset": offset,
            },
            map_task,
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn list_terminal_for_workspace(
        &self,
        workspace_id: zlogic_protocol::WorkspaceId,
        offset: u32,
        limit: u32,
        only_job_tasks: bool,
    ) -> Result<Vec<TaskRun>> {
        let mut statement = self.conn.prepare(
            "SELECT task_id, job_id, workspace_id, executor, permission_policy,
                    notification_session_id, trigger, turn_scoped, state, attempt,
                    scheduled_for, started_at, finished_at, result, error, created_at, updated_at
               FROM task
              WHERE workspace_id = :workspace_id
                AND state IN ('succeeded', 'failed', 'cancelled', 'interrupted')
                AND (:only_job_tasks = 0 OR job_id IS NOT NULL)
              ORDER BY id DESC
              LIMIT :limit OFFSET :offset",
        )?;
        let rows = statement.query_map(
            named_params! {
                ":workspace_id": workspace_id,
                ":only_job_tasks": if only_job_tasks { 1 } else { 0 },
                ":limit": limit,
                ":offset": offset,
            },
            map_task,
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn count_terminal_for_session(
        &self,
        session_id: zlogic_protocol::SessionId,
    ) -> Result<u64> {
        let count: i64 = self.conn.query_row(
            "SELECT count(*)
               FROM task
              WHERE notification_session_id = :session_id
                AND state IN ('succeeded', 'failed', 'cancelled', 'interrupted')",
            named_params! { ":session_id": session_id },
            |row| row.get(0),
        )?;
        Ok(count.try_into().unwrap_or(0))
    }

    pub fn count_terminal_for_workspace(
        &self,
        workspace_id: zlogic_protocol::WorkspaceId,
        only_job_tasks: bool,
    ) -> Result<u64> {
        let count: i64 = self.conn.query_row(
            "SELECT count(*)
               FROM task
              WHERE workspace_id = :workspace_id
                AND state IN ('succeeded', 'failed', 'cancelled', 'interrupted')
                AND (:only_job_tasks = 0 OR job_id IS NOT NULL)",
            named_params! {
                ":workspace_id": workspace_id,
                ":only_job_tasks": if only_job_tasks { 1 } else { 0 },
            },
            |row| row.get(0),
        )?;
        Ok(count.try_into().unwrap_or(0))
    }

    pub fn list_terminal_for_job(
        &self,
        job_id: JobId,
        offset: u32,
        limit: u32,
    ) -> Result<Vec<TaskRun>> {
        let mut statement = self.conn.prepare(
            "SELECT task_id, job_id, workspace_id, executor, permission_policy,
                    notification_session_id, trigger, turn_scoped, state, attempt,
                    scheduled_for, started_at, finished_at, result, error, created_at, updated_at
               FROM task
              WHERE job_id = :job_id
                AND state IN ('succeeded', 'failed', 'cancelled', 'interrupted')
              ORDER BY id DESC
              LIMIT :limit OFFSET :offset",
        )?;
        let rows = statement.query_map(
            named_params! {
                ":job_id": job_id,
                ":limit": limit,
                ":offset": offset,
            },
            map_task,
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn count_terminal_for_job(&self, job_id: JobId) -> Result<u64> {
        let count: i64 = self.conn.query_row(
            "SELECT count(*)
               FROM task
              WHERE job_id = :job_id
                AND state IN ('succeeded', 'failed', 'cancelled', 'interrupted')",
            named_params! { ":job_id": job_id },
            |row| row.get(0),
        )?;
        Ok(count.try_into().unwrap_or(0))
    }

    /// Interrupts a run whose owning process is gone, and only that.
    ///
    /// The guard is the point: several hosts share one database, so a bare
    /// `state = 'running'` would let whichever engine starts next reach into work a live engine is
    /// doing. Three conditions come from what the caller observed — the state it read and the
    /// heartbeat cutoff it decided on — so a run claimed between the read and this write is left
    /// alone rather than interrupted on the strength of a reading that has since expired.
    ///
    /// The state list is also pinned here rather than taken from `from`: `queued` is work no engine
    /// has started, and a live scheduler may still pick it up, so no caller can turn this into a
    /// way to throw away work that nobody owns yet.
    pub fn interrupt_orphan(
        &self,
        task_id: TaskId,
        from: TaskState,
        reason: &str,
        live_cutoff: DateTime<Utc>,
    ) -> Result<bool> {
        let now = Utc::now();
        let changed = self.conn.execute(
            "UPDATE task
                SET state = 'interrupted',
                    finished_at = :now,
                    error = :reason,
                    updated_at = :now
              WHERE task_id = :task_id
                AND state = :from_state
                AND state IN ('running', 'needs_input')
                AND (
                  owner_instance IS NULL
                  OR owner_instance NOT IN (
                    SELECT instance_id FROM engine_instance WHERE heartbeat_at >= :live_cutoff
                  )
                )",
            named_params! {
                ":now": now,
                ":reason": reason,
                ":task_id": task_id,
                ":from_state": from.as_str(),
                ":live_cutoff": live_cutoff,
            },
        )?;
        Ok(changed == 1)
    }

    /// Output objects retained by durable process-task results.
    /// These references do not live in `session_entry`, so object GC must include them explicitly.
    pub fn output_objects(&self) -> Result<Vec<zlogic_objects::ObjectId>> {
        let mut statement = self
            .conn
            .prepare("SELECT result FROM task WHERE result IS NOT NULL")?;
        let results = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut objects = Vec::new();
        for result in results {
            if let TaskResult::Process(ProcessResult {
                output_object_id: Some(id),
                ..
            }) = decode::<TaskResult>(result, "task.result").map_err(StoreError::from)?
            {
                objects.push(id);
            }
        }
        Ok(objects)
    }

    pub fn references_output(&self, object_id: &zlogic_objects::ObjectId) -> Result<bool> {
        Ok(self.output_objects()?.iter().any(|id| id == object_id))
    }

    /// Persists one state transition with compare-and-set semantics.
    /// The caller must present the state it observed. A concurrent runner winning the update is
    /// reported as corruption instead of silently overwriting its terminal result.
    pub fn transition(
        &self,
        task_id: TaskId,
        from: TaskState,
        to: TaskState,
        result: Option<TaskResult>,
        error: Option<&str>,
    ) -> Result<TaskRun> {
        if !valid_transition(from, to) {
            return Err(StoreError::Corrupt(format!(
                "invalid task transition: {} -> {}",
                from.as_str(),
                to.as_str()
            )));
        }
        if to == TaskState::Succeeded && result.is_none() {
            return Err(StoreError::Corrupt(
                "succeeded task must have a result".into(),
            ));
        }

        let now = Utc::now();
        let started_at = (to == TaskState::Running).then_some(now);
        let finished_at = to.is_terminal().then_some(now);
        let changed = self.conn.execute(
            "UPDATE task
                SET state = :to_state,
                    started_at = COALESCE(started_at, :started_at),
                    finished_at = :finished_at,
                    result = :result,
                    error = :error,
                    updated_at = :updated_at
              WHERE task_id = :task_id AND state = :from_state",
            named_params! {
                ":to_state": to.as_str(),
                ":started_at": started_at,
                ":finished_at": finished_at,
                ":result": result.as_ref().map(json).transpose()?,
                ":error": error,
                ":updated_at": now,
                ":task_id": task_id,
                ":from_state": from.as_str(),
            },
        )?;
        if changed == 0 {
            let actual = self.find(task_id)?;
            return match actual {
                None => Err(StoreError::NotFound {
                    kind: "task",
                    id: task_id.to_string(),
                }),
                Some(actual) => Err(StoreError::Corrupt(format!(
                    "task {} is {}, expected {}",
                    task_id,
                    actual.state.as_str(),
                    from.as_str()
                ))),
            };
        }
        self.get(task_id)
    }

    /// Claims a queued run for execution and records which engine process owns its runtime.
    ///
    /// Owner and state move in one statement, so there is no window in which a run is `running`
    /// with nobody accountable for it — the shape startup reconciliation would otherwise have to
    /// interrupt a healthy run it could not tell apart from an orphan.
    pub fn claim(&self, task_id: TaskId, owner_instance: &str) -> Result<TaskRun> {
        let now = Utc::now();
        let changed = self.conn.execute(
            "UPDATE task
                SET state = 'running',
                    owner_instance = :owner_instance,
                    started_at = COALESCE(started_at, :now),
                    updated_at = :now
              WHERE task_id = :task_id AND state = 'queued'",
            named_params! {
                ":owner_instance": owner_instance,
                ":now": now,
                ":task_id": task_id,
            },
        )?;
        if changed == 0 {
            let actual = self.find(task_id)?;
            return match actual {
                None => Err(StoreError::NotFound {
                    kind: "task",
                    id: task_id.to_string(),
                }),
                Some(actual) => Err(StoreError::Corrupt(format!(
                    "task {} is {}, expected queued",
                    task_id,
                    actual.state.as_str(),
                ))),
            };
        }
        self.get(task_id)
    }
}

fn valid_transition(from: TaskState, to: TaskState) -> bool {
    use TaskState::{Cancelled, Failed, Interrupted, NeedsInput, Queued, Running, Succeeded};
    matches!(
        (from, to),
        (Queued, Running | Failed | Cancelled | Interrupted)
            | (
                Running,
                NeedsInput | Succeeded | Failed | Cancelled | Interrupted
            )
            | (NeedsInput, Queued | Cancelled | Interrupted)
    )
}

fn json<T: Serialize>(value: &T) -> Result<String> {
    Ok(serde_json::to_string(value)?)
}

fn decode<T: DeserializeOwned>(value: String, field: &'static str) -> rusqlite::Result<T> {
    serde_json::from_str(&value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(StoreError::Corrupt(format!("{field}: {error}"))),
        )
    })
}

fn concurrency_wire(value: ConcurrencyPolicy) -> &'static str {
    match value {
        ConcurrencyPolicy::Allow => "allow",
        ConcurrencyPolicy::Forbid => "forbid",
        ConcurrencyPolicy::Replace => "replace",
    }
}

fn parse_concurrency(value: &str) -> rusqlite::Result<ConcurrencyPolicy> {
    match value {
        "allow" => Ok(ConcurrencyPolicy::Allow),
        "forbid" => Ok(ConcurrencyPolicy::Forbid),
        "replace" => Ok(ConcurrencyPolicy::Replace),
        other => Err(invalid_enum("concurrency_policy", other)),
    }
}

fn permission_wire(value: PermissionPolicy) -> &'static str {
    match value {
        PermissionPolicy::RequireInteraction => "require_interaction",
        PermissionPolicy::DenyRequests => "deny_requests",
    }
}

fn parse_permission(value: &str) -> rusqlite::Result<PermissionPolicy> {
    match value {
        "require_interaction" => Ok(PermissionPolicy::RequireInteraction),
        "deny_requests" => Ok(PermissionPolicy::DenyRequests),
        other => Err(invalid_enum("permission_policy", other)),
    }
}

fn invalid_enum(field: &'static str, value: &str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        Box::new(StoreError::Corrupt(format!("{field}: {value:?}"))),
    )
}

fn map_job(row: &rusqlite::Row<'_>) -> rusqlite::Result<JobDefinition> {
    Ok(JobDefinition {
        job_id: row.get(0)?,
        workspace_id: row.get(1)?,
        owner: decode::<JobOwner>(row.get(2)?, "owner")?,
        title: row.get(3)?,
        executor: decode::<ExecutorSpec>(row.get(4)?, "executor")?,
        schedule: decode::<Schedule>(row.get(5)?, "schedule")?,
        enabled: row.get(6)?,
        concurrency_policy: parse_concurrency(row.get_ref(7)?.as_str()?)?,
        permission_policy: parse_permission(row.get_ref(8)?.as_str()?)?,
        notification_session_id: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
    })
}

fn map_task(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRun> {
    let state_text: String = row.get(8)?;
    let state =
        TaskState::parse(&state_text).ok_or_else(|| invalid_enum("task.state", &state_text))?;
    let result: Option<String> = row.get(13)?;
    Ok(TaskRun {
        task_id: row.get(0)?,
        job_id: row.get(1)?,
        workspace_id: row.get(2)?,
        executor: decode::<ExecutorSpec>(row.get(3)?, "executor")?,
        permission_policy: parse_permission(row.get_ref(4)?.as_str()?)?,
        notification_session_id: row.get(5)?,
        trigger: decode::<TaskTrigger>(row.get(6)?, "trigger")?,
        turn_scoped: row.get(7)?,
        state,
        attempt: row.get(9)?,
        scheduled_for: row.get(10)?,
        started_at: row.get(11)?,
        finished_at: row.get(12)?,
        result: result
            .map(|value| decode::<TaskResult>(value, "result"))
            .transpose()?,
        error: row.get(14)?,
        created_at: row.get(15)?,
        updated_at: row.get(16)?,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use zlogic_protocol::WorkspaceId;

    use super::*;
    use crate::instance::{EngineInstance, live_cutoff, new_instance_id};
    use crate::{ExecutorSpec, NewJob, NewTask, ProcessResult, ProcessSpec, TaskResult};

    fn process() -> ExecutorSpec {
        ExecutorSpec::Process(ProcessSpec {
            program: "cargo".into(),
            args: vec!["test".into()],
            cwd: None,
            env: BTreeMap::new(),
        })
    }

    fn connection() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        install_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn job_and_task_round_trip_with_executor_snapshot() {
        let conn = connection();
        let jobs = JobStore::new(&conn);
        let tasks = TaskStore::new(&conn);
        let workspace_id = WorkspaceId::new();

        let job = jobs
            .create(NewJob::manual(workspace_id, "test workspace", process()))
            .unwrap();
        let task = tasks
            .create(NewTask::from_job(&job, TaskTrigger::Manual))
            .unwrap();

        assert_eq!(task.state, TaskState::Queued);
        assert_eq!(task.executor, job.executor);
        assert_eq!(tasks.list_for_job(job.job_id).unwrap(), vec![task]);
    }

    #[test]
    fn deleting_job_preserves_task_history() {
        let conn = connection();
        let jobs = JobStore::new(&conn);
        let tasks = TaskStore::new(&conn);
        let workspace_id = WorkspaceId::new();
        let job = jobs
            .create(NewJob::manual(workspace_id, "one shot", process()))
            .unwrap();
        let task = tasks
            .create(NewTask::from_job(&job, TaskTrigger::Manual))
            .unwrap();

        jobs.delete(job.job_id).unwrap();
        assert_eq!(tasks.get(task.task_id).unwrap().job_id, None);
    }

    #[test]
    fn transitions_are_compare_and_set() {
        let conn = connection();
        let tasks = TaskStore::new(&conn);
        let task = tasks
            .create(NewTask::manual(WorkspaceId::new(), process()))
            .unwrap();
        let running = tasks
            .transition(
                task.task_id,
                TaskState::Queued,
                TaskState::Running,
                None,
                None,
            )
            .unwrap();
        assert!(running.started_at.is_some());

        let result = TaskResult::Process(ProcessResult {
            exit_code: Some(0),
            output_object_id: None,
            output_chars: 0,
        });
        let done = tasks
            .transition(
                task.task_id,
                TaskState::Running,
                TaskState::Succeeded,
                Some(result.clone()),
                None,
            )
            .unwrap();
        assert_eq!(done.result, Some(result));
        assert!(done.finished_at.is_some());
        assert!(
            tasks
                .transition(
                    task.task_id,
                    TaskState::Running,
                    TaskState::Failed,
                    None,
                    Some("late")
                )
                .is_err()
        );
    }

    #[test]
    fn startup_interrupts_only_tasks_whose_owner_is_gone() {
        let conn = connection();
        let tasks = TaskStore::new(&conn);
        let claimed_by = new_instance_id();
        let abandoned = new_instance_id();
        EngineInstance::new(&conn)
            .register(&claimed_by, "test", 1)
            .unwrap();
        EngineInstance::new(&conn)
            .register(&abandoned, "test", 2)
            .unwrap();

        let alive = tasks
            .claim(
                tasks
                    .create(NewTask::manual(WorkspaceId::new(), process()))
                    .unwrap()
                    .task_id,
                &claimed_by,
            )
            .unwrap();
        let orphan = tasks
            .claim(
                tasks
                    .create(NewTask::manual(WorkspaceId::new(), process()))
                    .unwrap()
                    .task_id,
                &abandoned,
            )
            .unwrap();

        // The abandoned instance stops heartbeating; the live one is read one instant before the
        // cutoff, so only its rows stay off limits.
        conn.execute(
            "UPDATE engine_instance SET heartbeat_at = '1970-01-01T00:00:00+00:00'
              WHERE instance_id = ?1",
            [&abandoned],
        )
        .unwrap();
        let cutoff = live_cutoff();
        assert_eq!(tasks.list_recoverable(cutoff).unwrap().len(), 1);
        assert_eq!(
            tasks.list_recoverable(cutoff).unwrap()[0].task_id,
            orphan.task_id
        );

        assert!(!tasks
            .interrupt_orphan(alive.task_id, TaskState::Running, "gone", cutoff)
            .unwrap());
        assert_eq!(tasks.get(alive.task_id).unwrap().state, TaskState::Running);

        assert!(tasks
            .interrupt_orphan(orphan.task_id, TaskState::Running, "runtime restarted", cutoff)
            .unwrap());
        let interrupted = tasks.get(orphan.task_id).unwrap();
        assert_eq!(interrupted.state, TaskState::Interrupted);
        assert_eq!(interrupted.error.as_deref(), Some("runtime restarted"));
        assert!(interrupted.finished_at.is_some());
    }

    #[test]
    fn a_queued_run_survives_startup_reconciliation() {
        let conn = connection();
        let tasks = TaskStore::new(&conn);
        let queued = tasks
            .create(NewTask::manual(WorkspaceId::new(), process()))
            .unwrap();
        tasks
            .transition(
                queued.task_id,
                TaskState::Queued,
                TaskState::Running,
                None,
                None,
            )
            .and_then(|running| {
                tasks.transition(
                    running.task_id,
                    TaskState::Running,
                    TaskState::NeedsInput,
                    None,
                    None,
                )
            })
            .and_then(|_| {
                tasks.transition(
                    queued.task_id,
                    TaskState::NeedsInput,
                    TaskState::Queued,
                    None,
                    None,
                )
            })
            .unwrap();

        assert!(tasks.list_recoverable(live_cutoff()).unwrap().is_empty());
        assert!(!tasks
            .interrupt_orphan(queued.task_id, TaskState::Queued, "gone", live_cutoff())
            .unwrap());
        assert_eq!(tasks.get(queued.task_id).unwrap().state, TaskState::Queued);
    }

    #[test]
    fn queued_schedule_index_and_terminal_constraint_exist() {
        let conn = connection();
        let indexes: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'index'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(indexes.iter().any(|name| name == "idx_task_schedule"));

        let task_id = TaskId::new();
        let now = Utc::now();
        let result = conn.execute(
            "INSERT INTO task (
               task_id, workspace_id, executor, permission_policy, trigger, state, attempt,
               created_at, updated_at
             ) VALUES (
               :task_id, :workspace_id, '{}', 'deny_requests', '{}', 'failed', 1, :now, :now
             )",
            named_params! {
                ":task_id": task_id,
                ":workspace_id": WorkspaceId::new(),
                ":now": now,
            },
        );
        assert!(
            result.is_err(),
            "terminal task without finished_at must be rejected"
        );
    }
}
