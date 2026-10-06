use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::ProviderConfig;
pub use crate::error::{ApiError, ApiResult};
use crate::ids::{RoundId, SessionId, TranslationId, TurnId, WorkspaceId};
use crate::interaction::{InteractionBody, InteractionDecision};
use crate::llm::Effort;
use crate::stream::{ToolDisplay, ToolStatus, TurnStats, TurnStatus};
use crate::usage::Purpose;
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
    MobileAndroid,
    /// Structured investigation: web search and page fetching over a folder that holds research
    /// output. Distinct from Chat because it keeps the filesystem tools — the outline, the field
    /// schema and the per-item results are files on disk, not conversation.
    Research,
}

pub fn chat_workspace_tools() -> Vec<String> {
    vec!["time".into(), "web_fetch".into(), "web_search".into()]
}

/// A research workspace is Chat's search pair plus the tools that read and write its own output.
pub fn research_workspace_tools() -> Vec<String> {
    vec![
        "time".into(),
        "web_fetch".into(),
        "web_search".into(),
        "read_file".into(),
        "write_file".into(),
        "edit".into(),
        "list_dir".into(),
        "glob".into(),
        "grep".into(),
        "shell".into(),
        "create_agent".into(),
    ]
}

impl WorkspaceKind {
    pub fn of_tools(tools: Option<&[String]>) -> Self {
        match tools {
            None => Self::Coding,
            Some(list) => {
                if matches_preset(list, &chat_workspace_tools()) {
                    Self::Chat
                } else if matches_preset(list, &research_workspace_tools()) {
                    Self::Research
                } else {
                    Self::Custom
                }
            }
        }
    }

    pub fn from_record(tools: Option<&[String]>, persisted: Self) -> Self {
        match persisted {
            Self::MobileAndroid | Self::Chat | Self::Custom | Self::Research => persisted,
            Self::Coding => Self::of_tools(tools),
        }
    }

    pub fn preset_tools(self) -> Option<Vec<String>> {
        match self {
            Self::Coding | Self::Custom | Self::MobileAndroid => None,
            Self::Chat => Some(chat_workspace_tools()),
            Self::Research => Some(research_workspace_tools()),
        }
    }
}

