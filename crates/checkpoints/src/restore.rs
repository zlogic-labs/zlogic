//! Restoring a snapshot means putting a tree back, not applying a patch: the working tree ends up
//! byte-for-byte what the snapshot recorded, and `git status` is left to say so. That is the
//! semantic the user asked for and the one a diff-based restore cannot give — there is no
//! three-way merge here, and nothing about a file the user edited after the snapshot survives.
//!
//! What is *not* restored, and says so in the plan rather than in a footnote: files git ignores
//! (they were never in the tree), the staging area (a tree cannot express it), and anything the
//! session had not created — a restore never deletes a file the user brought with them.

use std::collections::BTreeMap;
use std::path::Path;

use git2::{Commit, Oid, Repository, Tree};
use serde::{Deserialize, Serialize};

use crate::store::{CheckpointError, Snapshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    /// Not on disk now; the snapshot has it.
    Create,
    /// On disk with different content; the snapshot's version wins.
    Overwrite,
}

/// Lines moved between the snapshot and now, counted from the snapshot **towards** the working
/// tree: a positive `insertions` is a line that appeared after the snapshot was taken. The list
/// is the answer to "how far have I drifted", and it is what the panel shows before anyone opens
/// a diff.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineStats {
    pub insertions: u32,
    pub deletions: u32,
}

