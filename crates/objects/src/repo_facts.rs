use std::path::{Path, PathBuf};

use git2::{ErrorCode, Repository};

/// A repository that is here but that this process is not allowed to open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoRefusal {
    /// The `.git` directory is not owned by the user running this process, so libgit2 refuses it.
    /// Nothing about the repository is broken, and `git` the CLI lets an administrator in where
    /// libgit2 does not, so this says what *this* process may do, not what the user may do.
    OwnerMismatch,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct RepoFacts {
    pub is_repo: bool,
    /// Set when discovery met a repository and refused it, as distinct from meeting no repository
    /// at all: the first is a state to report, the second is the normal answer for a plain folder.
    pub refusal: Option<RepoRefusal>,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub common_dir: Option<PathBuf>,
    pub is_worktree: bool,
}

impl RepoFacts {
    pub fn discover(dir: &Path) -> Self {
        let repo = match Repository::discover(dir) {
            Ok(repo) => repo,
            Err(error) => {
                return Self {
                    refusal: refusal_of(&error),
                    ..Self::default()
                }
            }
        };
        let head = repo.head().ok();
        let branch = match head.as_ref() {
            Some(head) if head.is_branch() => head.shorthand().map(str::to_string),
            Some(_) => None,
            // A repository with no commits has no `HEAD` object to read, but it is on a branch all
            // the same — and git treats that branch as real: `git switch -c other` moves onto it.
            // Reporting no branch here is what let a branch creation look like it had worked: the
            // panel had no name to show and nothing to compare the typed one against, while
            // `git switch -c <the name it already has>` exits 0 having changed nothing at all.
            None => unborn_branch(&repo),
        };
        let sha = head
            .as_ref()
            .and_then(|h| h.target())
            .map(|o| o.to_string());

        Self {
            is_repo: true,
            refusal: None,
            branch,
            head: sha,
            common_dir: Some(repo.commondir().to_path_buf()),
            is_worktree: repo.commondir() != repo.path(),
        }
    }

    pub fn branch_key(&self) -> Option<String> {
        match (&self.branch, &self.head) {
            (Some(b), _) => Some(b.clone()),
            (None, Some(sha)) => Some(format!("detached:{}", &sha[..sha.len().min(12)])),
            _ => None,
        }
    }

    pub fn work_dir(dir: &Path) -> Option<PathBuf> {
        Repository::discover(dir)
            .ok()?
            .workdir()
            .map(Path::to_path_buf)
    }

    pub fn main_repo_root(dir: &Path) -> Option<PathBuf> {
        let repo = Repository::discover(dir).ok()?;
        if repo.is_bare() {
            return None;
        }
        repo.commondir().parent().map(Path::to_path_buf)
    }
}

/// Why libgit2 would not open what it found. Every other failure — no `.git` above the path, a
/// directory that is not one, a corrupt file — means there is no repository to report, which is
/// the plain default and needs no name of its own.
fn refusal_of(error: &git2::Error) -> Option<RepoRefusal> {
    match error.code() {
        ErrorCode::Owner => Some(RepoRefusal::OwnerMismatch),
        _ => None,
    }
}

