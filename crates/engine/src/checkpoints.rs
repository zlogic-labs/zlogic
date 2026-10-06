//! The checkpoint timeline as a service, translating the store's records into the wire shapes.
//!
//! Two rules shape this. A workspace that is not a git repository, or a store that is switched
//! off, is a normal answer rather than an error — the UI says "unavailable, here is why" instead
//! of showing nothing. And a restore is re-derived from the snapshot rather than trusted from
//! the request: the client sends the plan it drew its confirmation from, and the server recomputes
//! it, because a plan is a description of the past and `HEAD` is a fact about the present.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use zlogic_core::SharedStore;
use zlogic_checkpoints::{
    Capture, CheckpointError, Checkpoints, RestoreOptions, Snapshot, Trigger,
};
use zlogic_protocol::query::{
    ApiError, ApiResult, CheckpointCompare, CheckpointFileChange, CheckpointLineStats,
    CheckpointRestoreFailure, CheckpointTriggerKind, WorkspaceCheckpoint,
    WorkspaceCheckpointCaptureReq, WorkspaceCheckpointClearReq, WorkspaceCheckpointCleared,
    WorkspaceCheckpointFile, WorkspaceCheckpointFileDiff, WorkspaceCheckpointFileDiffReq,
    WorkspaceCheckpointList, WorkspaceCheckpointListReq, WorkspaceCheckpointPlan,
    WorkspaceCheckpointPlanReq, WorkspaceCheckpointRestore, WorkspaceCheckpointRestoreReq,
    WorkspaceCheckpointStep, WorkspaceCheckpointStepReq, WorkspaceCheckpointStepSummary,
    WorkspaceCheckpointSteps, WorkspaceCheckpointStepsReq, WorkspaceSelector,
};

use crate::lock::SessionLocks;
use crate::service::WorkspaceCheckpointsService;
use crate::workspaces::Workspaces;

/// What a client that says nothing gets: enough rows for the card, and for a short timeline.
const DEFAULT_PAGE: u32 = 50;
const MAX_PAGE: u32 = 500;

/// How many file rows a plan puts on the wire. The plan itself is whole — a restore writes every
/// file it names — so the cut happens here, where the list becomes something a dialog renders.
/// The totals beside the rows are what the confirmation and the header count.
const ROW_LIMIT: usize = 200;

/// How many step summaries one call may ask for. The card asks for three; the ceiling is here so
/// "give me the numbers for the whole chain" is not a single request that diffs hundreds of pairs.
const MAX_SUMMARY_STEPS: usize = 50;

pub struct CheckpointTimeline {
    store: Arc<Checkpoints>,
    workspaces: Arc<Workspaces>,
    locks: Arc<SessionLocks>,
    /// The session store, for the one thing a snapshot cannot answer about itself: which
    /// conversation it belongs to. A record carries the session id and nothing else, so the
    /// timeline's title column is joined in on read.
    sessions: SharedStore,
}

impl CheckpointTimeline {
    pub fn new(
        store: Arc<Checkpoints>,
        workspaces: Arc<Workspaces>,
        locks: Arc<SessionLocks>,
        sessions: SharedStore,
    ) -> Self {
        Self {
            store,
            workspaces,
            locks,
            sessions,
        }
    }

    fn root(&self, selector: &WorkspaceSelector) -> ApiResult<std::path::PathBuf> {
        self.workspaces.file_root(selector)
    }

    /// Titles for a page of snapshots, one query per distinct session rather than per row: a page
    /// is a few hundred snapshots of what is usually one or two conversations, and the session
    /// table is read on every timeline page otherwise. A session that no longer parses or no
    /// longer exists contributes no title, which is what a row with no conversation to name
    /// should say.
    fn session_titles<'a>(&self, items: &'a [Snapshot]) -> HashMap<&'a str, Option<String>> {
        let ids: HashSet<&str> = items
            .iter()
            .map(|snapshot| snapshot.session.as_str())
            .filter(|session| !session.is_empty())
            .collect();
        ids.into_iter().map(|id| (id, self.title_of(id))).collect()
    }

    fn export_page(&self, items: &[Snapshot]) -> Vec<WorkspaceCheckpoint> {
        let titles = self.session_titles(items);
        items
            .iter()
            .map(|snapshot| {
                let title = titles.get(snapshot.session.as_str()).cloned().flatten();
                export_snapshot(snapshot, title)
            })
            .collect()
    }

    fn export_one(&self, snapshot: &Snapshot) -> WorkspaceCheckpoint {
        export_snapshot(snapshot, self.title_of(&snapshot.session))
    }

    fn title_of(&self, session: &str) -> Option<String> {
        session
            .parse::<zlogic_protocol::SessionId>()
            .ok()
            .and_then(|id| self.sessions.with(|db| db.sessions().find(id)).ok().flatten())
            .and_then(|session| session.title)
    }

    /// Whether the session the client named is mid-turn, which is the one thing that makes a
    /// restore unsafe rather than merely unwelcome: the agent is holding a view of the tree that
    /// a whole-tree restore invalidates, and its next write lands on top of the rolled-back state
    /// without ever having seen the rollback. A session string that does not parse is not treated
    /// as busy — the gate protects the common case and must not become a way to make restore fail
    /// for a client that names no session.
    fn session_is_busy(&self, session: &str) -> bool {
        session
            .parse::<zlogic_protocol::SessionId>()
            .ok()
            .and_then(|id| self.locks.live_turn(id).ok().flatten())
            .is_some()
    }
}