impl LineStats {
    pub fn is_empty(&self) -> bool {
        self.insertions == 0 && self.deletions == 0
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PathChange {
    pub path: String,
    pub change: Change,
    /// Absent when the change is a binary file or when the file budget cut the count short.
    pub lines: Option<LineStats>,
}

/// What changed between one point in the timeline and the one before it. This is the browsing
/// question — "what did the agent do to get from there to here" — and it is the default view of an
/// expanded row, because a snapshot records a state rather than an action: the work belongs to the
/// step that ended at this point. The restore side of a plan answers the other question, "what
/// would pressing the button do", and is only needed once someone is about to press it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StepDiff {
    /// The snapshot this one is compared against, absent for the first point in a store: there is
    /// nothing before it to compare with, and saying so is better than an empty diff that reads
    /// as "this step changed nothing".
    pub previous: Option<String>,
    pub files: Vec<PathChange>,
    /// How many files the step touched, against `files.len()`.
    pub files_total: usize,
    /// Paths the step removed.
    pub deletions: Vec<String>,
    /// How many paths the step removed, against `deletions.len()`.
    pub deletions_total: usize,
    pub drift: LineStats,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestorePlan {
    pub snapshot: Snapshot,
    /// Whether the live `HEAD` is the commit the snapshot was taken at. The caller may still
    /// restore across a mismatch; the default refuses, because the result would be one branch's
    /// files under another branch's `HEAD` and every later git command would object.
    pub head_matches: bool,
    pub current_head: Option<String>,
    pub writes: Vec<PathChange>,
    /// How many files a restore would write, which is not `writes.len()`: the rows are capped.
    pub writes_total: usize,
    pub deletes: Vec<String>,
    /// How many files a restore would remove, capped the same way.
    pub deletes_total: usize,
    pub unchanged: usize,
    /// The whole tree's drift, which is not the sum of `writes` and `deletes` alone: a deleted
    /// file's lines are part of it too.
    pub drift: LineStats,
}

impl RestorePlan {
    pub fn is_empty(&self) -> bool {
        self.writes.is_empty() && self.deletes.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreOutcome {
    pub written: usize,
    pub deleted: usize,
    pub failed: Vec<RestoreFailure>,
    /// The state as it was immediately before this restore wrote, when the caller took one. This
    /// is the way back out of a restore that turned out to be the wrong one, so it travels with
    /// the result rather than being a detail only the store knows.
    #[serde(default)]
    pub guard: Option<crate::store::Snapshot>,
}

/// One file a restore could not write, kept apart rather than pre-formatted into a single string:
/// the path is a git path, which may itself contain a colon, so a client that has to take the
/// string apart again to show the user which file failed is a client that will show the wrong one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreFailure {
    pub path: String,
    pub error: String,
}

/// Every blob in a tree, flattened, keyed by its full path from the root. Trees are not listed
/// separately: git has no record of an empty directory, and a path that is a directory in one tree
/// and a file in the other is exactly a write plus a delete.
///
/// The prefix is carried down the walk because a tree entry knows only its own name: two files
/// both called `index.ts` are different files, and a key of just the leaf name would collapse
/// them into one — and, worse, name a path that does not exist on disk to write to.
///
/// The walk reads no objects. A tree entry states its own type, so descending needs the object
/// only for a directory — a few hundred, against thousands of blobs — and every blob is carried
/// by its id. Loading each blob to ask how big it is cost half a second on a 2400-file
/// workspace, per tree, on a path the UI spends a moment displaying.
fn flatten(repo: &Repository, tree: &Tree<'_>) -> BTreeMap<Vec<u8>, (Oid, i32)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![(tree.clone(), Vec::new())];
    while let Some((tree, prefix)) = stack.pop() {
        for entry in tree.iter() {
            let name = entry.name_bytes().to_vec();
            let mut path = prefix.clone();
            if !path.is_empty() {
                path.push(b'/');
            }
            path.extend_from_slice(&name);
            if entry.kind() == Some(git2::ObjectType::Tree) {
                if let Ok(object) = entry.to_object(repo) {
                    if let Ok(child) = object.into_tree() {
                        stack.push((child, path));
                        continue;
                    }
                }
            }
            out.insert(path, (entry.id(), entry.filemode()));
        }
    }
    out
}

/// The most files a plan will count lines for. Counting is a patch generation per file, and a
/// restore that touches a thousand files should still open instantly; past this the per-file
/// numbers are simply absent, while the totals stay right.
const LINE_BUDGET: usize = 64;

/// How many files a caller should be shown. A workspace can differ from a checkpoint in thousands
/// of files — a branch switch, a refactor, or simply a snapshot taken long ago — and a dialog
/// that renders a row per file is a dialog that stops responding.
///
/// The cap belongs to whoever is *showing* the list, not here: a restore has to write every file
/// the plan names, so the plan itself stays whole and the totals travel beside the rows it kept.
pub const ROW_LIMIT: usize = 200;

/// `current` is the working tree as it stands now, `current_head` the live `HEAD` of the user's
/// repository — read there, not from the checkpoint store, whose own `HEAD` is unborn by design.
pub fn plan(
    repo: &Repository,
    current: Oid,
    current_head: Option<String>,
    snapshot_commit: &Commit<'_>,
    snapshot: Snapshot,
    protect: Option<Oid>,
) -> Result<RestorePlan, CheckpointError> {
    let snapshot_tree = snapshot_commit.tree()?;
    let live_tree = repo.find_tree(current)?;
    let wanted = flatten(repo, &snapshot_tree);
    let live = flatten(repo, &live_tree);
    let floor = match protect {
        Some(oid) => flatten(repo, &repo.find_tree(oid)?),
        None => BTreeMap::new(),
    };

    let mut changed = Vec::new();
    let mut unchanged = 0usize;
    for (path, (oid, _)) in &wanted {
        match live.get(path) {
            Some((live_oid, _)) if live_oid == oid => unchanged += 1,
            Some(_) => changed.push(PathChange {
                path: String::from_utf8_lossy(path).into_owned(),
                change: Change::Overwrite,
                lines: None,
            }),
            None => changed.push(PathChange {
                path: String::from_utf8_lossy(path).into_owned(),
                change: Change::Create,
                lines: None,
            }),
        }
    }
    let mut deletes = live
        .keys()
        .filter(|path| !wanted.contains_key(*path) && !floor.contains_key(*path))
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect::<Vec<_>>();
    deletes.sort();

    let drift = count_and_fill(repo, &snapshot_tree, &live_tree, &mut changed);

    let head_matches = snapshot.head == current_head;
    Ok(RestorePlan {
        writes_total: changed.len(),
        deletes_total: deletes.len(),
        snapshot,
        head_matches,
        current_head,
        writes: changed,
        deletes,
        unchanged,
        drift,
    })
}

/// The change from the previous point in the chain to this one. The chain is what makes this
/// cheap: every snapshot's parent *is* the snapshot before it, so the "previous" side is a commit
/// that already exists and no walk is needed. The first snapshot of a store has no parent, and
/// reports no comparison rather than an empty one.
pub fn step_diff(repo: &Repository, commit: &Commit<'_>) -> Result<StepDiff, CheckpointError> {
    let Ok(previous) = commit.parent(0) else {
        return Ok(StepDiff::default());
    };
    let before = previous.tree()?;
    let after = commit.tree()?;
    let from = flatten(repo, &before);
    let to = flatten(repo, &after);

    let mut files = Vec::new();
    for (path, (oid, _)) in &to {
        match from.get(path) {
            Some((was, _)) if was == oid => {}
            Some(_) => files.push(PathChange {
                path: String::from_utf8_lossy(path).into_owned(),
                change: Change::Overwrite,
                lines: None,
            }),
            None => files.push(PathChange {
                path: String::from_utf8_lossy(path).into_owned(),
                change: Change::Create,
                lines: None,
            }),
        }
    }
    let mut deletions = from
        .keys()
        .filter(|path| !to.contains_key(*path))
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect::<Vec<_>>();
    deletions.sort();

    let drift = count_and_fill(repo, &before, &after, &mut files);
    Ok(StepDiff {
        previous: Some(previous.id().to_string()),
        files_total: files.len(),
        deletions_total: deletions.len(),
        files,
        deletions,
        drift,
    })
}

/// The tree's whole drift, and the per-file counts of the files that make it up, from one diff.
///
/// Two things want the same diff — the total the panel prints and the numbers on the rows beneath
/// it — and a diff over a few thousand files is not free, so it is built once. A binary file has
/// no lines and reports none; so does a file past the budget, which is the difference between "not
/// applicable" and "too expensive to say".
fn count_and_fill(
    repo: &Repository,
    before: &Tree<'_>,
    after: &Tree<'_>,
    rows: &mut [PathChange],
) -> LineStats {
    let Ok(diff) = repo.diff_tree_to_tree(Some(before), Some(after), None) else {
        return LineStats::default();
    };
    let drift = diff.stats().map_or_else(
        |_| LineStats::default(),
        |stats| LineStats {
            insertions: stats.insertions() as u32,
            deletions: stats.deletions() as u32,
        },
    );
    if rows.is_empty() {
        return drift;
    }
    // Owned, so the borrow of `rows` ends before the loop hands it out mutably.
    let wanted = rows
        .iter()
        .take(LINE_BUDGET)
        .map(|change| change.path.clone())
        .collect::<std::collections::HashSet<_>>();

    for (index, delta) in diff.deltas().enumerate() {
        // A path only in the live tree is a delete, not a `PathChange`. A file in both trees, and
        // one a restore would create, both have a new side to read a path from.
        let Some(path) = delta.new_file().path().and_then(|path| path.to_str()) else {
            continue;
        };
        if !wanted.contains(path) {
            continue;
        }
        let Ok(Some(patch)) = git2::Patch::from_diff(&diff, index) else {
            continue;
        };
        let Ok((_, insertions, deletions)) = patch.line_stats() else {
            continue;
        };
        // Zero of both means there was nothing to count: libgit2 answers a binary file's patch
        // with an empty one rather than refusing, and a changed text file always moves at least
        // one line (an unchanged one is not a delta at all). Reporting "0 +0 −0" for an image
        // would be a number nobody should have to read as true.
        if insertions == 0 && deletions == 0 {
            continue;
        }
        if let Some(change) = rows.iter_mut().find(|change| change.path == path) {
            change.lines = Some(LineStats {
                insertions: insertions as u32,
                deletions: deletions as u32,
            });
        }
    }
    drift
}

/// One file as a unified patch. Which two trees it sits between is the caller's question to
/// answer: the browsing view compares the step that ended at this point against the one before it,
/// the restore view compares this point against the workspace as it stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiff {
    pub path: String,
    pub unified: String,
    pub binary: bool,
}

/// Which side of a checkpoint a file's patch is measured against.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Compare {
    /// The step that ended at this point, against the point before it. The default, because it is
    /// what an expanded row is showing.
    #[default]
    Previous,
    /// This point against the working tree as it stands: what restoring would change.
    Workspace,
}

/// The patch for one path. `current` is the working tree, which only the workspace comparison
/// needs — a step is measured between two commits and works without touching the disk. The
/// comparison is between two trees either way: a file deleted since the snapshot still has its
/// old side, and a file the step created has nothing before it.
pub fn file_diff(
    repo: &Repository,
    current: Oid,
    snapshot_commit: &Commit<'_>,
    path: &str,
    compare: Compare,
) -> Result<FileDiff, CheckpointError> {
    let (before, after) = match compare {
        Compare::Previous => match snapshot_commit.parent(0) {
            Ok(previous) => (previous.tree()?, snapshot_commit.tree()?),
            // No earlier point to compare with, which is an empty patch rather than an error: the
            // row is on the first snapshot of the store, and says so in its own view.
            Err(_) => {
                return Ok(FileDiff {
                    path: path.to_string(),
                    unified: String::new(),
                    binary: false,
                });
            }
        },
        Compare::Workspace => (snapshot_commit.tree()?, repo.find_tree(current)?),
    };
    let mut options = git2::DiffOptions::new();
    options.pathspec(path);
    let diff = repo.diff_tree_to_tree(Some(&before), Some(&after), Some(&mut options))?;
    let binary = diff
        .deltas()
        .any(|delta| is_binary(repo, &delta.new_file()) || is_binary(repo, &delta.old_file()));
    let mut unified = String::new();
    diff.print(git2::DiffFormat::Patch, |_delta, _hunk, line| {
        // Context and hunk headers are already part of the line's content; the origin character is
        // not, and a patch without it is a file body.
        if matches!(line.origin(), '+' | '-' | ' ') {
            unified.push(line.origin());
        }
        unified.push_str(&String::from_utf8_lossy(line.content()));
        true
    })?;
    Ok(FileDiff {
        path: path.to_string(),
        unified,
        binary,
    })
}

/// Whether one side of a delta is binary. `DiffFile::is_binary` reads a flag libgit2 only sets
/// once a diff driver has classified the file, and a plain tree-to-tree diff has no driver — so it
/// answers "text" for a PNG. `Blob::is_binary` is the heuristic itself (a NUL byte in the first
/// 8000), and it costs one blob read on a path that is already reading this file's contents.
fn is_binary(repo: &Repository, side: &git2::DiffFile<'_>) -> bool {
    if side.is_binary() {
        return true;
    }
    repo.find_blob(side.id())
        .map(|blob| blob.is_binary())
        .unwrap_or(false)
}

/// How much of a plan a restore may carry out. The defaults are the conservative pair: a file
/// the tree gained after the snapshot is left alone unless deletion is asked for, because
/// "undo my changes" and "delete what I made since" are different requests and only one of them
/// is what the user usually means.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreOptions {
    /// Restore across a `HEAD` that has moved since the snapshot.
    pub cross_head: bool,
    /// Remove the files the working tree gained after the snapshot.
    pub delete_new: bool,
    /// Restore one path instead of the whole tree, by a path relative to the repository root.
    pub only: Option<String>,
}

