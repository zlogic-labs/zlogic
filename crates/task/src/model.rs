use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use zlogic_objects::ObjectId;
use zlogic_protocol::{CallId, EntryId, SessionId, TurnId, WorkspaceId};

use crate::{JobId, TaskId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobOwner {
    User,
    Agent { session_id: SessionId },
    System,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Schedule {
    Manual,
    Once {
        at: DateTime<Utc>,
    },
    Cron {
        expression: String,
        /// IANA timezone name, for example `Asia/Shanghai`.
        timezone: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConcurrencyPolicy {
    Allow,
    Forbid,
    Replace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionPolicy {
    /// The runner may suspend the run as `needs_input`.
    RequireInteraction,
    /// Unattended execution fails instead of opening an interaction.
    DenyRequests,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessSpec {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// `None` means the workspace's current root at execution time.
    pub cwd: Option<String>,
    /// Only explicit overrides. Resolved secrets must not be placed here.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSpec {
    pub prompt: String,
    pub agent: String,
    pub model_ref: Option<String>,
    /// `None` means the workspace's current root at execution time.
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExecutorSpec {
    Process(ProcessSpec),
    Agent(AgentSpec),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobDefinition {
    pub job_id: JobId,
    pub workspace_id: WorkspaceId,
    pub owner: JobOwner,
    pub title: String,
    pub executor: ExecutorSpec,
    pub schedule: Schedule,
    pub enabled: bool,
    pub concurrency_policy: ConcurrencyPolicy,
    pub permission_policy: PermissionPolicy,
    pub notification_session_id: Option<SessionId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewJob {
    pub workspace_id: WorkspaceId,
    pub owner: JobOwner,
    pub title: String,
    pub executor: ExecutorSpec,
    pub schedule: Schedule,
    pub enabled: bool,
    pub concurrency_policy: ConcurrencyPolicy,
    pub permission_policy: PermissionPolicy,
    pub notification_session_id: Option<SessionId>,
}

impl NewJob {
    pub fn manual(
        workspace_id: WorkspaceId,
        title: impl Into<String>,
        executor: ExecutorSpec,
    ) -> Self {
        Self {
            workspace_id,
            owner: JobOwner::User,
            title: title.into(),
            executor,
            schedule: Schedule::Manual,
            enabled: true,
            concurrency_policy: ConcurrencyPolicy::Forbid,
            permission_policy: PermissionPolicy::RequireInteraction,
            notification_session_id: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskTrigger {
    Manual,
    Tool {
        session_id: SessionId,
        turn_id: TurnId,
        call_id: CallId,
    },
    Scheduled {
        scheduled_for: DateTime<Utc>,
    },
    Retry {
        previous_task_id: TaskId,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Queued,
    Running,
    NeedsInput,
    Succeeded,
    Failed,
    Cancelled,
    Interrupted,
}

impl TaskState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::NeedsInput => "needs_input",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "queued" => Self::Queued,
            "running" => Self::Running,
            "needs_input" => Self::NeedsInput,
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            "interrupted" => Self::Interrupted,
            _ => return None,
        })
    }

    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Interrupted
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessResult {
    pub exit_code: Option<i32>,
    pub output_object_id: Option<ObjectId>,
    pub output_chars: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentResult {
    pub child_session_id: SessionId,
    pub final_entry_id: Option<EntryId>,
    /// Compact conclusion for notification; the transcript remains in the child session.
    pub conclusion: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskResult {
    Process(ProcessResult),
    Agent(AgentResult),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRun {
    pub task_id: TaskId,
    pub job_id: Option<JobId>,
    pub workspace_id: WorkspaceId,
    /// Snapshot taken when the run is created. Editing/deleting the job never rewrites history.
    pub executor: ExecutorSpec,
    /// Execution policy is also a snapshot; a running task must not gain permissions when its job
    /// is edited.
    pub permission_policy: PermissionPolicy,
    /// Completion target is a run property so deleting the job cannot orphan its notification.
    pub notification_session_id: Option<SessionId>,
    pub trigger: TaskTrigger,
    /// Whether the creating turn should wait for this run before ending.
    /// True for shell commands expected to terminate (compiles, test runs backgrounded with
    /// `background: true` so the turn can keep working); false for servers / watchers, scheduled
    /// jobs and background agents, which are meant to outlive the turn.
    pub turn_scoped: bool,
    pub state: TaskState,
    pub attempt: u32,
    pub scheduled_for: Option<DateTime<Utc>>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub result: Option<TaskResult>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTask {
    pub job_id: Option<JobId>,
    pub workspace_id: WorkspaceId,
    pub executor: ExecutorSpec,
    pub permission_policy: PermissionPolicy,
    pub notification_session_id: Option<SessionId>,
    pub trigger: TaskTrigger,
    pub turn_scoped: bool,
    pub attempt: u32,
    pub scheduled_for: Option<DateTime<Utc>>,
}

impl NewTask {
    pub fn manual(workspace_id: WorkspaceId, executor: ExecutorSpec) -> Self {
        Self {
            job_id: None,
            workspace_id,
            executor,
            permission_policy: PermissionPolicy::RequireInteraction,
            notification_session_id: None,
            trigger: TaskTrigger::Manual,
            turn_scoped: true,
            attempt: 1,
            scheduled_for: None,
        }
    }

    pub fn from_job(job: &JobDefinition, trigger: TaskTrigger) -> Self {
        let scheduled_for = match &trigger {
            TaskTrigger::Scheduled { scheduled_for } => Some(*scheduled_for),
            _ => None,
        };
        Self {
            job_id: Some(job.job_id),
            workspace_id: job.workspace_id,
            executor: job.executor.clone(),
            permission_policy: job.permission_policy,
            notification_session_id: job.notification_session_id,
            trigger,
            turn_scoped: false,
            attempt: 1,
            scheduled_for,
        }
    }
}
