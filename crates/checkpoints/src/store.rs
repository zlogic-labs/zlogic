//! Where snapshots live: one bare repository per user repository, under zlogic's own data
//! directory. The user's repository is opened **read-only** — nothing here writes its index,
//! `HEAD`, refs or config, and no git executable is involved.
//!
//! Each snapshot is one commit whose tree is the working directory (everything git would not
//! ignore, minus nested repositories). Commits chain onto each other so the whole history is one
//! `git log` in `<data>/checkpoints/<id>/repo.git`, and a file's content is stored once no matter
//! how many snapshots contain it.
//!
//! The increment is git's own: the checkpoint repository keeps a private index at
//! `repo.git/index`, so a file is re-read only when its stat changed. That index is the reason a
//! snapshot can sit in front of a mutating tool without being felt.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use git2::{Commit, Index, Oid, Repository, Signature};
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::restore::{self, RestoreOptions, RestoreOutcome, RestorePlan};
use crate::retention::{self, SweepReport};

/// The one ref a checkpoint repository has. `refs/zlogic/…` is outside `refs/heads`, so
/// `git branch`, `git push` and every other ordinary git command cannot see it.
pub const HEAD_REF: &str = "refs/zlogic/checkpoints/current";

pub const REPO_DIR: &str = "repo.git";
const FORMAT: &str = "zlogic-checkpoint 1";
const SIGNATURE_NAME: &str = "Zlogic Checkpoint";
const SIGNATURE_EMAIL: &str = "checkpoint@zlogic.invalid";

#[derive(Debug, thiserror::Error)]
pub enum CheckpointError {
    #[error("{0} is not inside a git repository, so it cannot be checkpointed")]
    NotARepository(String),
    #[error(
        "{0} is a git repository zlogic is not allowed to open: its .git is not owned by the user \
         zlogic runs as"
    )]
    NotOwned(String),
    #[error("git: {0}")]
    Git(#[from] git2::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "snapshot {0} is incomplete, so it cannot describe a whole tree and cannot be restored"
    )]
    Partial(String),
    #[error("the working tree is on {current}, not on {expected}: switch back before restoring")]
    HeadMoved { expected: String, current: String },
    #[error("no checkpoint {0}")]
    Unknown(String),
    #[error("{0} is not a path a snapshot can write to")]
    Refused(String),
}

/// What asked for a snapshot. It lands in the commit, so a snapshot reads back as "before the
/// `edit` call of turn 12" without a second database to keep in step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    TurnStart,
    TurnEnd,
    BeforeTool,
    /// The user pressed the button. Its own kind because "I archived this on purpose" and "the
    /// agent happened to be here" are different reasons to keep a point, and the user will look
    /// for one and not the other.
    Manual,
    /// Taken immediately before a restore writes to the working tree, so the state a restore is
    /// about to overwrite is still reachable afterwards.
    BeforeRestore,
}