/// The branch a repository is on before it has any commits: `HEAD` is a symbolic ref to a branch
/// that does not exist yet, so `Repository::head` has nothing to resolve and the name has to come
/// from the ref itself.
fn unborn_branch(repo: &Repository) -> Option<String> {
    repo.find_reference("HEAD")
        .ok()?
        .symbolic_target()
        .and_then(|target| target.strip_prefix("refs/heads/"))
        .map(str::to_string)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WorkTreeStatus {
    pub staged: u32,
    pub unstaged: u32,
    pub untracked: u32,
}

impl WorkTreeStatus {
    pub fn read(dir: &Path) -> Self {
        let Ok(repo) = Repository::discover(dir) else {
            return Self::default();
        };
        if repo.is_bare() {
            return Self::default();
        }
        let mut opts = git2::StatusOptions::new();
        opts.include_untracked(true)
            .recurse_untracked_dirs(false)
            .include_ignored(false)
            .include_unmodified(false);
        let Ok(statuses) = repo.statuses(Some(&mut opts)) else {
            return Self::default();
        };

        let mut out = Self::default();
        for entry in statuses.iter() {
            let s = entry.status();
            if s.intersects(
                git2::Status::INDEX_NEW
                    | git2::Status::INDEX_MODIFIED
                    | git2::Status::INDEX_DELETED
                    | git2::Status::INDEX_RENAMED
                    | git2::Status::INDEX_TYPECHANGE,
            ) {
                out.staged += 1;
            }
            if s.intersects(
                git2::Status::WT_MODIFIED
                    | git2::Status::WT_DELETED
                    | git2::Status::WT_RENAMED
                    | git2::Status::WT_TYPECHANGE,
            ) {
                out.unstaged += 1;
            }
            if s.contains(git2::Status::WT_NEW) {
                out.untracked += 1;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_directory_is_not_a_repo_and_that_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let facts = RepoFacts::discover(dir.path());
        assert!(!facts.is_repo);
        assert_eq!(
            facts.branch_key(),
            None,
            "with no git there is no branch key; these rows match any branch"
        );
    }

    #[test]
    fn a_fresh_repo_reports_its_branch_once_there_is_a_commit() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();

        let facts = RepoFacts::discover(dir.path());
        assert!(facts.is_repo);
        assert!(facts.head.is_none());
        // No commit yet, but HEAD names a branch all the same — and `git switch -c <that name>`
        // succeeds without moving, which is only knowable if the name is reported.
        let unborn = facts
            .branch
            .clone()
            .expect("an unborn HEAD still names a branch");
        assert_eq!(facts.branch_key(), Some(unborn.clone()));

        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("a.txt")).unwrap();
        index.write().unwrap();
        let tree = index.write_tree().unwrap();
        let tree = repo.find_tree(tree).unwrap();
        let sig = git2::Signature::now("t", "t@test").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();

        let facts = RepoFacts::discover(dir.path());
        assert!(facts.head.is_some());
        let branch = facts
            .branch
            .clone()
            .expect("once there is a commit there must be a branch name");
        assert_eq!(facts.branch_key(), Some(branch));
        assert!(!facts.is_worktree);
        assert!(
            facts.common_dir.is_some(),
            "workspace normalisation depends on it"
        );
    }

    #[test]
    fn discovery_walks_up_from_a_subdirectory() {
        let dir = tempfile::tempdir().unwrap();
        Repository::init(dir.path()).unwrap();
        let sub = dir.path().join("deep/nested");
        std::fs::create_dir_all(&sub).unwrap();
        assert!(RepoFacts::discover(&sub).is_repo);
    }

    #[test]
    fn a_plain_directory_has_no_status_and_that_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(WorkTreeStatus::read(dir.path()), WorkTreeStatus::default());
    }

    #[test]
    fn status_separates_staged_unstaged_and_untracked() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let write = |name: &str, body: &str| {
            std::fs::write(dir.path().join(name), body).unwrap();
        };

        write("tracked.txt", "v1");
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("tracked.txt")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("t", "t@test").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
        write("tracked.txt", "v2");

        write("staged.txt", "new");
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("staged.txt")).unwrap();
        index.write().unwrap();

        write("loose.txt", "x");

        let status = WorkTreeStatus::read(dir.path());
        assert_eq!(status.staged, 1, "{status:?}");
        assert_eq!(status.unstaged, 1, "{status:?}");
        assert_eq!(status.untracked, 1, "{status:?}");
    }

    #[test]
    fn an_untracked_directory_counts_as_one() {
        let dir = tempfile::tempdir().unwrap();
        Repository::init(dir.path()).unwrap();
        for i in 0..50 {
            let d = dir.path().join("node_modules").join(format!("pkg{i}"));
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("index.js"), "x").unwrap();
        }

        assert_eq!(WorkTreeStatus::read(dir.path()).untracked, 1);
    }

    #[test]
    fn a_file_can_be_both_staged_and_dirty() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        std::fs::write(dir.path().join("a.txt"), "staged").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("a.txt")).unwrap();
        index.write().unwrap();
        std::fs::write(dir.path().join("a.txt"), "then changed").unwrap();

        let status = WorkTreeStatus::read(dir.path());
        assert_eq!(status.staged, 1, "{status:?}");
        assert_eq!(status.unstaged, 1, "{status:?}");
    }
}