/// Whether a stored allowlist is exactly this preset. Order is the user's, membership is not.
fn matches_preset(tools: &[String], preset: &[String]) -> bool {
    tools.len() == preset.len() && preset.iter().all(|t| tools.contains(t))
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

/// A repository that exists at this workspace but that the engine is not allowed to open. It is
/// reported alongside `is_git: false` rather than folded into it, because the two need different
/// words in the UI: one is a folder to put a repository in, the other is a repository whose
/// ownership has to change, and offering `git init` for the second can only fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum GitRefusal {
    /// The `.git` directory is not owned by the user the engine runs as, so libgit2 refuses to
    /// open it. The repository itself is intact — `git` the CLI accepts one owned by
    /// Administrators when the user is an administrator, and libgit2 has no such exemption — so
    /// this is a limit on what zlogic can do, not a verdict on the user's repository.
    OwnerMismatch,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceGitInfo {
    pub is_git: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<GitRefusal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detached_head: Option<String>,
    /// `branch` names a branch git has not created a ref for yet, because the repository has no
    /// commits. Distinct from both "on a branch" and "detached", and the only way to tell: a
    /// branch with no head sha is a branch that does not exist as far as `git branch` is concerned,
    /// so it is missing from every list and `git switch -c <its own name>` succeeds having done
    /// nothing.
    #[serde(default)]
    pub unborn: bool,
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

/// A byte range of a workspace file.
///
/// The payload is raw bytes, not base64. The one caller that moves these across a process
/// boundary (the desktop's file server) writes them straight to a socket, so a base64 field
/// would put a 1.33x string in memory for the length of the transfer and then be decoded back
/// into the very bytes it just encoded. Range reads are the path large media takes, so that
/// overhead lands exactly where it hurts most.
///
/// The JSON-RPC surface is the exception: it only ever asks for a zero-length read to learn
/// `total`, which is why `data` is `[]` there rather than a megabyte-long number array.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceFileRange {
    pub path: String,
    pub start: u64,
    pub bytes: u64,
    pub total: u64,
    pub data: Vec<u8>,
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

/// Turn the workspace root into a git repository. The offer to do this comes from the surfaces
/// that need one — checkpoints refuse to work without it — so it takes no branch or message: it
/// is `git init` and nothing else, leaving the first commit to the user.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceGitInitReq {
    pub workspace: WorkspaceSelector,
}

/// Add the workspace root to git's `safe.directory` list, which is what makes a repository whose
/// `.git` belongs to another account openable — the same answer git itself gives, and the one
/// libgit2 honours, so one entry serves both. This is a trust decision about a repository, so it
/// is only ever done on an explicit request, never as a side effect of reading the workspace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceGitTrustReq {
    pub workspace: WorkspaceSelector,
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
    /// A nested repository recorded as a gitlink whose recorded commit is still the one it points
    /// at, so the only thing that moved is uncommitted work *inside* it. Git reports this
    /// identically to a gitlink that advanced, but the two need opposite answers: the first cannot
    /// be staged at all (`git add` on a submodule records a new `HEAD`, never its dirty tree), and
    /// the second is an ordinary staged bump. A client that cannot tell them apart offers a button
    /// that exits 0 and changes nothing.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub submodule_dirty: bool,
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

// ── checkpoints ──

/// One restore point, as the timeline renders it. A mirror of the store's own record with the
/// fields a host does not need removed, so the wire shape can stay stable while the store moves.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceCheckpoint {
    pub id: String,
    /// Unix seconds, formatted on the client: a host may want a relative label and a different
    /// timezone from the same timestamp.
    pub at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub session: String,
    /// The session's display title, joined in when the list is read: the snapshot itself only
    /// records the id. Resolved live rather than frozen at capture, so a renamed conversation
    /// renames its own history — the timeline is asking which conversation this was, and that
    /// answer changing with a rename is the correct one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<String>,
    pub trigger: CheckpointTriggerKind,
    /// The tool that was about to run, for a before-tool point.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// One line saying what that call was about to do — the command, the path. This is the text a
    /// user scans for when asking which command went wrong.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The user's own words for a manual point.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// A snapshot missing files over the size or count budget. Kept in the timeline, refused by
    /// restore, and marked in the UI.
    pub partial: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum CheckpointTriggerKind {
    TurnStart,
    TurnEnd,
    BeforeTool,
    Manual,
    BeforeRestore,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceCheckpointListReq {
    pub workspace: WorkspaceSelector,
    /// Continue after this snapshot id — the last row of the previous page. `None` starts at the
    /// newest. Paging by id rather than by offset is what keeps a snapshot taken mid-scroll from
    /// making the next page repeat or skip a row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    /// How many rows this page may hold. `None` means "the card's worth", which is a small
    /// number; the timeline dialog asks for more and keeps asking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// The branch the panel is showing, so the response can say how many snapshots live on other
    /// branches without the client having every row to count them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceCheckpointList {
    pub checkpoints: Vec<WorkspaceCheckpoint>,
    /// Unix seconds, so a client can label ages without a second round trip.
    pub now: i64,
    /// Whether another page follows, and how many snapshots the repository has in total.
    #[serde(default)]
    pub has_more: bool,
    #[serde(default)]
    pub total: usize,
    /// Snapshots not on `branch` — the card's "N on other branches" line, exact even when the
    /// client only holds the first page.
    #[serde(default)]
    pub other_branches: usize,
    /// Whether the user has turned checkpoints on at all. This is the switch the panel shows, so
    /// it is reported next to the list rather than inferred from an empty `checkpoints`.
    pub enabled: bool,
    /// False when the workspace is not a git repository, or the store is off. The timeline shows
    /// why it is empty rather than hiding the section.
    pub available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub retention_days: u32,
    /// The directory snapshots are written under, and the one this workspace's snapshots live in.
    /// Both are reported because the user is being asked to agree to a copy of their code sitting
    /// on disk: where it is has to be answerable without reading the source.
    pub store_dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_dir: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceCheckpointPlanReq {
    pub workspace: WorkspaceSelector,
    pub id: String,
    /// The session the plan is for. It decides which files a restore may delete: anything the
    /// session found already on disk is off limits.
    pub session: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceCheckpointFileDiffReq {
    pub workspace: WorkspaceSelector,
    /// The checkpoint the file belongs to, and the side of the comparison.
    pub checkpoint: String,
    /// A path as the plan's rows spell it: relative to the repository root, `/`-separated.
    pub path: String,
    /// Which side of the checkpoint to measure against. The step is the default, and the only one
    /// an expanded row needs; the workspace view is what the restore confirmation reads.
    #[serde(default)]
    pub compare: CheckpointCompare,
}

/// Which two trees a file's patch sits between. The step is the browsing view — what the agent
/// did to get from the previous point to this one — and the workspace is the restore view, only
/// needed once someone is about to press the button.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum CheckpointCompare {
    #[default]
    Previous,
    Workspace,
}

/// One file of a checkpoint, as a unified patch in the direction the caller asked for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceCheckpointFileDiff {
    pub path: String,
    pub unified: String,
    pub binary: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct CheckpointLineStats {
    pub insertions: u32,
    pub deletions: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum CheckpointFileChange {
    Create,
    Overwrite,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceCheckpointFile {
    pub path: String,
    pub change: CheckpointFileChange,
    /// Absent for a binary file, or a diff too large to count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<CheckpointLineStats>,
}

/// The browsing view of a timeline row: what changed between this point and the one before it.
///
/// Its own call, and separate from the plan on purpose. The answer is a diff between two commits
/// the store already holds, so it costs nothing on disk — while the plan has to walk the working
/// tree, because it answers "what would restoring change *right now*". Putting both in one
/// response would make reading a row cost a workspace walk, which is the price nobody should pay
/// to look at three filenames.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceCheckpointStepReq {
    pub workspace: WorkspaceSelector,
    pub checkpoint: String,
}

/// One step's numbers without the files behind them. The card shows a count per row and refetches
/// whenever the list does, so sending up to 200 file rows per row to render a single integer is
/// the wrong trade; a caller that wants the rows asks for them by opening the row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceCheckpointStepSummary {
    pub checkpoint: String,
    /// The snapshot this one is compared against, absent for the first point in a store — which has
    /// nothing before it, and whose count would otherwise read as "this step changed nothing".
    pub previous: Option<String>,
    pub files: usize,
    pub deletions: usize,
    pub lines: CheckpointLineStats,
}

/// Several steps' summaries in one call, because the card asks for its rows together and a card
/// that costs a round trip per row is a card that renders its numbers one at a time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceCheckpointStepsReq {
    pub workspace: WorkspaceSelector,
    /// Checkpoint ids as the list returns them. Clamped server-side: this is a per-row summary,
    /// and a client that wants every number in a long chain is asking for a diff each.
    pub checkpoints: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceCheckpointSteps {
    pub steps: Vec<WorkspaceCheckpointStepSummary>,
}

/// The browsing half of a plan: what changed between one point in the timeline and the one before
/// it. `writes`/`deletes` above answer the restore question and are only read once someone is
/// about to restore; these answer "what happened here", which is what the row is for.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceCheckpointStep {
    /// The point this one is compared against, absent for the first point in a store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
    /// Capped at what a dialog can render; `files_total` is the real number.
    pub writes: Vec<WorkspaceCheckpointFile>,
    pub files_total: usize,
    pub deletes: Vec<String>,
    pub deletions_total: usize,
    pub drift: CheckpointLineStats,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceCheckpointPlan {
    pub checkpoint: WorkspaceCheckpoint,
    /// False when `HEAD` has moved since the snapshot. A restore is refused unless the user
    /// overrides, and the dialog says which branch it came from.
    pub head_matches: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_head: Option<String>,
    pub writes: Vec<WorkspaceCheckpointFile>,
    /// How many files a restore would write. `writes` is capped for rendering; this is the truth,
    /// and the confirmation counts with it.
    pub writes_total: usize,
    pub deletes: Vec<String>,
    /// How many files a restore would remove, capped the same way.
    pub deletes_total: usize,
    pub unchanged: usize,
    pub drift: CheckpointLineStats,
    /// False when a partial snapshot, or a snapshot the retention sweep has already dropped.
    pub restorable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceCheckpointRestoreReq {
    pub workspace: WorkspaceSelector,
    /// The checkpoint to go back to. Not a plan: the store recomputes what restoring would do from
    /// the snapshot and the tree as they are *now*, so nothing the client said about the past can
    /// authorise a write — and a request carrying two hundred rows to say "this one" was a waste
    /// of both ends.
    pub checkpoint: String,
    /// The session that is doing the restoring. It decides which files a restore may delete:
    /// anything that session found already on disk is off limits.
    pub session: String,
    /// Remove the files the tree gained after the snapshot. Off by default: "undo" and "delete
    /// what I made since" are different requests.
    #[serde(default)]
    pub delete_new: bool,
    /// Restore although `HEAD` has moved.
    #[serde(default)]
    pub cross_head: bool,
    /// One path instead of the whole tree, relative to the repository root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub only: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceCheckpointRestore {
    pub written: usize,
    pub deleted: usize,
    pub failed: Vec<CheckpointRestoreFailure>,
    /// The guard snapshot taken immediately before the write, so the timeline can offer the way
    /// back out of a restore that turned out to be the wrong one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guard: Option<WorkspaceCheckpoint>,
}

/// A restore writes file by file and a locked file does not stop the others, so the result
/// carries what did not happen. Path and reason are separate fields rather than one formatted
/// string: a path may contain the separator a client would otherwise split on, and a count with
/// no names is not something the user can act on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct CheckpointRestoreFailure {
    pub path: String,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceCheckpointCaptureReq {
    pub workspace: WorkspaceSelector,
    /// A free-form label the user typed, kept in the timeline so "before the refactor" can be
    /// found again. Absent for an untitled point.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The session the point belongs to. A manual point made with no turn running still gets one,
    /// so it is grouped with the work it was made for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

/// Emptying one workspace's checkpoint store. There is no `what` to narrow it by: the store is a
/// directory of every snapshot taken of that repository, and keeping some of them while asking to
/// delete it is not a request anybody makes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceCheckpointClearReq {
    pub workspace: WorkspaceSelector,
}

/// What the deletion took, for the message the user reads afterwards. The numbers are the ones
/// only the store knew: the confirmation can say what will go, but not how much disk it was using.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WorkspaceCheckpointCleared {
    pub dropped: usize,
    pub bytes: u64,
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
    /// The sub-agent's profile name (agent tasks only). `title` is the task's own first line,
    /// which says nothing about *who* is running — a runtime list that can only name the prompt
    /// cannot answer "which sub-agent is still going".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
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
    /// Narrow the page to one conversation: only runs whose completion wakes this session
    /// (`task.notification_session_id`). A session's own runtime rail asks this question; the
    /// task centre asks the workspace-wide one and leaves it unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
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

/// Translate one piece of text outside the conversation.
///
/// Same no-turn auxiliary path as [`WorkspaceGitGenerateCommitMessageReq`]: no transcript, no
/// global system prompt, no tools, no visible turn. `to` is the target language **as the model
/// should name it** ("English", "简体中文") — naming it is the entire instruction. The source
/// language is deliberately not a field: it is detected, so a saved "quick translate" is one
/// choice (the target) rather than two.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct TextTranslateReq {
    /// Usage is accounted against a hidden task session in this workspace, as the task draft does.
    pub workspace_id: WorkspaceId,
    pub text: String,
    pub to: String,
    /// The target's language tag, when the caller knows it. Nothing in the prompt uses it: it is
    /// what history and its cache match on, so "English" the name can change wording without
    /// splitting the history of the same language in two.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_code: Option<String>,
    /// Model to translate with; absent means the utility route (the session model, then the light
    /// tier), which is what a short call like this should get by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_ref: Option<String>,
}

/// One streamed completion on the auxiliary model path.
///
/// Same shape as a collected auxiliary call, with the answer arriving as pieces. The caller is
/// something that cannot wait: a spoken reply is synthesised sentence by sentence while the model
/// is still writing, so the pieces are the feature and not a transport detail.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct TextCompleteStreamReq {
    /// Usage is accounted against this session. A live call has no session of its own and
    /// passes the one it interrupted, or nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    /// How the call is routed and billed. `voice` is the one a call should use.
    #[serde(default = "default_purpose")]
    pub purpose: Purpose,
    /// Model to answer with; absent means the role's own chain (the session model, then the
    /// light tier).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_ref: Option<String>,
    /// The only system prompt this call gets. The agent's global one is not in scope here.
    #[serde(default)]
    pub system: String,
    pub input: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
}

fn default_purpose() -> Purpose {
    Purpose::Voice
}

/// One piece of a streamed answer. A failing call sends one `Err` and then stops.
pub type TextDelta = Result<String, ApiError>;

/// One translation, as answered.
///
/// `cached` is the honest half: the same text into the same language was translated before, so the
/// stored answer came back and no model was called. The UI says so rather than presenting a
/// remembered result as a fresh one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct TextTranslateResp {
    pub translation_id: TranslationId,
    pub text: String,
    pub cached: bool,
    pub created_at: DateTime<Utc>,
}

/// A remembered translation, for the history list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct TranslationEntry {
    pub translation_id: TranslationId,
    pub workspace_id: WorkspaceId,
    /// The target as the prompt named it, and the tag the cache matches on.
    pub target: String,
    pub target_code: String,
    pub source_text: String,
    pub result_text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_ref: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct TranslationListReq {
    /// How many of the most recent to return. Absent means [`DEFAULT_TRANSLATION_PAGE`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// How much history a list call returns when the caller does not say.
pub const DEFAULT_TRANSLATION_PAGE: u32 = 50;

/// The most a list call may return, so a hand-written request cannot ask for everything.
pub const MAX_TRANSLATION_PAGE: u32 = 500;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct TranslationDeleteReq {
    pub translation_id: TranslationId,
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
    pub kind: Option<WorkspaceKind>,
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
    /// The session this one was spawned from. `root_session_id` flattens the whole tree onto one
    /// key, which answers "what tree is this in" but not "who is my parent" — and with two
    /// children of the same profile the agent path cannot tell them apart either.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<SessionId>,
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
    /// The session's **last** turn ended unfinished — the user stopped it (`cancelled`), it was cut
    /// off mid-flight (`incomplete: interrupted`) or it errored out (`failed`). Those are exactly
    /// the endings the chat view can continue from, so this is what the sidebar marks. A
    /// `limit_reached` turn is not this: that is a budget the user set, not a half-said turn.
    ///
    /// Derived from the last `turn_end` entry, so the next turn that ends normally clears it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub interrupted: bool,
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
    /// Servers this conversation's background runs are listening on right now.
    ///
    /// Neither declared in the command nor parsed out of the output: the ports are read from the
    /// operating system's listen table and matched back to the run that started it. Recomputed on
    /// every read rather than remembered, because a port that outlives the process that reported
    /// it is worse than no port at all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<SessionService>,
}

/// One listening server, attributed to the background run that started it.
///
/// The attribution is what makes this worth more than a line of scraped output. A `vite` dev
/// server prints its own URL and is still wrong often enough to matter — `--port 0`, a host
/// override, a line in a build log that merely mentions `localhost` — and a wrapper that never
/// binds anything has nothing to print in the first place.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SessionService {
    pub task_id: String,
    /// Which session in the tree started it. A delegated run reports the sub-agent's own id, so
    /// this differs from the conversation the caller asked about.
    pub session_id: SessionId,
    /// `main/researcher` for a delegated run, `None` for the conversation itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_path: Option<String>,
    /// The command line, as the task list renders it.
    pub title: String,
    /// Sorted ascending and free of duplicates: one service bound on both address families
    /// reports one port.
    pub ports: Vec<u16>,
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

/// Fork a new session from a turn. The handle is `turn_seq` (see the rewind request): the fork
/// keeps every turn up to and including it, and the original session is left untouched.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SessionForkReq {
    pub session_id: SessionId,
    pub keep_through_turn: u32,
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
        /// What the turn left in its delivery directory. Timeline-only: the model is never told,
        /// because it wrote the files and has nothing to learn from being told they exist.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        deliverables: Vec<crate::stream::TurnDeliverable>,
        /// Files this turn wrote and then removed, so the client stops offering rows for them.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        gone: Vec<String>,
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

/// A background-task notification that **opened** this turn.
///
/// Only the ones that arrive before anything else in the turn count: a task finishing while the
/// model is already replying is injected mid-turn, and the turn was not woken by it. Carried on
/// the row because a collapsed history line is otherwise an answer with no question in front of
/// it — the turn has no user message, and without this the conversation looks like it continued
/// on its own for no reason.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct TurnWake {
    pub task_id: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wakes: Vec<TurnWake>,
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

/// A byte range of an object in the content-addressed store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ObjectRangeReq {
    pub object: String,
    pub start: u64,
    /// `None` reads to the end; `Some(0)` reads nothing and only reports `total`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<u64>,
}

/// The bytes [`ObjectRangeReq`] asked for. Raw, not base64, for the reason
/// [`WorkspaceFileRange::data`] gives.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ObjectRange {
    pub start: u64,
    pub bytes: u64,
    pub total: u64,
    pub data: Vec<u8>,
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
    /// The per-tool roll-up costs a scan over every tool result in the range — seconds on a
    /// library, and a question only the usage page asks. Off by default, so the panels that poll
    /// the summary for their token counts are not paying for it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub include_tools: bool,
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
    /// `None` when the request did not ask for it, which is different from an empty list: the tool
    /// roll-up is a scan of every tool result in the range, and the caller has to be able to say
    /// "not looked at" rather than "looked at, found nothing".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by_tool: Option<Vec<ToolUsageGroup>>,
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
    pub retention: crate::settings::RetentionConfig,
    pub checkpoints: crate::settings::CheckpointsConfig,
    pub auto_detect_env: bool,
    pub keychain: bool,
    pub env: crate::settings::EnvConfig,
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
    pub retention: Option<crate::settings::RetentionConfig>,
    /// The section is replaced whole, and the engine pushes the result into the live snapshot
    /// store — so this is also how the panel's switch takes effect without a restart.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub checkpoints: Option<crate::settings::CheckpointsConfig>,
    /// The global `env:` block. Written through the same path as every other section so the
    /// global layer needs no second write route; the workspace and session layers are not
    /// expressible in a config file the process owns, so they go through `env_set` instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub env: Option<crate::settings::EnvConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub auto_detect_env: Option<bool>,
    /// `None` leaves the switch alone. Setting it also re-points the vault at the other backend for
    /// the rest of this process, so a change takes effect without a restart.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub keychain: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub llm_roles: Option<std::collections::BTreeMap<String, crate::roles::RoleSettings>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub expected_revision: Option<u64>,
}