impl From<zlogic_tools::CheckpointTrigger> for Trigger {
    fn from(trigger: zlogic_tools::CheckpointTrigger) -> Self {
        match trigger {
            zlogic_tools::CheckpointTrigger::TurnStart => Trigger::TurnStart,
            zlogic_tools::CheckpointTrigger::TurnEnd => Trigger::TurnEnd,
            zlogic_tools::CheckpointTrigger::BeforeTool => Trigger::BeforeTool,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub tree: String,
    /// Unix seconds.
    pub at: i64,
    /// The `HEAD` commit at capture time, absent in a repository with no commits yet. Restore
    /// compares this against the live `HEAD` rather than comparing branch names, so a rename or
    /// a detached head cannot slip past the check.
    pub head: Option<String>,
    /// Display only, never load-bearing: two branches can point at one commit, and a detached
    /// `HEAD` has no name at all.
    pub branch: Option<String>,
    pub session: String,
    pub turn: Option<String>,
    pub trigger: Trigger,
    pub tool: Option<String>,
    /// What the call was about to do, in one line: `git checkout -- .`, `src/lib.rs`. The tool
    /// name alone does not answer "which command wiped my work", and that is the question a user
    /// comes back to a timeline with.
    pub detail: Option<String>,
    /// Words the user typed for a manual point. Display only, and deliberately separate from
    /// `detail`: one is the user's own note, the other is a fact about the tree.
    pub label: Option<String>,
    /// A file over the size cap was left out, or the file budget ran out. The record is still
    /// worth keeping, but it cannot describe a complete tree, so restore refuses it.
    pub partial: bool,
}

impl Snapshot {
    pub fn trigger_label(&self) -> &'static str {
        match self.trigger {
            Trigger::TurnStart => "turn start",
            Trigger::TurnEnd => "turn end",
            Trigger::BeforeTool => "before tool",
            Trigger::Manual => "manual",
            Trigger::BeforeRestore => "before restore",
        }
    }

    /// Whether the snapshot is on the branch that is checked out now. Display only — the
    /// decision to refuse a cross-branch restore belongs to the caller, which can see what is
    /// on screen.
    pub fn is_current_branch(&self, branch: Option<&str>) -> bool {
        self.branch.as_deref() == branch
    }

    pub fn age_label(&self, now: i64) -> String {
        let seconds = now.saturating_sub(self.at).max(0);
        match seconds {
            0..=89 => "just now".into(),
            90..=5_399 => format!("{} min ago", seconds / 60),
            5_400..=172_799 => format!("{} h ago", seconds / 3600),
            _ => format!("{} d ago", seconds / 86_400),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub enabled: bool,
    /// A snapshot older than this is dropped by the sweep.
    pub retention_days: u32,
    /// The cap on snapshots kept per repository, newest first.
    pub max_snapshots: usize,
    /// The cap on the bytes the kept snapshots' trees add up to.
    pub max_bytes: u64,
    /// A file above this is left out of a snapshot rather than copied.
    pub max_file_bytes: u64,
    /// The cap on files per snapshot. Reaching it marks the snapshot partial.
    pub max_files: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            retention_days: 7,
            max_snapshots: 500,
            max_bytes: 2 * 1024 * 1024 * 1024,
            max_file_bytes: 256 * 1024 * 1024,
            max_files: 200_000,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Capture {
    pub session: String,
    pub turn: Option<String>,
    pub trigger: Trigger,
    pub tool: Option<String>,
    pub detail: Option<String>,
    pub label: Option<String>,
}

/// One page of the timeline, and what the client needs to ask for the next one.
#[derive(Debug, Clone)]
pub struct Page {
    pub items: Vec<Snapshot>,
    pub has_more: bool,
    /// The whole chain, not the page: the card's "view all (N)" count is the point of it.
    pub total: usize,
    /// Snapshots taken somewhere other than the branch the caller is showing. Counted over the
    /// whole chain — a partial count on a card holding three rows is a wrong number, not a rough
    /// one.
    pub other_branches: usize,
}

/// What an explicit empty-the-store left behind. Both numbers are the ones the confirmation
/// needed and could not know: what the user is about to lose, and what it costs on disk.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClearReport {
    pub dropped: usize,
    pub bytes: u64,
}

pub struct Checkpoints {
    inner: Arc<Inner>,
}

struct Inner {
    root: PathBuf,
    /// Live, not a boot copy: the switch the user flips in the panel has to take effect on the
    /// next tool call, not the next launch. A write lock is held for a few scalar assignments and
    /// a read for a six-field clone, so this is never the reason a snapshot is slow.
    config: RwLock<Config>,
    /// One writer per user repository: a snapshot racing another would be a torn tree.
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// When each repository was last swept, so retention runs on a slow clock rather than on
    /// every snapshot. A process that never snapshots never sweeps, which costs nothing.
    swept: Mutex<HashMap<String, i64>>,
}

struct Located {
    user: Repository,
    directory: PathBuf,
}

struct Opened {
    /// The checkpoint store, workdir pointed at the user's checkout.
    repo: Repository,
    /// The user's own repository, opened read-only.
    user: Repository,
    root: PathBuf,
}

impl Checkpoints {
    pub fn new(root: impl Into<PathBuf>, config: Config) -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(Inner {
                root: root.into(),
                config: RwLock::new(config),
                locks: Mutex::new(HashMap::new()),
                swept: Mutex::new(HashMap::new()),
            }),
        })
    }

    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    pub fn config(&self) -> Config {
        self.inner.config()
    }

    /// Replace the live policy. The settings page writes `config.yaml` and the engine pushes the
    /// result here, so turning checkpoints on or off — or tightening what a snapshot may hold —
    /// takes effect without a restart.
    pub fn set_config(&self, config: Config) {
        *self.inner.config.write().expect("checkpoint config") = config;
    }

    pub fn enabled(&self) -> bool {
        self.config().enabled
    }

    /// Where this workspace's snapshots are kept, for the panel to show the user. `None` when the
    /// folder is not a repository, because there is no store to point at.
    pub fn repository_dir(&self, workspace: &Path) -> Option<PathBuf> {
        self.inner
            .locate(workspace)
            .ok()
            .map(|located| located.directory)
    }