#[async_trait]
impl WorkspaceCheckpointsService for CheckpointTimeline {
    async fn checkpoint_list(
        &self,
        req: WorkspaceCheckpointListReq,
    ) -> ApiResult<WorkspaceCheckpointList> {
        let config = self.store.config();
        let store_dir = self.store.root().display().to_string();
        // Answered whether or not the feature is on: "where would my code be kept" is the
        // question the switch asks, so it cannot be a question only the enabled state answers.
        let repo_dir = self
            .root(&req.workspace)
            .ok()
            .and_then(|root| self.store.repository_dir(&root))
            .map(|path| path.display().to_string());
        if !config.enabled {
            // Off is not an error and not a "no snapshots yet": the panel shows the switch and
            // what the user would be agreeing to, which is why nothing else is filled in here.
            return Ok(WorkspaceCheckpointList {
                checkpoints: Vec::new(),
                now: zlogic_checkpoints::now(),
                has_more: false,
                total: 0,
                other_branches: 0,
                enabled: false,
                available: false,
                reason: None,
                retention_days: config.retention_days,
                store_dir,
                repo_dir,
            });
        }
        let root = self.root(&req.workspace)?;
        let page = self
            .store
            .list_page(
                root,
                req.before.clone(),
                page_limit(req.limit),
                req.branch.clone(),
            )
            .await;
        let mut list = WorkspaceCheckpointList {
            checkpoints: Vec::new(),
            now: zlogic_checkpoints::now(),
            has_more: false,
            total: 0,
            other_branches: 0,
            enabled: true,
            available: true,
            reason: None,
            retention_days: config.retention_days,
            store_dir,
            repo_dir,
        };
        match page {
            Ok(page) => {
                list.has_more = page.has_more;
                list.total = page.total;
                list.other_branches = page.other_branches;
                list.checkpoints = self.export_page(&page.items);
                Ok(list)
            }
            // A workspace that never was a repository is not a failure; the list is empty and the
            // reason is what the section shows in place of a timeline.
            Err(CheckpointError::NotARepository(path)) => {
                list.available = false;
                list.reason = Some(format!("{path} is not a git repository"));
                Ok(list)
            }
            // Same shape, different words: this one has a repository, and the remedy is not
            // `git init`, so the section must not read as though there were nothing to open.
            Err(CheckpointError::NotOwned(path)) => {
                list.available = false;
                list.reason = Some(format!(
                    "{path} is a git repository zlogic is not allowed to open: its .git is not \
                     owned by the user zlogic runs as"
                ));
                Ok(list)
            }
            Err(error) => Err(translate(error)),
        }
    }

    async fn checkpoint_plan(
        &self,
        req: WorkspaceCheckpointPlanReq,
    ) -> ApiResult<WorkspaceCheckpointPlan> {
        let root = self.root(&req.workspace)?;
        let plan = self
            .store
            .plan_restore(root, req.id.clone(), req.session)
            .await
            .map_err(translate)?;
        let title = self.title_of(&plan.snapshot.session);
        Ok(export_plan(plan, title))
    }

    async fn checkpoint_step(
        &self,
        req: WorkspaceCheckpointStepReq,
    ) -> ApiResult<WorkspaceCheckpointStep> {
        let root = self.root(&req.workspace)?;
        let step = self
            .store
            .step(root, req.checkpoint)
            .await
            .map_err(translate)?;
        Ok(export_step(step))
    }