pub fn apply(
    repo: &Repository,
    plan: RestorePlan,
    options: RestoreOptions,
) -> Result<RestoreOutcome, CheckpointError> {
    let commit = repo.find_commit(Oid::from_str(&plan.snapshot.id)?)?;
    let tree = commit.tree()?;
    let root = repo
        .workdir()
        .ok_or_else(|| {
            CheckpointError::NotARepository("the checkpoint store has no workdir".into())
        })?
        .to_path_buf();

    let mut outcome = RestoreOutcome {
        written: 0,
        deleted: 0,
        failed: Vec::new(),
        guard: None,
    };
    match &options.only {
        Some(only) => {
            if !is_safe(only) {
                return Err(CheckpointError::Refused(only.clone()));
            }
            match restore_one(repo, &tree, &root, only) {
                Ok(()) => outcome.written = 1,
                Err(error) => outcome.failed.push(RestoreFailure {
                    path: only.clone(),
                    error,
                }),
            }
        }
        None => {
            for change in &plan.writes {
                match restore_one(repo, &tree, &root, &change.path) {
                    Ok(()) => outcome.written += 1,
                    Err(error) => outcome.failed.push(RestoreFailure {
                        path: change.path.clone(),
                        error,
                    }),
                }
            }
            if options.delete_new {
                for path in &plan.deletes {
                    match std::fs::remove_file(root.join(path)) {
                        Ok(()) => outcome.deleted += 1,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => outcome.failed.push(RestoreFailure {
                            path: path.clone(),
                            error: error.to_string(),
                        }),
                    }
                }
            }
        }
    }
    Ok(outcome)
}