    /// One snapshot of the working tree of the repository owning `workspace`.
    pub async fn capture(
        &self,
        workspace: PathBuf,
        capture: Capture,
    ) -> Result<Snapshot, CheckpointError> {
        self.maybe_sweep(&workspace).await;
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || inner.capture(&workspace, &capture))
            .await
            .map_err(join_error)?
    }

    /// Retention on a slow clock: a store that is only swept when a snapshot happens would keep
    /// the last tree of a session forever, and a repository nobody works in would keep all of
    /// them. A failure is logged and forgotten — the next snapshot tries again.
    async fn maybe_sweep(&self, workspace: &Path) {
        const INTERVAL: i64 = 3600;
        let at = now();
        let Ok(located) = self.inner.locate(workspace) else {
            return;
        };
        let id = repo_id(&located.user);
        {
            let mut swept = self
                .inner
                .swept
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if swept
                .get(&id)
                .is_some_and(|last| at.saturating_sub(*last) < INTERVAL)
            {
                return;
            }
            swept.insert(id.clone(), at);
        }
        if let Err(error) = self.sweep(workspace.to_path_buf(), at).await {
            tracing::warn!(target: "zlogic::checkpoints", %error, "checkpoint sweep failed");
        }
    }

    /// Every snapshot of this repository, newest first.
    pub async fn list(&self, workspace: PathBuf) -> Result<Vec<Snapshot>, CheckpointError> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || inner.list(&workspace))
            .await
            .map_err(join_error)?
    }

    /// One page of [`Checkpoints::list`], for a timeline with more rows than a dialog should hold
    /// at once. `before` is the id the previous page ended on: paging by position would repeat or
    /// skip rows as soon as a snapshot lands mid-scroll, paging by id cannot. `branch` is the one
    /// the caller is showing, so the page can also say how many snapshots are on other branches.
    pub async fn list_page(
        &self,
        workspace: PathBuf,
        before: Option<String>,
        limit: usize,
        branch: Option<String>,
    ) -> Result<Page, CheckpointError> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            inner.list_page(&workspace, before.as_deref(), limit, branch.as_deref())
        })
        .await
        .map_err(join_error)?
    }

    /// What restoring `id` would do, without doing it. The plan is a snapshot of intent, not a
    /// promise: the working tree can move between this call and the apply, and the apply is what
    /// actually writes.
    pub async fn plan_restore(
        &self,
        workspace: PathBuf,
        id: String,
        session: String,
    ) -> Result<RestorePlan, CheckpointError> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || inner.plan_restore(&workspace, &id, &session))
            .await
            .map_err(join_error)?
    }

    /// The patch for one file of a snapshot, against the step that ended at it or against the
    /// working tree, whichever the caller is looking at. This is the same comparison the plan's
    /// rows come from, narrowed to a single path — asked for when a user opens one row, not with
    /// the plan, because a patch per file would make every expansion of the timeline cost a diff
    /// of the whole workspace.
    pub async fn file_diff(
        &self,
        workspace: PathBuf,
        id: String,
        path: String,
        compare: restore::Compare,
    ) -> Result<restore::FileDiff, CheckpointError> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || inner.file_diff(&workspace, &id, &path, compare))
            .await
            .map_err(join_error)?
    }

    /// What changed between a point and the one before it — the browsing view of a timeline row.
    ///
    /// It touches no file on disk, and that is the point. A row is opened to be read, and the
    /// answer is a diff between two commits the store already holds; making the reader wait for a
    /// stat walk of the whole workspace to learn which three files a step wrote is the cost of
    /// answering a question nobody asked. What restoring would change is a different question,
    /// answered by [`Checkpoints::plan_restore`] when someone is about to press the button.
    pub async fn step(
        &self,
        workspace: PathBuf,
        id: String,
    ) -> Result<restore::StepDiff, CheckpointError> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || inner.step(&workspace, &id))
            .await
            .map_err(join_error)?
    }

    /// Carries out a plan, after taking a `BeforeRestore` snapshot of the state it is about to
    /// overwrite. That snapshot is what makes a restore reversible: without it, the tree the
    /// user was in before confirming is only recoverable by finding a snapshot that happens to
    /// match, which is not the same thing.
    pub async fn apply_restore(
        &self,
        workspace: PathBuf,
        plan: RestorePlan,
        options: RestoreOptions,
    ) -> Result<RestoreOutcome, CheckpointError> {
        let session = plan.snapshot.session.clone();
        // A failed guard is not a reason to refuse the restore: it means the state being
        // overwritten is one snapshot older than intended, which the user is about to lose on
        // purpose. Losing the restore instead would be the worse of the two.
        let guard = match self
            .capture(
                workspace.clone(),
                Capture {
                    session,
                    turn: None,
                    trigger: Trigger::BeforeRestore,
                    tool: None,
                    detail: options.only.clone(),
                    label: None,
                },
            )
            .await
        {
            Ok(snapshot) => Some(snapshot),
            Err(error) => {
                tracing::warn!(target: "zlogic::checkpoints", %error, "restore guard snapshot failed");
                None
            }
        };
        let inner = self.inner.clone();
        let mut outcome =
            tokio::task::spawn_blocking(move || inner.apply_restore(&workspace, plan, options))
                .await
                .map_err(join_error)??;
        // The guard may be an existing snapshot rather than a new one — a capture of an unchanged
        // tree reuses the last commit — and that is exactly right: it is still the point that
        // describes the state being overwritten.
        outcome.guard = guard;
        Ok(outcome)
    }

    /// Drops what the policy no longer keeps. Takes the same per-repository lock a capture does,
    /// so it can never run against a half-written tree.
    pub async fn sweep(&self, workspace: PathBuf, at: i64) -> Result<SweepReport, CheckpointError> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || inner.sweep(&workspace, at))
            .await
            .map_err(join_error)?
    }

    /// Sweeps every store under the root rather than the one a capture just touched. Retention
    /// that only runs on a snapshot never reaches a repository the user has walked away from,
    /// which is exactly the one holding the largest number of copies of code nobody is editing.
    pub async fn sweep_all(&self, at: i64) -> Result<SweepReport, CheckpointError> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || inner.sweep_all(at))
            .await
            .map_err(join_error)?
    }

    /// Deletes every snapshot of this workspace, and the directory that holds them. Nothing here
    /// is recoverable afterwards — no snapshot survives to restore from and the timeline starts
    /// empty — which is why the caller is expected to have asked.
    pub async fn clear(&self, workspace: PathBuf) -> Result<ClearReport, CheckpointError> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || inner.clear(&workspace))
            .await
            .map_err(join_error)?
    }
}