    async fn checkpoint_steps(
        &self,
        req: WorkspaceCheckpointStepsReq,
    ) -> ApiResult<WorkspaceCheckpointSteps> {
        let root = self.root(&req.workspace)?;
        let mut steps = Vec::new();
        for id in req.checkpoints.iter().take(MAX_SUMMARY_STEPS) {
            let step = self.store.step(root.clone(), id.clone()).await;
            // A row whose summary cannot be read is a row that shows no number, not a request that
            // failed: the other two rows on the card are still worth rendering, and a retention
            // sweep that dropped this snapshot between the list and this call is not an error the
            // user can act on.
            match step {
                Ok(step) => steps.push(WorkspaceCheckpointStepSummary {
                    checkpoint: id.clone(),
                    previous: step.previous,
                    files: step.files_total,
                    deletions: step.deletions_total,
                    lines: export_stats(step.drift),
                }),
                Err(error) => tracing::debug!(
                    target: "zlogic::checkpoints",
                    checkpoint = %id,
                    %error,
                    "no step summary for this row"
                ),
            }
        }
        Ok(WorkspaceCheckpointSteps { steps })
    }

    async fn checkpoint_diff(
        &self,
        req: WorkspaceCheckpointFileDiffReq,
    ) -> ApiResult<WorkspaceCheckpointFileDiff> {
        let root = self.root(&req.workspace)?;
        let compare = match req.compare {
            CheckpointCompare::Previous => zlogic_checkpoints::Compare::Previous,
            CheckpointCompare::Workspace => zlogic_checkpoints::Compare::Workspace,
        };
        let diff = self
            .store
            .file_diff(root, req.checkpoint, req.path, compare)
            .await
            .map_err(translate)?;
        Ok(WorkspaceCheckpointFileDiff {
            path: diff.path,
            unified: diff.unified,
            binary: diff.binary,
        })
    }

    async fn checkpoint_capture(
        &self,
        req: WorkspaceCheckpointCaptureReq,
    ) -> ApiResult<WorkspaceCheckpoint> {
        // The switch is the user's, and the button that presses it is not a way around it.
        if !self.store.enabled() {
            return Err(ApiError::denied(
                "checkpoints_disabled",
                "Checkpoints are off. Turn them on to take a snapshot.",
            ));
        }
        let root = self.root(&req.workspace)?;
        let snapshot = self
            .store
            .capture(
                root,
                Capture {
                    session: req.session.unwrap_or_default(),
                    turn: None,
                    trigger: Trigger::Manual,
                    tool: None,
                    detail: None,
                    label: req.label,
                },
            )
            .await
            .map_err(translate)?;
        Ok(self.export_one(&snapshot))
    }

    async fn checkpoint_restore(
        &self,
        req: WorkspaceCheckpointRestoreReq,
    ) -> ApiResult<WorkspaceCheckpointRestore> {
        // Checked before the workspace resolves: this is a fact about the session, and a refusal
        // that could be reported as "unknown workspace" instead would send the user looking in the
        // wrong place.
        if self.session_is_busy(&req.session) {
            return Err(ApiError::conflict_code(
                "checkpoint_session_busy",
                "the agent is running in this session; restoring now would write under it",
            ));
        }
        let root = self.root(&req.workspace)?;
        // The client names the checkpoint; the store recomputes what restoring would do from the
        // snapshot and the tree as they are now, so a working tree that moved while the
        // confirmation was open cannot be written to on the strength of a stale description.
        let fresh = self
            .store
            .plan_restore(root.clone(), req.checkpoint.clone(), req.session)
            .await
            .map_err(translate)?;
        let options = RestoreOptions {
            cross_head: req.cross_head,
            delete_new: req.delete_new,
            only: req.only.clone(),
        };
        let outcome = self
            .store
            .apply_restore(root, fresh, options)
            .await
            .map_err(translate)?;
        Ok(WorkspaceCheckpointRestore {
            written: outcome.written,
            deleted: outcome.deleted,
            failed: outcome
                .failed
                .into_iter()
                .map(|failure| CheckpointRestoreFailure {
                    path: failure.path,
                    error: failure.error,
                })
                .collect(),
            guard: outcome
                .guard
                .as_ref()
                .map(|snapshot| self.export_one(snapshot)),
        })
    }