fn restore_one(repo: &Repository, tree: &Tree<'_>, root: &Path, path: &str) -> Result<(), String> {
    if !is_safe(path) {
        return Err("refused: the snapshot names a path outside the tree".into());
    }
    let entry = tree
        .get_path(Path::new(path))
        .map_err(|error| error.to_string())?;
    let blob = repo
        .find_blob(entry.id())
        .map_err(|error| error.to_string())?;
    let target = root.join(path);
    if let Some(parent) = target.parent() {
        // A directory in the snapshot where a file stands now cannot be created until the file
        // goes; the reverse is handled by the delete pass.
        if parent.is_file() {
            std::fs::remove_file(parent).map_err(|error| error.to_string())?;
        }
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    if entry.filemode() & 0o170000 == 0o120000 {
        if target.exists() || target.is_symlink() {
            std::fs::remove_file(&target).map_err(|error| error.to_string())?;
        }
        let link = String::from_utf8_lossy(blob.content()).into_owned();
        symlink(&link, &target)?;
        return Ok(());
    }
    // Write beside the target and rename over it: a restore that fails half way leaves the
    // previous content recoverable, and a case-only rename on a case-insensitive filesystem
    // works, which an in-place write would not. The scratch file has a name of its own rather
    // than a suffix of the target's, because the tree may contain that name too.
    let temporary = match parent_of(&target) {
        Some(parent) => tempfile::Builder::new()
            .prefix(".zlogic-restore-")
            .tempfile_in(parent),
        None => return Err("the path has no parent directory".into()),
    }
    .map_err(|error| error.to_string())?;
    std::fs::write(temporary.path(), blob.content()).map_err(|error| error.to_string())?;
    if target.exists() {
        std::fs::remove_file(&target).map_err(|error| error.to_string())?;
    }
    temporary
        .persist(&target)
        .map_err(|error| error.to_string())?;
    #[cfg(unix)]
    if entry.filemode() & 0o170000 == 0o100755 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn parent_of(target: &Path) -> Option<&Path> {
    target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
}

fn is_safe(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == ".." || part == ".git")
}

#[cfg(unix)]
fn symlink(link: &str, target: &Path) -> Result<(), String> {
    std::os::unix::fs::symlink(link, target).map_err(|error| error.to_string())
}

#[cfg(windows)]
fn symlink(_link: &str, _target: &Path) -> Result<(), String> {
    Err("symlinks cannot be restored without developer mode or elevation".into())
}
