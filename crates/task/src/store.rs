use chrono::Utc;
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
pub const SCHEMA: &str = r#"
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

type Result<T> = std::result::Result<T, StoreError>;

pub fn install_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)?;
    Ok(())
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

    /// Process-local executions that cannot still have a live runtime after this process starts.
    /// Queued and needs-input rows are included because their launch future disappeared too.
    pub fn list_recoverable(&self) -> Result<Vec<TaskRun>> {
        let mut statement = self.conn.prepare(
            "SELECT task_id, job_id, workspace_id, executor, permission_policy,
                    notification_session_id, trigger, turn_scoped, state, attempt,
                    scheduled_for, started_at, finished_at, result, error, created_at, updated_at
               FROM task
              WHERE state IN ('queued', 'running', 'needs_input')
              ORDER BY id",
        )?;
        let rows = statement.query_map([], map_task)?;
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

    /// Reconciles process-local runtimes after a restart.
    /// A row still marked `running` cannot have a live [`crate::RuntimeHandle`] in this process.
    /// It is interrupted rather than failed: the work may have produced useful partial output,
    /// and retry policy is a separate scheduler decision.
    pub fn interrupt_running(&self, reason: &str) -> Result<usize> {
        let now = Utc::now();
        Ok(self.conn.execute(
            "UPDATE task
                SET state = 'interrupted',
                    finished_at = :now,
                    error = :reason,
                    updated_at = :now
              WHERE state = 'running'",
            named_params! {
                ":now": now,
                ":reason": reason,
            },
        )?)
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
    fn startup_interrupts_orphaned_running_tasks() {
        let conn = connection();
        let tasks = TaskStore::new(&conn);
        let task = tasks
            .create(NewTask::manual(WorkspaceId::new(), process()))
            .unwrap();
        tasks
            .transition(
                task.task_id,
                TaskState::Queued,
                TaskState::Running,
                None,
                None,
            )
            .unwrap();

        assert_eq!(tasks.interrupt_running("runtime restarted").unwrap(), 1);
        let interrupted = tasks.get(task.task_id).unwrap();
        assert_eq!(interrupted.state, TaskState::Interrupted);
        assert_eq!(interrupted.error.as_deref(), Some("runtime restarted"));
        assert!(interrupted.finished_at.is_some());
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
