//! Checkpoints: a point-in-time copy of a working tree, kept outside the user's repository.
//!
//! A snapshot is the working directory as a git tree, stored in a bare repository under zlogic's
//! own data directory. Nothing is written to the user's repository — no index, no `HEAD`, no refs,
//! no config, no hooks — and no `git` executable is needed, because the store is libgit2. What the
//! user gets is the one thing a tree is good at: a moment that can be put back exactly.
//!
//! What a snapshot is **not**: a patch, a three-way merge, or a backup of the user's history. It
//! holds the working tree, minus what git ignores, and the staging area is not part of it. A
//! restore is a checkout, not a reconciliation.

pub mod maintenance;
pub mod restore;
pub mod retention;
pub mod store;

use std::path::PathBuf;

use async_trait::async_trait;
use zlogic_tools::{CheckpointHost, CheckpointReceipt, CheckpointRequest};

pub use restore::{
    Change, Compare, FileDiff, LineStats, PathChange, RestoreOptions, RestoreOutcome, RestorePlan,
    StepDiff,
};
pub use retention::SweepReport;
pub use store::{
    Capture, CheckpointError, Checkpoints, ClearReport, Config, Page, Snapshot, Trigger, now,
    HEAD_REF, REPO_DIR,
};

/// The agent loop's half of the contract. Every trigger is best-effort by design: a checkpoint
/// that cannot be taken is a warning in the transcript, never a failed tool call and never a failed
/// turn. A workspace that is not a git repository is not an error either — it simply has no
/// checkpoints, and the first capture says so once.
#[async_trait]
impl CheckpointHost for Checkpoints {
    async fn capture(&self, request: CheckpointRequest) -> Result<CheckpointReceipt, String> {
        if !self.enabled() {
            return Ok(CheckpointReceipt::skipped("checkpoints are off"));
        }
        let workspace: PathBuf = request.workspace;
        let capture = Capture {
            session: request.session_id,
            turn: request.turn_id,
            trigger: request.trigger.into(),
            tool: request.tool,
            detail: request.detail,
            label: None,
        };
        match Checkpoints::capture(self, workspace, capture).await {
            Ok(snapshot) => Ok(CheckpointReceipt::captured(snapshot.id, snapshot.at)),
            Err(CheckpointError::NotARepository(path)) => Ok(CheckpointReceipt::skipped(format!(
                "{path} is not a git repository, so it has no checkpoints"
            ))),
            // Skipped rather than failed for the same reason: a repository zlogic may not open is
            // a standing condition, not something that went wrong on this turn.
            Err(CheckpointError::NotOwned(path)) => Ok(CheckpointReceipt::skipped(format!(
                "{path} is a git repository zlogic is not allowed to open: its .git is not owned \
                 by the user zlogic runs as"
            ))),
            Err(error) => Ok(CheckpointReceipt::failed(format!(
                "checkpoint could not be taken: {error}"
            ))),
        }
    }
}