impl Inner {
    fn config(&self) -> Config {
        self.config.read().expect("checkpoint config").clone()
    }

    fn capture(&self, workspace: &Path, capture: &Capture) -> Result<Snapshot, CheckpointError> {
        let located = self.locate(workspace)?;
        let _held = self.hold(&located.user);
        let root = located
            .user
            .workdir()
            .ok_or_else(|| CheckpointError::NotARepository(workspace.display().to_string()))?
            .to_path_buf();
        std::fs::create_dir_all(&located.directory)?;
        let repo = open_repo(&located.directory.join(REPO_DIR))?;
        // A workdir is what lets the private index resolve the paths the walker hands it, and what
        // turns this repository from bare into a normal one. Nothing else may use that: a
        // checkout through this handle would rewrite the user's files.
        repo.set_workdir(&root, false)?;
        let (head, branch) = head_info(&located.user);

        let written = write_working_tree(
            &repo,
            &root,
            self.config().max_file_bytes,
            self.config().max_files,
        )?;
        let previous = tip(&repo);
        if let Some(previous) = &previous
            && previous.tree == written.tree.to_string()
            && previous.head == head
            && !written.partial
        {
            return Ok(previous.clone());
        }

        let signature = Signature::now(SIGNATURE_NAME, SIGNATURE_EMAIL)?;
        let meta = Snapshot {
            id: String::new(),
            tree: written.tree.to_string(),
            at: now(),
            head,
            branch,
            session: capture.session.clone(),
            turn: capture.turn.clone(),
            trigger: capture.trigger,
            tool: capture.tool.clone(),
            // Collapsed here rather than at encode time, so the record handed back to the caller
            // is the same one a later list will read. Sanitising on the way out alone would make
            // a point's label change the moment the timeline was reopened.
            detail: one_line(capture.detail.as_deref()),
            label: one_line(capture.label.as_deref()),
            partial: written.partial,
        };
        let parents = previous
            .iter()
            .filter_map(|parent| Oid::from_str(&parent.id).ok())
            .filter_map(|oid| repo.find_commit(oid).ok())
            .collect::<Vec<Commit>>();
        let parent_refs = parents.iter().collect::<Vec<&Commit>>();
        let tree = repo.find_tree(written.tree)?;
        let id = repo.commit(
            None,
            &signature,
            &signature,
            &encode(&meta),
            &tree,
            &parent_refs,
        )?;
        repo.reference(HEAD_REF, id, true, "zlogic: snapshot")?;
        Ok(Snapshot {
            id: id.to_string(),
            ..meta
        })
    }

    fn list(&self, workspace: &Path) -> Result<Vec<Snapshot>, CheckpointError> {
        let Some(opened) = self.open_stored(workspace)? else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for oid in walk_from_tip(&opened.repo)? {
            let commit = opened.repo.find_commit(oid)?;
            if let Some(snapshot) = decode(&commit) {
                out.push(snapshot);
            }
        }
        Ok(out)
    }

    fn list_page(
        &self,
        workspace: &Path,
        before: Option<&str>,
        limit: usize,
        branch: Option<&str>,
    ) -> Result<Page, CheckpointError> {
        // The chain is retention-bounded (a few hundred commits), so it is read whole and then cut.
        // Paging exists for what the client sends and renders, not to make the disk walk cheaper.
        let every = self.list(workspace)?;
        // Counted before the filter, over the whole chain: this is what the card's "N on other
        // branches" line says, and a count taken from one page would be a wrong number.
        let other_branches = branch
            .map(|branch| {
                every
                    .iter()
                    .filter(|snapshot| snapshot.branch.as_deref() != Some(branch))
                    .count()
            })
            .unwrap_or(0);
        // The filter is here rather than in the client so a page is thirty rows of what the view
        // shows. Filtering after paging would make scrolling through one branch walk every snapshot
        // ever taken on every other one.
        let all: Vec<Snapshot> = match branch {
            Some(branch) => every
                .into_iter()
                .filter(|snapshot| snapshot.branch.as_deref() == Some(branch))
                .collect(),
            None => every,
        };
        let total = all.len();
        let start = match before {
            // A cursor that no longer exists — a snapshot the sweep dropped between pages — pages
            // from the top rather than returning nothing, so a stale scroll still shows rows.
            Some(cursor) => all
                .iter()
                .position(|snapshot| snapshot.id == cursor)
                .map(|index| index + 1)
                .unwrap_or(0),
            None => 0,
        };
        let end = start.saturating_add(limit).min(total);
        Ok(Page {
            items: all[start..end].to_vec(),
            has_more: end < total,
            total,
            other_branches,
        })
    }