    async fn checkpoint_clear(
        &self,
        req: WorkspaceCheckpointClearReq,
    ) -> ApiResult<WorkspaceCheckpointCleared> {
        // No session to be busy in: this writes nothing into the checkout, and the store takes the
        // same per-repository lock a capture does, so a turn mid-snapshot cannot be pulled out
        // from under itself. A snapshot taken after this is a new one, which is what the user
        // turned checkpoints on for.
        let root = self.root(&req.workspace)?;
        let report = self.store.clear(root).await.map_err(translate)?;
        Ok(WorkspaceCheckpointCleared {
            dropped: report.dropped,
            bytes: report.bytes,
        })
    }
}

/// A page is a rendering decision, so the size is the client's — with a floor, because a limit of
/// zero would make the timeline infinite to scroll, and a ceiling, because the whole chain is
/// already in memory by the time a page is cut out of it.
fn page_limit(limit: Option<u32>) -> usize {
    limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE) as usize
}

fn export_snapshot(snapshot: &Snapshot, session_title: Option<String>) -> WorkspaceCheckpoint {
    WorkspaceCheckpoint {
        id: snapshot.id.clone(),
        at: snapshot.at,
        head: snapshot.head.clone(),
        branch: snapshot.branch.clone(),
        session: snapshot.session.clone(),
        session_title,
        turn: snapshot.turn.clone(),
        trigger: match snapshot.trigger {
            Trigger::TurnStart => CheckpointTriggerKind::TurnStart,
            Trigger::TurnEnd => CheckpointTriggerKind::TurnEnd,
            Trigger::BeforeTool => CheckpointTriggerKind::BeforeTool,
            Trigger::Manual => CheckpointTriggerKind::Manual,
            Trigger::BeforeRestore => CheckpointTriggerKind::BeforeRestore,
        },
        tool: snapshot.tool.clone(),
        detail: snapshot.detail.clone(),
        label: snapshot.label.clone(),
        partial: snapshot.partial,
    }
}

fn export_plan(
    plan: zlogic_checkpoints::RestorePlan,
    session_title: Option<String>,
) -> WorkspaceCheckpointPlan {
    let restorable = !plan.snapshot.partial;
    let writes_total = plan.writes_total;
    let deletes_total = plan.deletes_total;
    WorkspaceCheckpointPlan {
        checkpoint: export_snapshot(&plan.snapshot, session_title),
        head_matches: plan.head_matches,
        current_head: plan.current_head,
        writes: plan
            .writes
            .iter()
            .take(ROW_LIMIT)
            .map(|change| export_file(change))
            .collect(),
        writes_total,
        deletes: plan.deletes.into_iter().take(ROW_LIMIT).collect(),
        deletes_total,
        unchanged: plan.unchanged,
        drift: export_stats(plan.drift),
        restorable,
        reason: (!restorable).then(|| {
            "this snapshot left out files over the size or count budget, so it cannot describe a whole tree"
                .to_string()
        }),
    }
}

fn export_step(step: zlogic_checkpoints::StepDiff) -> WorkspaceCheckpointStep {
    let files_total = step.files_total;
    let deletions_total = step.deletions_total;
    WorkspaceCheckpointStep {
        previous: step.previous,
        writes: step
            .files
            .iter()
            .take(ROW_LIMIT)
            .map(|change| export_file(change))
            .collect(),
        files_total,
        deletes: step.deletions.into_iter().take(ROW_LIMIT).collect(),
        deletions_total,
        drift: export_stats(step.drift),
    }
}

fn export_file(change: &zlogic_checkpoints::PathChange) -> WorkspaceCheckpointFile {
    WorkspaceCheckpointFile {
        path: change.path.clone(),
        change: match change.change {
            zlogic_checkpoints::Change::Create => CheckpointFileChange::Create,
            zlogic_checkpoints::Change::Overwrite => CheckpointFileChange::Overwrite,
        },
        lines: change.lines.map(export_stats),
    }
}

fn export_stats(lines: zlogic_checkpoints::LineStats) -> CheckpointLineStats {
    CheckpointLineStats {
        insertions: lines.insertions,
        deletions: lines.deletions,
    }
}