// ═══════════════════════════════════ env ═══════════════════════════════════

/// One variable as one layer states it. `rejected` is set instead of the entry being dropped: a
/// name the layer may not contribute has to stay visible, or the user edits a file that has no
/// effect and finds out from the wrong symptom.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct EnvEntry {
    pub name: String,
    pub value: String,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub rejected: Option<String>,
}

/// One layer, as it is written down — never merged with the others.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct EnvLayer {
    pub scope: crate::settings::EnvScope,
    pub entries: Vec<EnvEntry>,
    /// The file or store this layer lives in, so the table can say where an edit will land.
    pub location: String,
    /// `false` when the caller named no context for this layer — the settings page, with no
    /// workspace open, can read the global layer and nothing else.
    pub available: bool,
    /// The layer's own master switch.
    pub enabled: bool,
}

/// What a shell call would actually see, and which layer each name came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct EnvEffective {
    pub name: String,
    pub value: String,
    pub source: crate::settings::EnvSource,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct EnvView {
    pub layers: Vec<EnvLayer>,
    pub effective: Vec<EnvEffective>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(default)]
pub struct EnvGetReq {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub workspace_id: Option<WorkspaceId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub session_id: Option<SessionId>,
}

/// Replaces one layer whole, which is the same contract `config_update` has for every other
/// section: a partial write would have to invent a merge rule, and the merge is already what the
/// three layers mean to each other.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct EnvSetReq {
    pub scope: crate::settings::EnvScope,
    pub variables: std::collections::BTreeMap<String, crate::settings::EnvVarDetail>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub workspace_id: Option<WorkspaceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub session_id: Option<SessionId>,
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
    pub rate_limit: Option<crate::config::ModelRateLimit>,
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
    /// The OS credential store, or the encrypted vault that stands in front of it.
    Keyring,
    /// The local secret file, used when the keychain is turned off. A different label on purpose:
    /// the value there is stored unencrypted, and calling that "Keychain" would misdescribe it.
    File,
    Env,
    Missing,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum ProviderSignInMethod {
    /// The browser callback on the loopback port.
    Browser,
    /// A code the user types at the issuer: the flow that works without a browser.
    Device,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ProviderSignInBeginReq {
    pub provider_id: String,
    #[serde(default = "browser_sign_in")]
    pub method: ProviderSignInMethod,
}

fn browser_sign_in() -> ProviderSignInMethod {
    ProviderSignInMethod::Browser
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ProviderSignInBegin {
    pub flow_id: String,
    pub provider_id: String,
    pub method: ProviderSignInMethod,
    pub authorization_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ProviderSignInStatusReq {
    pub flow_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ProviderSignInCancelReq {
    pub flow_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum ProviderSignInState {
    Pending,
    Succeeded {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plan: Option<String>,
    },
    Failed {
        message: String,
    },
    Expired,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ProviderSignInStatus {
    pub flow_id: String,
    pub provider_id: String,
    pub state: ProviderSignInState,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ProviderModelsReq {
    pub provider_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ProviderModels {
    pub provider_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<DateTime<Utc>>,
    pub models: Vec<crate::config::ModelConfig>,
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