    fn plan_restore(
        &self,
        workspace: &Path,
        id: &str,
        session: &str,
    ) -> Result<RestorePlan, CheckpointError> {
        let Some(opened) = self.open_existing(workspace)? else {
            return Err(CheckpointError::Unknown(id.to_string()));
        };
        let oid = Oid::from_str(id)?;
        let commit = opened.repo.find_commit(oid)?;
        let snapshot = decode(&commit).ok_or_else(|| CheckpointError::Unknown(id.to_string()))?;
        if snapshot.partial {
            return Err(CheckpointError::Partial(id.to_string()));
        }
        let current = write_working_tree(
            &opened.repo,
            &opened.root,
            self.config().max_file_bytes,
            self.config().max_files,
        )?;
        let protect = session_tree(&opened.repo, session);
        let (head, _) = head_info(&opened.user);
        restore::plan(&opened.repo, current.tree, head, &commit, snapshot, protect)
    }

    fn step(&self, workspace: &Path, id: &str) -> Result<restore::StepDiff, CheckpointError> {
        let Some(opened) = self.open_stored(workspace)? else {
            return Err(CheckpointError::Unknown(id.to_string()));
        };
        let commit = opened.repo.find_commit(Oid::from_str(id)?)?;
        restore::step_diff(&opened.repo, &commit)
    }

    fn file_diff(
        &self,
        workspace: &Path,
        id: &str,
        path: &str,
        compare: restore::Compare,
    ) -> Result<restore::FileDiff, CheckpointError> {
        // The step is a diff between two commits and needs nothing from the disk; the workspace
        // comparison does, and pays for the same stat walk the plan pays — and so pays for the
        // workdir too.
        let workdir = compare == restore::Compare::Workspace;
        let Some(opened) = self.open_store(workspace, workdir)? else {
            return Err(CheckpointError::Unknown(id.to_string()));
        };
        let commit = opened.repo.find_commit(Oid::from_str(id)?)?;
        let current = match compare {
            restore::Compare::Previous => None,
            restore::Compare::Workspace => Some(write_working_tree(
                &opened.repo,
                &opened.root,
                self.config().max_file_bytes,
                self.config().max_files,
            )?),
        };
        let current = current.map_or(Oid::zero(), |written| written.tree);
        restore::file_diff(&opened.repo, current, &commit, path, compare)
    }

    fn apply_restore(
        &self,
        workspace: &Path,
        plan: RestorePlan,
        options: RestoreOptions,
    ) -> Result<RestoreOutcome, CheckpointError> {
        let Some(opened) = self.open_existing(workspace)? else {
            return Err(CheckpointError::Unknown(plan.snapshot.id));
        };
        // The same-head rule is re-checked here rather than trusted from the plan: between the
        // plan and the confirm the user may have switched branches, and a stale plan must not
        // authorise a write against a `HEAD` it never saw.
        let live = opened.user.head().ok().and_then(|head| head.target());
        let current = live.map(|oid| oid.to_string());
        if !options.cross_head && plan.snapshot.head != current {
            return Err(CheckpointError::HeadMoved {
                expected: plan
                    .snapshot
                    .head
                    .clone()
                    .unwrap_or_else(|| "no commits".into()),
                current: current.unwrap_or_else(|| "no commits".into()),
            });
        }
        restore::apply(&opened.repo, plan, options)
    }

    fn sweep(&self, workspace: &Path, at: i64) -> Result<SweepReport, CheckpointError> {
        let located = self.locate(workspace)?;
        let _held = self.hold(&located.user);
        let path = located.directory.join(REPO_DIR);
        let repo = match Repository::open(path) {
            Ok(repo) => repo,
            Err(error) if error.code() == git2::ErrorCode::NotFound => {
                return Ok(SweepReport::default());
            }
            Err(error) => return Err(error.into()),
        };
        retention::sweep(&repo, &self.config(), at)
    }

