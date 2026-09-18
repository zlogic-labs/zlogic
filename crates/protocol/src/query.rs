use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::ProviderConfig;
pub use crate::error::{ApiError, ApiResult};
use crate::ids::{RoundId, SessionId, TurnId, WorkspaceId};
use crate::interaction::{InteractionBody, InteractionDecision};
use crate::llm::Effort;
use crate::stream::{ToolDisplay, ToolStatus, TurnStats, TurnStatus};
use crate::usage::{CostTotal, TokenUsage};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Page<T> {
    pub items: Vec<T>,
    pub total: u64,
}

impl<T> Page<T> {
    pub fn whole(items: Vec<T>) -> Self {
        let total = items.len() as u64;
        Self { items, total }
    }
}

// ═══════════════════════════════ workspace ═══════════════════════════════

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum WorkspaceSelector {
    Id {
        workspace_id: WorkspaceId,
    },
    /// Resolve by directory (opening a new workspace). An existing row is reused, not duplicated.
    Path {
        root: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum WorkspaceKind {
    Coding,
    Chat,
    Custom,
}

pub fn chat_workspace_tools() -> Vec<String> {
    vec!["time".into(), "web_fetch".into(), "web_search".into()]
}

impl WorkspaceKind {
    pub fn of_tools(tools: Option<&[String]>) -> Self {
        match tools {
            None => Self::Coding,
            Some(list) => {
                let chat = chat_workspace_tools();
                if list.len() == chat.len() && chat.iter().all(|t| list.contains(t)) {
                    Self::Chat
                } else {
                    Self::Custom
                }
            }
        }
    }

    pub fn preset_tools(self) -> Option<Vec<String>> {
        match self {
            Self::Coding => None,
            Self::Chat => Some(chat_workspace_tools()),
            Self::Custom => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceSummary {
    pub workspace_id: WorkspaceId,
    pub root: String,
    pub name: String,
    pub exists: bool,
    pub pinned: bool,
    pub sort_order: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_opened_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub hidden: bool,
    pub session_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    pub kind: WorkspaceKind,
    #[serde(default, skip_serializing_if = "is_false")]
    pub managed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceGitInfo {
    pub is_git: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detached_head: Option<String>,
    pub staged: u32,
    pub unstaged: u32,
    pub untracked: u32,
}

impl WorkspaceGitInfo {
    pub fn dirty(&self) -> bool {
        self.staged + self.unstaged + self.untracked > 0
    }
}

/// Workspace-relative file API used by the workspace inspector.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceFileListReq {
    pub workspace: WorkspaceSelector,
    #[serde(default)]
    pub path: String,
    /// When true, show ignored (gitignore) entries too. Defaults to false.
    #[serde(default, skip_serializing_if = "is_false")]
    pub include_ignored: bool,
}

/// Search workspace-relative file and directory names for pickers such as `@file`.
/// The engine applies the same ignore policy as the search tools: nested `.gitignore` files,
/// dependency/cache directories, and (when no gitignore exists) common build-output directories.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceFileSearchReq {
    pub workspace: WorkspaceSelector,
    #[serde(default)]
    pub query: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum WorkspaceFileKind {
    File,
    Directory,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceFileEntry {
    pub name: String,
    pub path: String,
    pub kind: WorkspaceFileKind,
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitChangeKind>,
    /// true when the entry is ignored by `.gitignore` (only set when listing with
    /// `include_ignored`; otherwise ignored entries never appear at all).
    #[serde(default, skip_serializing_if = "is_false")]
    pub ignored: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceFileReadReq {
    pub workspace: WorkspaceSelector,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceFileRangeReq {
    pub workspace: WorkspaceSelector,
    pub path: String,
    pub start: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceFileRange {
    pub path: String,
    pub start: u64,
    pub bytes: u64,
    pub total: u64,
    pub data_base64: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceFileText {
    pub path: String,
    pub content: String,
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceFileBase64 {
    pub path: String,
    pub data: String,
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceFileWriteReq {
    pub workspace: WorkspaceSelector,
    pub path: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_modified_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceFileCreateReq {
    pub workspace: WorkspaceSelector,
    pub path: String,
    pub kind: WorkspaceFileKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceFileRenameReq {
    pub workspace: WorkspaceSelector,
    pub path: String,
    pub new_path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceFileDeleteReq {
    pub workspace: WorkspaceSelector,
    pub path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum GitChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Untracked,
    Conflicted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceGitOverviewReq {
    pub workspace: WorkspaceSelector,
    /// Number of first-parent commits to return. The engine caps this value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceGitCommitReq {
    pub workspace: WorkspaceSelector,
    pub message: String,
}

/// Stage/unstage a single workspace-relative path (or a directory prefix) in the Git index.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceGitStageReq {
    pub workspace: WorkspaceSelector,
    /// Workspace-relative path. A directory stages/unstages everything under it.
    pub path: String,
    /// true = add to the index (untracked → added), false = remove from the index (restores the
    /// working-tree state; for untracked files this does nothing).
    pub staged: bool,
}

/// Ask the auxiliary model for a commit message without entering the conversation/turn pipeline.
/// `session_id` selects the same model fallback as the open chat, but no transcript, global
/// system prompt, or tools are sent to the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceGitGenerateCommitMessageReq {
    pub workspace: WorkspaceSelector,
    pub session_id: SessionId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum WorkspaceGitSyncAction {
    Fetch,
    Pull,
    Push,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceGitSyncReq {
    pub workspace: WorkspaceSelector,
    pub action: WorkspaceGitSyncAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum WorkspaceGitBranchAction {
    Checkout,
    Create,
    Rename,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceGitBranchReq {
    pub workspace: WorkspaceSelector,
    pub action: WorkspaceGitBranchAction,
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_point: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub force: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceGitCommitDetailReq {
    pub workspace: WorkspaceSelector,
    pub sha: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct GitCommitFile {
    pub path: String,
    pub change: GitChangeKind,
    pub additions: u32,
    pub deletions: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceGitCommitDetail {
    pub summary: GitCommitSummary,
    pub message: String,
    pub parents: Vec<String>,
    pub files: Vec<GitCommitFile>,
    pub additions: u32,
    pub deletions: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceGitDiffReq {
    pub workspace: WorkspaceSelector,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceGitDiff {
    pub path: String,
    pub unified: String,
    pub binary: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct GitFileChange {
    pub path: String,
    pub change: GitChangeKind,
    pub staged: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct GitWorktree {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub main: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct GitCommitSummary {
    pub sha: String,
    pub subject: String,
    pub author: String,
    pub committed_at: DateTime<Utc>,
    /// True when the commit is reachable from HEAD but not from its upstream.
    pub ahead: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceGitOverview {
    pub info: WorkspaceGitInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub local_branches: Vec<String>,
    pub remote_branches: Vec<String>,
    pub worktrees: Vec<GitWorktree>,
    pub changes: Vec<GitFileChange>,
    pub commits: Vec<GitCommitSummary>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum RuntimeTaskState {
    Queued,
    Running,
    NeedsInput,
    Succeeded,
    Failed,
    Cancelled,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum RuntimeTaskKind {
    Process,
    Agent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct RuntimeTask {
    pub task_id: String,
    pub state: RuntimeTaskState,
    pub kind: RuntimeTaskKind,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct RuntimeTaskListReq {
    pub workspace_id: WorkspaceId,
    #[serde(default)]
    pub stopped_offset: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_limit: Option<u32>,
    #[serde(default)]
    pub only_job_tasks: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct RuntimeTaskPage {
    /// Always complete: active tasks must never disappear behind stopped-task pagination.
    pub active: Vec<RuntimeTask>,
    pub stopped: Vec<RuntimeTask>,
    pub stopped_total: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct RuntimeTaskStopReq {
    pub workspace_id: WorkspaceId,
    pub task_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct RuntimeTaskDeleteReq {
    pub workspace_id: WorkspaceId,
    pub task_id: String,
    #[serde(default)]
    pub remove_orphan_job: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct RuntimeTaskLogReq {
    pub workspace_id: WorkspaceId,
    pub task_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct RuntimeTaskLog {
    pub task_id: String,
    pub state: RuntimeTaskState,
    pub kind: RuntimeTaskKind,
    pub text: String,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_object_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A user-owned durable job. Each execution of a job produces a normal [`RuntimeTask`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum TaskJobSchedule {
    Manual,
    Once {
        at: DateTime<Utc>,
    },
    Cron {
        expression: String,
        timezone: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum TaskJobConcurrencyPolicy {
    Allow,
    Forbid,
    Replace,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum TaskJobExecutor {
    Process {
        program: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
    Agent {
        profile: String,
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_ref: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct TaskJob {
    pub job_id: String,
    pub workspace_id: WorkspaceId,
    pub title: String,
    pub executor: TaskJobExecutor,
    pub schedule: TaskJobSchedule,
    pub enabled: bool,
    pub concurrency_policy: TaskJobConcurrencyPolicy,
    /// Scheduler-owned context root. It is separate from ordinary chat sessions.
    pub task_session_id: SessionId,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct TaskJobListReq {
    pub workspace_id: WorkspaceId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct TaskJobRunsReq {
    pub job_id: String,
    #[serde(default)]
    pub stopped_offset: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct TaskJobCreateReq {
    pub workspace_id: WorkspaceId,
    /// Present when the request came from a generated draft; otherwise the engine creates it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_session_id: Option<SessionId>,
    pub title: String,
    pub executor: TaskJobExecutor,
    pub schedule: TaskJobSchedule,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub concurrency_policy: TaskJobConcurrencyPolicy,
}

/// Natural-language input for generating a reviewable job draft.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct TaskJobDraftReq {
    pub workspace_id: WorkspaceId,
    pub instruction: String,
    pub timezone: String,
}

/// A model-generated proposal. It is not persisted or scheduled until the caller submits a
/// separate [`TaskJobCreateReq`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct TaskJobDraft {
    /// The isolated session used for draft-model usage and later Job execution.
    pub task_session_id: SessionId,
    pub title: String,
    pub executor: TaskJobExecutor,
    pub schedule: TaskJobSchedule,
    pub concurrency_policy: TaskJobConcurrencyPolicy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct TaskJobSetEnabledReq {
    pub job_id: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct TaskJobRunReq {
    pub job_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct TaskJobDeleteReq {
    pub job_id: String,
}

const fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceUpdateReq {
    pub workspace_id: WorkspaceId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort_order: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<WorkspaceToolsUpdate>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum WorkspaceToolsUpdate {
    All,
    Only { tools: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub source: String,
    pub available: bool,
}

// ═══════════════════════════════ session ═════════════════════════════════

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum TitleSource {
    Draft,
    Model,
    User,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SessionSummary {
    pub session_id: SessionId,
    pub workspace_id: WorkspaceId,
    pub agent_paths: Vec<String>,
    pub root_session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_source: Option<TitleSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_message_at: Option<DateTime<Utc>>,
    pub turn_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_turn_id: Option<TurnId>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub awaiting_input: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<DateTime<Utc>>,
}

impl SessionSummary {
    pub fn is_root(&self) -> bool {
        self.agent_paths.len() <= 1
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SessionListReq {
    pub workspace: WorkspaceSelector,
    #[serde(default, skip_serializing_if = "is_false")]
    pub include_sub_agents: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub include_archived: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SessionOpenReq {
    pub workspace: WorkspaceSelector,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SessionOpened {
    pub session: SessionSummary,
    pub workspace: WorkspaceSummary,
    pub exec_cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_turn_id: Option<TurnId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<TurnState>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_submissions: Vec<PendingSubmission>,
    pub model: ModelSelection,
    pub edit_protection: EditProtection,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ModelSelection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_ref: Option<String>,
    pub source: ModelSelectionSource,
    pub models_configured: bool,
    pub has_usable_credential: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum ModelSelectionSource {
    Session,
    Default,
    FirstUsable,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum EditProtection {
    Active,
    InitFailed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SessionRenameReq {
    pub session_id: SessionId,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SessionSearchReq {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceSelector>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    pub query: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SessionSearchHit {
    pub session: SessionSummary,
    pub turn_seq: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round_seq: Option<u32>,
    pub kind: TranscriptKind,
    pub at: DateTime<Utc>,
    pub snippet: String,
}

// ═════════════════════════════ transcript ════════════════════════════════

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum TranscriptKind {
    User,
    Reasoning,
    Text,
    ToolCall,
    ToolResult,
    InteractionRequest,
    InteractionResponse,
    Notice,
    Compaction,
    Steering,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct TranscriptEntry {
    pub entry_id: String,
    pub turn_id: TurnId,
    pub turn_seq: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round_id: Option<RoundId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round_seq: Option<u32>,
    pub at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_final: bool,
    pub body: TranscriptBody,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum TranscriptBody {
    User {
        parts: Vec<TranscriptPart>,
    },
    Reasoning {
        text: String,
        #[serde(default, skip_serializing_if = "is_false")]
        truncated: bool,
    },
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "is_false")]
        truncated: bool,
    },
    ToolCall {
        calls: Vec<TranscriptToolCall>,
    },
    ToolResult {
        call_id: String,
        name: String,
        status: ToolStatus,
        summary: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        display: Vec<ToolDisplay>,
        #[serde(default, skip_serializing_if = "is_zero")]
        duration_ms: u64,
    },
    InteractionRequest {
        interaction_id: String,
        body: InteractionBody,
    },
    InteractionResponse {
        interaction_id: String,
        decision: InteractionDecision,
    },
    Notice {
        level: crate::stream::NoticeLevel,
        code: String,
        message: crate::error::LocalizedMessage,
    },
    TurnEnd {
        status: crate::stream::TurnStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// A background task reached a terminal state and woke this conversation.
    TaskUpdate {
        task_id: String,
        state: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preview: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        child_session_id: Option<SessionId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        job_title: Option<String>,
    },
    Compaction {
        replaces: (u32, u32),
        summary: String,
        reason: crate::stream::CompactionReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary_tokens: Option<u64>,
    },
    Steering {
        parts: Vec<TranscriptPart>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum TranscriptPart {
    Text {
        text: String,
    },
    File {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bytes: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preview: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        degraded: bool,
    },
    /// A remote-client attachment backed by the content-addressed object store.
    Attachment {
        object_id: String,
        display_name: String,
        mime: String,
        bytes: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preview: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        degraded: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct TranscriptToolCall {
    pub call_id: String,
    pub name: String,
    pub args: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct TranscriptReq {
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_turn_seq: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct TurnsReq {
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_turn_seq: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum TurnAnswerKind {
    Text,
    Reasoning,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct TurnAnswer {
    pub kind: TurnAnswerKind,
    pub text: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct TurnWidget {
    pub object_id: String,
    pub title: String,
    pub height: u32,
    pub libraries: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct TurnCompaction {
    pub replaces: (u32, u32),
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct TurnItem {
    pub turn_seq: u32,
    pub turn_id: TurnId,
    pub at: DateTime<Utc>,
    pub user: Vec<TranscriptPart>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<TurnAnswer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<TurnStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub detail: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<TurnCompaction>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub widgets: Vec<TurnWidget>,
}

/// - `user` → `kind='user'`
/// - `event` → `kind='event'`
/// - `tool` → `kind IN ('tool_call','tool_result')`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum EntryRole {
    User,
    Assistant,
    Final,
    Event,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct EntriesReq {
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_seq: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<EntryRole>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ObjectReadReq {
    pub object: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u64>,
}

/// Complete bytes for a small MIME-aware UI preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ObjectDataReq {
    pub object: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ObjectData {
    pub base64: String,
    pub total_bytes: u64,
}

/// Result of uploading raw bytes into the content-addressed object store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct UploadedObject {
    pub object_id: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ObjectText {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    pub start_line: u64,
    pub end_line: u64,
    pub total_lines: u64,
    pub total_bytes: u64,
}

// ═══════════════════════════════ turn ════════════════════════════════════

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct TurnState {
    pub turn_id: TurnId,
    pub session_id: SessionId,
    pub phase: TurnPhase,
    pub started_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<TurnStatus>,
    pub stats: TurnStats,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_interaction: Option<PendingInteraction>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum TurnPhase {
    Running,
    AwaitingInput,
    Ended,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct PendingInteraction {
    pub interaction_id: String,
    pub body: InteractionBody,
    pub asked_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum PendingOrigin {
    User,
    Task,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct PendingSubmission {
    pub submission_id: String,
    pub parts: Vec<TranscriptPart>,
    pub origin: PendingOrigin,
    /// The `provider:model` snapshot captured when this input was submitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub model_ref: Option<String>,
    pub delivery: crate::input::Delivery,
    pub queued_at: DateTime<Utc>,
}

// ═══════════════════════════════ usage ═══════════════════════════════════

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct UsageGroup {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub calls: u32,
    pub sessions: u32,
    pub turns: u32,
    pub aux_calls: u32,
    pub estimated_calls: u32,
    pub tokens: TokenUsage,
    pub max_input_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<CostTotal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg_first_token_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg_response_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg_turn_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum UsageSessionKind {
    Chat,
    Task,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct UsageSummaryReq {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceSelector>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub self_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_kind: Option<UsageSessionKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<DateTime<Utc>>,
    #[serde(default)]
    pub utc_offset_minutes: i32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct UsageSummary {
    pub tokens: TokenUsage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_context_tokens: Option<u64>,
    pub max_input_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<CostTotal>,
    pub calls: u32,
    pub main_calls: u32,
    pub aux_calls: u32,
    pub sessions: u32,
    pub turns: u32,
    pub estimated_calls: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aux_cost: Option<CostTotal>,
    pub total_tool_calls: u32,
    pub total_tool_duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg_first_token_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg_response_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg_turn_ms: Option<u64>,
    pub by_model: Vec<UsageGroup>,
    pub by_provider: Vec<UsageGroup>,
    pub by_day: Vec<UsageGroup>,
    pub by_session: Vec<UsageGroup>,
    pub by_workspace: Vec<UsageGroup>,
    pub by_aux_purpose: Vec<UsageGroup>,
    pub by_cost_source: Vec<UsageGroup>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_turn: Option<UsageGroup>,
    pub by_tool: Vec<ToolUsageGroup>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ToolUsageGroup {
    pub name: String,
    pub stats: crate::stream::ToolStats,
    pub duration_ms: u64,
}

// ═══════════════════════════════ config ═════════════════════════════════

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ConfigView {
    pub providers: Vec<ProviderConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    pub settings: SettingsView,
    pub llm_roles: std::collections::BTreeMap<String, crate::roles::RoleSettings>,
    pub global_path: String,
    pub models_path: String,
    pub config_dir: String,
    pub log_dir: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SettingsView {
    pub session: crate::settings::SessionConfig,
    pub context: crate::settings::ContextConfig,
    pub tools: crate::settings::ToolsConfig,
    pub log: crate::settings::LogConfig,
    pub worktree: crate::settings::WorktreeConfig,
    pub cost: crate::settings::CostConfig,
    pub limits: crate::settings::LimitsConfig,
    pub network: crate::settings::NetworkSettings,
    pub auto_detect_env: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(default)]
pub struct ConfigUpdateReq {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub default_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub session: Option<crate::settings::SessionConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub context: Option<crate::settings::ContextConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub tools: Option<crate::settings::ToolsConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub log: Option<crate::settings::LogConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub worktree: Option<crate::settings::WorktreeConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub cost: Option<crate::settings::CostConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub limits: Option<crate::settings::LimitsConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub network: Option<crate::settings::NetworkSettings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub auto_detect_env: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub llm_roles: Option<std::collections::BTreeMap<String, crate::roles::RoleSettings>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub expected_revision: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct OpenAiCompatibleProviderReq {
    pub provider_id: String,
    pub base_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub model_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub rename_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub wire_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub max_output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub sdk: Option<crate::config::Sdk>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub generic: Option<crate::config::GenericOpenAiDialect>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub wiring: Option<crate::config::Wiring>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub network: Option<crate::config::NetworkConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub model_network: Option<crate::config::NetworkConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub provider_default_params: Option<std::collections::BTreeMap<String, serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub model_default_params: Option<std::collections::BTreeMap<String, serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub no_think_params: Option<std::collections::BTreeMap<String, serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub vision: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub thinking: Option<crate::config::ThinkingCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub pricing: Option<crate::config::Pricing>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub tier: Option<crate::config::Tier>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub create_scope: Option<ConfigCreateScope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub expected_revision: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum ConfigCreateScope {
    Provider,
    Model,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ConfigRemoveProviderReq {
    pub provider_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub model_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub expected_revision: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct CredentialState {
    pub provider_id: String,
    pub present: bool,
    pub source: CredentialSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ProviderCatalog {
    pub providers: Vec<ProviderConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prices: Option<PriceSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog: Option<CatalogSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct CatalogSnapshot {
    pub source: String,
    pub version: String,
    pub fetched_at: DateTime<Utc>,
    pub models: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct CatalogCheck {
    pub source: String,
    pub current_version: String,
    pub remote_version: String,
    pub update_available: bool,
    pub models: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct PriceSnapshot {
    pub source: String,
    pub fetched_at: DateTime<Utc>,
    pub models: u32,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct CredentialSetReq {
    pub provider_id: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct CredentialDeleteReq {
    pub provider_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct CredentialVerifyReq {
    pub provider_id: String,
    pub model_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct CredentialVerifyResult {
    pub provider_id: String,
    pub model_id: String,
    pub reply: String,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum CredentialSource {
    Keyring,
    Env,
    Missing,
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_workspace_kind_is_derived_from_the_tool_list() {
        assert_eq!(WorkspaceKind::of_tools(None), WorkspaceKind::Coding);
        assert_eq!(
            WorkspaceKind::of_tools(Some(&chat_workspace_tools())),
            WorkspaceKind::Chat
        );
        let mut reversed = chat_workspace_tools();
        reversed.reverse();
        assert_eq!(
            WorkspaceKind::of_tools(Some(&reversed)),
            WorkspaceKind::Chat,
            "order does not matter"
        );
        assert_eq!(
            WorkspaceKind::of_tools(Some(&["web_fetch".to_string()])),
            WorkspaceKind::Custom
        );
        assert_eq!(WorkspaceKind::of_tools(Some(&[])), WorkspaceKind::Custom);
    }

    #[test]
    fn presets_round_trip_through_derivation() {
        for kind in [WorkspaceKind::Coding, WorkspaceKind::Chat] {
            let tools = kind.preset_tools();
            assert_eq!(WorkspaceKind::of_tools(tools.as_deref()), kind);
        }
    }

    #[test]
    fn api_error_carries_its_code_on_the_wire() {
        let v = serde_json::to_value(ApiError::not_wired("session_list")).unwrap();
        assert_eq!(v["code"], "engine_not_wired");
        assert_eq!(v["category"], "not_wired");
        assert_eq!(v["details"]["op"], "session_list");
    }

    #[test]
    fn not_found_keeps_both_the_code_and_the_missing_kind() {
        let v = serde_json::to_value(ApiError::not_found("session", "abc")).unwrap();
        assert_eq!(v["code"], "session_not_found");
        assert_eq!(v["category"], "not_found");
        assert_eq!(v["details"]["kind"], "session");
        assert_eq!(v["details"]["id"], "abc");
        assert_eq!(
            serde_json::from_value::<ApiError>(v).unwrap(),
            ApiError::not_found("session", "abc")
        );
    }

    #[test]
    fn not_wired_is_distinguishable_from_internal() {
        let a = serde_json::to_value(ApiError::not_wired("x")).unwrap();
        let b = serde_json::to_value(ApiError::internal("x")).unwrap();
        assert_ne!(a["code"], b["code"]);
    }

    #[test]
    fn page_total_survives_a_short_page() {
        let page = Page {
            items: vec![1, 2, 3],
            total: 137,
        };
        let v = serde_json::to_value(&page).unwrap();
        assert_eq!(
            v["total"], 137,
            "the total of 137 cannot be inferred from the length of this page"
        );
    }

    #[test]
    fn timeline_entries_expose_turn_seq_not_entry_seq() {
        let e = TranscriptEntry {
            entry_id: "e1".into(),
            turn_id: TurnId::new(),
            turn_seq: 4,
            round_id: Some(RoundId::new()),
            round_seq: Some(7),
            at: Utc::now(),
            agent: None,
            is_final: false,
            body: TranscriptBody::Text {
                text: "hi".into(),
                truncated: false,
            },
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["turn_seq"], 4);
        assert_eq!(
            v["round_id"],
            serde_json::to_value(e.round_id).unwrap(),
            "the round id must survive the transcript wire so the UI can group by model round"
        );
        assert_eq!(
            v["round_seq"],
            serde_json::to_value(e.round_seq).unwrap(),
            "the round seq must survive the transcript wire so UI replay carries the engine's numbering"
        );
        assert!(v.get("seq").is_none());
        assert_eq!(v["body"]["type"], "text");
    }

    #[test]
    fn a_file_part_can_travel_without_a_preview() {
        let p = TranscriptPart::File {
            path: "/tmp/a.csv".into(),
            display_name: None,
            mime: None,
            bytes: None,
            preview: None,
            degraded: false,
        };
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["type"], "file");
        assert!(
            v.get("preview").is_none(),
            "an absent field is omitted entirely rather than sent as null"
        );
        assert_eq!(serde_json::from_value::<TranscriptPart>(v).unwrap(), p);
    }
}