/// The store's wording is written for a transcript; the RPC wants a code a host can branch on.
/// The messages stay as close as possible so a host that only shows the text still says something
/// true.
fn translate(error: CheckpointError) -> ApiError {
    match error {
        CheckpointError::HeadMoved { expected, current } => ApiError::conflict_code(
            "checkpoint_head_moved",
            format!(
                "This checkpoint is from {expected}, but the workspace is now on {current}. \
                 Switch back, or confirm the cross-branch restore."
            ),
        ),
        CheckpointError::Partial(id) => ApiError::conflict_code(
            "checkpoint_incomplete",
            format!("Checkpoint {id} is incomplete and cannot be restored."),
        ),
        CheckpointError::Unknown(id) => ApiError::not_found("checkpoint", id),
        CheckpointError::NotARepository(path) => ApiError::invalid_code(
            "not_a_repository",
            format!("{path} is not a git repository, so it has no checkpoints."),
        ),
        CheckpointError::NotOwned(path) => ApiError::denied(
            "git_owner_mismatch",
            format!(
                "{path} is a git repository zlogic is not allowed to open: its .git is not owned \
                 by the user zlogic runs as."
            ),
        ),
        other => ApiError::internal(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zlogic_protocol::WorkspaceId;
    use zlogic_protocol::query::WorkspaceCheckpointRestoreReq;
    use zlogic_protocol::{SessionId, TurnId};
    use zlogic_store::{Db, LockOutcome, NewSession, SharedStore};

    /// A session with a turn in flight, which is the only thing the gate looks at.
    fn hold(store: &SharedStore) -> SessionId {
        let session = store
            .with(|db| db.sessions().create(NewSession::root(WorkspaceId::new())))
            .unwrap()
            .session_id;
        let outcome = store
            .with(|db| db.locks().acquire(session, TurnId::new(), Some("test")))
            .unwrap();
        assert!(
            matches!(outcome, LockOutcome::Acquired(_)),
            "the test needs the session to actually be held"
        );
        session
    }

    fn timeline() -> (CheckpointTimeline, SharedStore) {
        let store = SharedStore::new(Db::open_in_memory().unwrap());
        let home = tempfile::tempdir().unwrap();
        let checkpoints = Checkpoints::new(
            home.path().to_path_buf(),
            zlogic_checkpoints::Config {
                enabled: true,
                ..Default::default()
            },
        );
        let timeline = CheckpointTimeline::new(
            checkpoints,
            Arc::new(crate::Workspaces::new(store.clone())),
            Arc::new(SessionLocks::new(store.clone(), "test")),
            store.clone(),
        );
        (timeline, store)
    }

    fn request(session: &str) -> WorkspaceCheckpointRestoreReq {
        WorkspaceCheckpointRestoreReq {
            workspace: WorkspaceSelector::Path {
                root: "no-such-workspace".into(),
            },
            checkpoint: "id".into(),
            session: session.into(),
            delete_new: false,
            cross_head: false,
            only: None,
        }
    }

    /// The title column is a join, not a copy: the snapshot records the session id and nothing
    /// else, so this is the one thing the store cannot answer by itself.
    #[test]
    fn a_point_is_named_after_the_session_that_took_it() {
        use zlogic_store::TitleSource;

        let (timeline, store) = timeline();
        let session = store
            .with(|db| db.sessions().create(NewSession::root(WorkspaceId::new())))
            .unwrap()
            .session_id;
        store
            .with(|db| db.sessions().set_title(session, "修复登录 bug", TitleSource::User))
            .unwrap();

        assert_eq!(
            timeline.title_of(&session.to_string()).as_deref(),
            Some("修复登录 bug"),
            "a titled session has to reach the row"
        );
        assert_eq!(
            timeline.title_of("").as_deref(),
            None,
            "a point with no session names no conversation"
        );
    }

    /// A whole-tree restore while the agent is running would write under it: the agent is holding
    /// a view of the tree the restore invalidates, and its next write lands on the rolled-back
    /// state without ever having seen the rollback. The refusal is deliberately checked before the
    /// workspace resolves, so a busy session is never reported as an unknown workspace.
    #[tokio::test]
    async fn a_restore_is_refused_while_the_named_session_has_a_turn() {
        let (timeline, store) = timeline();
        let session = hold(&store);
        let error = timeline
            .checkpoint_restore(request(&session.to_string()))
            .await
            .unwrap_err();
        assert_eq!(error.code, "checkpoint_session_busy", "got {error:?}");
    }

    /// The gate must not become a way to make restore fail: a client that names no session, or
    /// names something that is not one, gets the ordinary "unknown workspace" answer instead.
    #[tokio::test]
    async fn a_session_that_is_not_running_is_not_gated() {
        let (timeline, store) = timeline();
        hold(&store);
        let other = SessionId::new();

        for named in [other.to_string(), String::new(), "not-a-session".into()] {
            let error = timeline
                .checkpoint_restore(request(&named))
                .await
                .unwrap_err();
            assert_ne!(
                error.code, "checkpoint_session_busy",
                "an idle or absent session must not be refused as busy: {named:?} gave {error:?}"
            );
        }
    }
}
