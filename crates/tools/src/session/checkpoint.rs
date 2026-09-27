//! The capability of snapshotting a working tree, as the agent loop needs it.
//!
//! Same boundary as [`super::worktree::WorktreeHost`]: the loop knows *when* a moment is worth
//! keeping — before a turn, before a tool that changes something, after a turn — and none of *how*.
//! The store needs libgit2, a place to put the snapshots and the repository to read, none of which
//! this crate has.
//!
//! One rule runs through the whole trait: **a checkpoint never fails a turn.** The store reports
//! what happened in a [`CheckpointReceipt`] instead of an error, because the alternative is a
//! full disk or a repository that cannot be opened turning every tool call into a dead one. A
//! receipt with no id and a note is how that is said out loud.

use std::path::PathBuf;

use async_trait::async_trait;

/// What asked for the snapshot. It is part of the record, so a snapshot can be read back as
/// "before the `edit` call of turn 12" without a second database to keep in step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointTrigger {
    TurnStart,
    TurnEnd,
    BeforeTool,
}

#[derive(Debug, Clone)]
pub struct CheckpointRequest {
    /// The working tree to copy. The session's `exec_cwd`, not the workspace root, so a session
    /// that moved into a worktree snapshots the checkout it is actually working in.
    pub workspace: PathBuf,
    pub session_id: String,
    pub turn_id: Option<String>,
    pub trigger: CheckpointTrigger,
    /// The tool about to run, for [`CheckpointTrigger::BeforeTool`].
    pub tool: Option<String>,
    /// One line saying what that call was about to do — the command, the path. The tool name
    /// alone cannot answer "which shell command wiped my work", which is the question a user comes
    /// back to a timeline with, so the loop passes the call's gist along.
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointOutcome {
    /// The working tree is stored and the id is the restore point.
    Captured,
    /// Nothing was stored and nothing went wrong: there is no store here, or it is switched off.
    Skipped,
    /// The store tried and could not: a full disk, a repository that cannot be opened.
    Failed,
}

impl Default for CheckpointOutcome {
    fn default() -> Self {
        CheckpointOutcome::Skipped
    }
}

#[derive(Debug, Clone, Default)]
pub struct CheckpointReceipt {
    /// The snapshot's id. Empty when nothing was captured.
    pub id: String,
    /// Unix seconds, or 0 when nothing was captured.
    pub at: i64,
    pub outcome: CheckpointOutcome,
    /// Why nothing was captured, or what went wrong. `None` on a plain success.
    pub note: Option<String>,
}

impl CheckpointReceipt {
    pub fn captured(id: String, at: i64) -> Self {
        Self {
            id,
            at,
            outcome: CheckpointOutcome::Captured,
            note: None,
        }
    }

    pub fn skipped(reason: impl Into<String>) -> Self {
        Self {
            id: String::new(),
            at: 0,
            outcome: CheckpointOutcome::Skipped,
            note: Some(reason.into()),
        }
    }

    pub fn failed(reason: impl Into<String>) -> Self {
        Self {
            id: String::new(),
            at: 0,
            outcome: CheckpointOutcome::Failed,
            note: Some(reason.into()),
        }
    }
}

#[async_trait]
pub trait CheckpointHost: Send + Sync {
    /// Copies the working tree. Best-effort: the returned error is a message for the transcript,
    /// not a reason to refuse the tool call that follows.
    async fn capture(&self, request: CheckpointRequest) -> Result<CheckpointReceipt, String>;
}