    fn sweep_all(&self, at: i64) -> Result<SweepReport, CheckpointError> {
        let mut total = SweepReport::default();
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Ok(total);
        };
        let config = self.config();
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            if !kind.is_dir() {
                continue;
            }
            // One repository's failure is not the pass's: a store that cannot be opened is left
            // exactly as it is, and the rest are still swept.
            match self.sweep_directory(&entry.path(), &config, at) {
                Ok(report) => {
                    total.kept += report.kept;
                    total.dropped += report.dropped;
                    total.bytes += report.bytes;
                }
                Err(error) => {
                    tracing::warn!(
                        target: "zlogic::checkpoints",
                        %error,
                        path = %entry.path().display(),
                        "checkpoint sweep failed for one store"
                    );
                }
            }
        }
        Ok(total)
    }

    fn clear(&self, workspace: &Path) -> Result<ClearReport, CheckpointError> {
        let located = self.locate(workspace)?;
        let _held = self.hold(&located.user);
        let id = repo_id(&located.user);
        if !located.directory.join(REPO_DIR).exists() {
            return Ok(ClearReport::default());
        }
        let dropped = Repository::open(located.directory.join(REPO_DIR))
            .map(|repo| walk_from_tip(&repo).map(|chain| chain.len()).unwrap_or(0))
            .unwrap_or(0);
        let bytes = retention::directory_size(&located.directory);
        std::fs::remove_dir_all(&located.directory)?;
        self.swept
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&id);
        Ok(ClearReport { dropped, bytes })
    }

    fn sweep_directory(
        &self,
        directory: &Path,
        config: &Config,
        at: i64,
    ) -> Result<SweepReport, CheckpointError> {
        let name = directory
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let _held = self.hold_id(&name);
        let report = retention::sweep(
            &Repository::open(directory.join(REPO_DIR))?,
            config,
            at,
        )?;
        self.swept
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(name, at);
        Ok(report)
    }

    fn locate(&self, workspace: &Path) -> Result<Located, CheckpointError> {
        let path = workspace.display().to_string();
        // A repository zlogic may not open is not an absent one: saying "not a repository" here
        // is what offered the user `git init` for a checkout that already had one.
        let user = Repository::discover(workspace).map_err(|error| {
            if error.code() == git2::ErrorCode::Owner {
                CheckpointError::NotOwned(path)
            } else {
                CheckpointError::NotARepository(path)
            }
        })?;
        let directory = self.root.join(repo_id(&user));
        Ok(Located { user, directory })
    }

    /// Opens the checkpoint store for the repository owning `workspace`, with its workdir pointed
    /// at the user's checkout — the private index and the restore path both need it.
    fn open_existing(&self, workspace: &Path) -> Result<Option<Opened>, CheckpointError> {
        Ok(self.open_store(workspace, true)?)
    }

    /// Opens the store without pointing it at the checkout, for the reads that only ever look at
    /// commits the store already holds.
    ///
    /// A workdir costs more than it looks. With one set, diffing two commits measured four times
    /// slower on a 2400-file workspace — 125 ms against 32 ms — and the whole of it landed in the
    /// part that reads blobs and runs the differ, not in setup. The index is not the reason: it
    /// loads in 3 ms. Whatever libgit2 does with a workdir attached, a question about two stored
    /// snapshots has no use for it, and leaving it off also keeps the user's `core.autocrlf` and
    /// `.gitattributes` out of a comparison between bytes we deliberately stored raw.
    fn open_stored(&self, workspace: &Path) -> Result<Option<Opened>, CheckpointError> {
        Ok(self.open_store(workspace, false)?)
    }

    fn open_store(
        &self,
        workspace: &Path,
        workdir: bool,
    ) -> Result<Option<Opened>, CheckpointError> {
        let located = self.locate(workspace)?;
        let repo = match Repository::open(located.directory.join(REPO_DIR)) {
            Ok(repo) => repo,
            Err(error) if error.code() == git2::ErrorCode::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let root = located
            .user
            .workdir()
            .ok_or_else(|| CheckpointError::NotARepository(workspace.display().to_string()))?
            .to_path_buf();
        if workdir {
            repo.set_workdir(&root, false)?;
        }
        Ok(Some(Opened {
            repo,
            user: located.user,
            root,
        }))
    }

    fn hold(&self, user: &Repository) -> Arc<Mutex<()>> {
        self.hold_id(&repo_id(user))
    }

    /// The same lock keyed by the store directory's name, which is the repository id the capture
    /// path derives. A sweep that only walks the root never opens the user's repository, so this
    /// is how it still serialises against a capture in flight.
    fn hold_id(&self, id: &str) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock().unwrap_or_else(|error| error.into_inner());
        locks
            .entry(id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
}

/// The oldest snapshot of `session`: what the tree looked like before the session touched it. Its
/// tree is the floor for "delete", so a file the user had before the session began is never
/// removed by a restore.
fn session_tree(repo: &Repository, session: &str) -> Option<Oid> {
    // The walk is newest first and every match counts, so the last one is the oldest — the
    // session's starting point, not its latest state.
    let mut found = None;
    for oid in walk_from_tip(repo).ok()? {
        let Ok(commit) = repo.find_commit(oid) else {
            continue;
        };
        if decode(&commit).is_some_and(|snapshot| snapshot.session == session) {
            found = Some(commit.tree_id());
        }
    }
    found
}

/// Every snapshot, newest first. The walk starts at the checkpoint ref rather than at `HEAD`,
/// which in the checkpoint repository is deliberately unborn — it has no branch of its own.
pub(crate) fn walk_from_tip(repo: &Repository) -> Result<Vec<Oid>, CheckpointError> {
    let Some(oid) = repo.find_reference(HEAD_REF).ok().and_then(|r| r.target()) else {
        return Ok(Vec::new());
    };
    let mut walk = repo.revwalk()?;
    walk.push(oid)?;
    let mut out = Vec::new();
    for oid in walk {
        out.push(oid?);
    }
    Ok(out)
}

fn join_error(error: tokio::task::JoinError) -> CheckpointError {
    CheckpointError::Io(std::io::Error::other(error.to_string()))
}

struct WrittenTree {
    tree: Oid,
    partial: bool,
}

/// The working directory as a tree in `repo`, written through the repository's own private index
/// so unchanged files are not re-read. The walk is the `ignore` crate's, which applies
/// `.gitignore`, `.git/info/exclude` and the user's global excludes — the set git itself would
/// keep. Those are found by walking up from the user's own root, which is why the walk starts
/// there and not at the checkpoint store.
fn write_working_tree(
    repo: &Repository,
    root: &Path,
    max_file_bytes: u64,
    max_files: usize,
) -> Result<WrittenTree, CheckpointError> {
    let mut index = repo.index()?;
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    let mut partial = false;
    let mut added = 0usize;
    let mut complete = true;
    let index_written = index
        .path()
        .and_then(|path| std::fs::metadata(path).ok())
        .and_then(|meta| meta.modified().ok());

    let walker = WalkBuilder::new(root)
        .hidden(false)
        .parents(false)
        .ignore(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .filter_entry(|entry| {
            // `.git` is the one name that is never content — it is a repository's own storage,
            // and in a worktree or a submodule it is a file rather than a directory. Skipping it
            // by name is also what lets a nested repository be walked: a directory that holds a
            // `.git` of its own is still a directory of files. Pruning on `.git` instead left a
            // workspace opened as the parent of several repositories holding nothing but the few
            // files that sit beside them, and every file the model actually edits missing from
            // every snapshot.
            entry.file_name() != ".git"
        })
        .build();

    for entry in walker {
        let Ok(entry) = entry else {
            partial = true;
            continue;
        };
        let Some(kind) = entry.file_type() else {
            continue;
        };
        if !(kind.is_file() || kind.is_symlink()) {
            continue;
        }
        let meta = entry.metadata().ok();
        if kind.is_file()
            && meta
                .as_ref()
                .is_some_and(|meta| meta.len() > max_file_bytes)
        {
            partial = true;
            continue;
        }
        if added >= max_files {
            partial = true;
            complete = false;
            break;
        }
        let Ok(relative) = entry.path().strip_prefix(root) else {
            continue;
        };
        // The key has to be the path **git** stores, not the one the OS hands us: an index entry
        // is UTF-8 with `/` separators, while `Path` on Windows is WTF-8 with `\`. Keyed on the
        // OS form, every file below the root looks unseen to the stale pass below and is deleted
        // the moment it is added — a snapshot of the top level only, silently.
        let Some(key) = git_path(relative) else {
            partial = true;
            continue;
        };
        // A file the index already describes is left alone, which is what turns a snapshot of an
        // unchanged tree into a stat walk instead of a full rehash. `add_path` re-reads and
        // rehashes every path it is given, so the comparison has to happen here.
        if kind.is_file()
            && meta.is_some_and(|meta| index_already_has(&index, relative, &meta, index_written))
        {
            seen.insert(key);
            continue;
        }
        seen.insert(key);
        match index.add_path(relative) {
            Ok(()) => added += 1,
            Err(error) => {
                // One unreadable path is not a reason to store nothing. The snapshot is marked
                // partial, which keeps it in the timeline and refuses it as a restore point.
                tracing::debug!(target: "zlogic::checkpoints", %error, path = %relative.display(), "checkpoint skipped a path");
                partial = true;
            }
        }
    }

    // Only a walk that reached the end of the tree knows which entries are stale. Pruning against
    // a truncated walk would drop files that are still there.
    if complete {
        let stale = index
            .iter()
            .filter(|entry| !seen.contains(&entry.path))
            .map(|entry| entry.path.clone())
            .collect::<Vec<_>>();
        for path in stale {
            index.remove_path(&bytes_to_path(&path))?;
        }
    }

    let tree = index.write_tree()?;
    index.write()?;
    Ok(WrittenTree { tree, partial })
}

/// Whether the index already holds this exact file, so that reading and hashing it again would
/// only arrive at the object it points at. Size, mtime to the nanosecond and the executable bit
/// have to agree, and the mtime has to be older than the index's own — a file written in the same
/// instant the index was is one git refuses to trust on a filesystem whose timestamps are coarse,
/// and a wrong "unchanged" here is a snapshot that quietly loses an edit.
fn index_already_has(
    index: &Index,
    relative: &Path,
    meta: &std::fs::Metadata,
    index_written: Option<SystemTime>,
) -> bool {
    let Some(entry) = index.get_path(relative, 0) else {
        return false;
    };
    if u64::from(entry.file_size) != meta.len() {
        return false;
    }
    if is_executable(meta) != (entry.mode & 0o111 != 0) {
        return false;
    }
    let Ok(modified) = meta.modified() else {
        return false;
    };
    let Ok(since_epoch) = modified.duration_since(UNIX_EPOCH) else {
        return false;
    };
    if entry.mtime.seconds() != since_epoch.as_secs() as i32
        || entry.mtime.nanoseconds() != since_epoch.subsec_nanos()
    {
        return false;
    }
    match index_written {
        Some(written) => modified < written,
        None => false,
    }
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

/// Windows has no executable bit, and libgit2 records the mode it would have written itself, so
/// there is nothing to compare there.
#[cfg(not(unix))]
fn is_executable(_meta: &std::fs::Metadata) -> bool {
    false
}

/// A path in the form an index entry uses: UTF-8, `/` separators, no leading `./`. `None` when the
/// name is not valid UTF-8, which git cannot represent either — libgit2's own path conversion
/// rejects those, so there is nothing to store under that name.
fn git_path(relative: &Path) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for (index, component) in relative.components().enumerate() {
        let std::path::Component::Normal(name) = component else {
            return None;
        };
        if index > 0 {
            out.push(b'/');
        }
        out.extend_from_slice(name.to_str()?.as_bytes());
    }
    (!out.is_empty()).then_some(out)
}

fn open_repo(path: &Path) -> Result<Repository, CheckpointError> {
    match Repository::open(path) {
        Ok(repo) => Ok(repo),
        Err(_) => match Repository::init_bare(path) {
            Ok(repo) => Ok(repo),
            // Another process won the race to create it.
            Err(_) => Ok(Repository::open(path)?),
        },
    }
}

/// The repository's own identity, not its checkout's: a worktree shares a common directory with
/// the main repository, and both must land in the same checkpoint store or a session that moved
/// between them would watch its own history disappear.
fn repo_id(repo: &Repository) -> String {
    let mut hasher = Sha256::new();
    hasher.update(zlogic_paths::normalise(repo.commondir()).as_bytes());
    hasher.finalize()[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn head_info(repo: &Repository) -> (Option<String>, Option<String>) {
    let Ok(head) = repo.head() else {
        return (None, None);
    };
    let commit = head.target().map(|oid| oid.to_string());
    let branch = if repo.head_detached().unwrap_or(false) {
        None
    } else {
        head.shorthand().map(str::to_string)
    };
    (commit, branch)
}

fn tip(repo: &Repository) -> Option<Snapshot> {
    let reference = repo.find_reference(HEAD_REF).ok()?;
    let commit = repo.find_commit(reference.target()?).ok()?;
    decode(&commit)
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0)
}

pub(crate) fn encode(snapshot: &Snapshot) -> String {
    let none = "-";
    format!(
        "{FORMAT}\nat: {}\nhead: {}\nbranch: {}\nsession: {}\nturn: {}\ntrigger: {}\ntool: {}\ndetail: {}\nlabel: {}\npartial: {}\n",
        snapshot.at,
        snapshot.head.as_deref().unwrap_or(none),
        snapshot.branch.as_deref().unwrap_or(none),
        snapshot.session,
        snapshot.turn.as_deref().unwrap_or(none),
        match snapshot.trigger {
            Trigger::TurnStart => "turn_start",
            Trigger::TurnEnd => "turn_end",
            Trigger::BeforeTool => "before_tool",
            Trigger::Manual => "manual",
            Trigger::BeforeRestore => "before_restore",
        },
        snapshot.tool.as_deref().unwrap_or(none),
        snapshot.detail.as_deref().unwrap_or(none),
        snapshot.label.as_deref().unwrap_or(none),
        snapshot.partial,
    )
}

/// A trailer is one line, so a label or a command with a newline in it would end the message
/// early and leave the rest of the record unparsed. Applied when the snapshot is built, not when
/// it is written, so the record and its stored form are the same text.
fn one_line(text: Option<&str>) -> Option<String> {
    const LIMIT: usize = 300;
    let text = text?.trim();
    if text.is_empty() {
        return None;
    }
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::new();
    for c in flat.chars() {
        if out.chars().count() >= LIMIT {
            out.push('…');
            break;
        }
        out.push(c);
    }
    Some(out)
}

pub(crate) fn decode(commit: &Commit<'_>) -> Option<Snapshot> {
    let body = commit.message()?;
    if !body.starts_with(FORMAT) {
        return None;
    }
    let mut at = None;
    let mut head = None;
    let mut branch = None;
    let mut session = None;
    let mut turn = None;
    let mut trigger = None;
    let mut tool = None;
    let mut detail = None;
    let mut label = None;
    let mut partial = false;
    for line in body.lines() {
        let Some((key, value)) = line.split_once(": ") else {
            continue;
        };
        let present = |value: &str| (value != "-").then(|| value.to_string());
        match key {
            "at" => at = value.parse().ok(),
            "head" => head = present(value),
            "branch" => branch = present(value),
            "session" => session = present(value),
            "turn" => turn = present(value),
            "trigger" => {
                trigger = match value {
                    "turn_start" => Some(Trigger::TurnStart),
                    "turn_end" => Some(Trigger::TurnEnd),
                    "before_tool" => Some(Trigger::BeforeTool),
                    "manual" => Some(Trigger::Manual),
                    "before_restore" => Some(Trigger::BeforeRestore),
                    _ => None,
                }
            }
            "tool" => tool = present(value),
            "detail" => detail = present(value),
            "label" => label = present(value),
            "partial" => partial = value == "true",
            _ => {}
        }
    }
    Some(Snapshot {
        id: commit.id().to_string(),
        tree: commit.tree_id().to_string(),
        at: at?,
        head,
        branch,
        session: session?,
        turn,
        trigger: trigger?,
        tool,
        detail,
        label,
        partial,
    })
}

#[cfg(unix)]
fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec()))
}

#[cfg(not(unix))]
fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}
