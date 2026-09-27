use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use zlogic_core::SharedStore;
use zlogic_objects::RepoFacts;
use zlogic_protocol::SessionId;
use zlogic_tools::{ExitAction, ExitedWorktree, WorktreeChanges, WorktreeHost, WorktreeState};

const NAME_ATTEMPTS: u32 = 20;

pub struct Worktrees {
    store: SharedStore,
    template: String,
}

impl Worktrees {
    pub fn new(store: SharedStore, template: impl Into<String>) -> Self {
        Self {
            store,
            template: template.into(),
        }
    }

    pub fn host(&self, session_id: SessionId, root: impl Into<PathBuf>) -> Arc<SessionWorktree> {
        let root = PathBuf::from(zlogic_store::normalise(&root.into()));
        Arc::new(SessionWorktree {
            store: self.store.clone(),
            session_id,
            dir: resolve_dir(&self.template, &root),
            root,
        })
    }
}

fn resolve_dir(template: &str, root: &Path) -> PathBuf {
    let name = root
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let expanded = template.replace("{workspace}", &name);

    let path = if let Some(rest) = expanded.strip_prefix("~/") {
        match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(rest),
            None => PathBuf::from(&expanded),
        }
    } else {
        PathBuf::from(&expanded)
    };

    if path.is_absolute() {
        normalize(&path)
    } else {
        normalize(&root.join(path))
    }
}

fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

pub struct SessionWorktree {
    store: SharedStore,
    session_id: SessionId,
    root: PathBuf,
    dir: PathBuf,
}

impl SessionWorktree {
    fn deviation(&self) -> Option<PathBuf> {
        let record = self
            .store
            .with(|db| db.sessions().get(self.session_id))
            .ok()?;
        record.exec_cwd.map(PathBuf::from)
    }

    fn main_repo(&self, from: &Path) -> Option<PathBuf> {
        RepoFacts::main_repo_root(from)
    }

    fn flatten(name: &str) -> String {
        name.replace('/', "-")
    }

    fn generated_name(&self) -> String {
        let short: String = self.session_id.to_string().chars().take(8).collect();
        let base = format!("session-{short}");
        if !self.dir.join(&base).exists() {
            return base;
        }
        for n in 2..=NAME_ATTEMPTS {
            let candidate = format!("{base}-{n}");
            if !self.dir.join(Self::flatten(&candidate)).exists() {
                return candidate;
            }
        }
        base
    }
}

#[async_trait]
impl WorktreeHost for SessionWorktree {
    async fn current(&self) -> Option<WorktreeState> {
        let path = self.deviation()?;
        let name = path
            .strip_prefix(&self.dir)
            .ok()?
            .to_string_lossy()
            .to_string();
        if name.is_empty() {
            return None;
        }
        let facts = RepoFacts::discover(&path);
        Some(WorktreeState {
            name,
            path,
            branch: facts.branch,
            base_dir: self.root.clone(),
            notes: Vec::new(),
        })
    }

    async fn enter(&self, name: Option<&str>) -> Result<WorktreeState, String> {
        if let Some(existing) = self.current().await {
            return Err(format!(
                "already inside worktree {}",
                existing.path.display()
            ));
        }

        let from = self.deviation().unwrap_or_else(|| self.root.clone());
        let main_repo = self.main_repo(&from).ok_or_else(|| {
            format!(
                "{} is not inside a git repository; cannot create a worktree",
                from.display()
            )
        })?;

        let branch = name
            .map(str::to_string)
            .unwrap_or_else(|| self.generated_name());
        let slug = Self::flatten(&branch);
        let path = self.dir.join(&slug);

        let dir = self.dir.clone();
        let (created, notes) = tokio::task::spawn_blocking({
            let branch = branch.clone();
            let slug = slug.clone();
            let path = path.clone();
            move || create_worktree(&main_repo, &dir, &slug, &branch, &path)
        })
        .await
        .map_err(|e| format!("the worktree-creation task did not finish: {e}"))??;

        let key = zlogic_store::normalise(&created);
        self.store
            .with(|db| db.sessions().set_exec_cwd(self.session_id, &key))
            .map_err(|e| {
                format!(
                    "worktree created ({}), but the session directory could not be updated: {e}",
                    created.display()
                )
            })?;

        Ok(WorktreeState {
            name: slug,
            path: PathBuf::from(key),
            branch: Some(branch),
            base_dir: self.root.clone(),
            notes,
        })
    }

    async fn changes(&self) -> Option<WorktreeChanges> {
        let state = self.current().await?;
        let main_repo = self.main_repo(&state.path)?;
        let path = state.path.clone();
        tokio::task::spawn_blocking(move || count_changes(&main_repo, &path))
            .await
            .ok()?
    }

    async fn exit(&self, action: ExitAction) -> Result<ExitedWorktree, String> {
        let state = self
            .current()
            .await
            .ok_or("this session is not inside a worktree")?;

        if action == ExitAction::Remove {
            let main_repo = self.main_repo(&state.path).ok_or_else(|| {
                format!("{} is not inside a git repository", state.path.display())
            })?;
            let path = state.path.clone();
            let branch = state.branch.clone();
            tokio::task::spawn_blocking(move || {
                remove_worktree(&main_repo, &path, branch.as_deref())
            })
            .await
            .map_err(|e| format!("the worktree-removal task did not finish: {e}"))??;
        }

        self.store.with(|db| db.sessions().clear_exec_cwd(self.session_id)).map_err(|e| {
            if action == ExitAction::Remove {
                format!(
                    "worktree {} was removed, but the session directory could not be reset: {e}. \
                     The next tool call will point at a directory that no longer exists; \
                     reopen the session",
                    state.path.display()
                )
            } else {
                format!("the session directory could not be reset: {e}")
            }
        })?;

        Ok(ExitedWorktree {
            path: state.path,
            branch: state.branch,
            base_dir: self.root.clone(),
            removed: action == ExitAction::Remove,
        })
    }
}

fn create_worktree(
    main_repo: &Path,
    dir: &Path,
    slug: &str,
    branch: &str,
    path: &Path,
) -> Result<(PathBuf, Vec<String>), String> {
    if path.exists() {
        return Err(format!(
            "{} already exists; pick a different name",
            path.display()
        ));
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("failed to create {}: {e}", dir.display()))?;

    let repo = git2::Repository::open(main_repo)
        .map_err(|e| format!("failed to open repository {}: {e}", main_repo.display()))?;

    let head = repo.head().and_then(|h| h.peel_to_commit()).map_err(|_| {
        "the repository has no commits yet; there is no base for the worktree to start from"
            .to_string()
    })?;

    if repo.find_branch(branch, git2::BranchType::Local).is_ok() {
        return Err(format!(
            "branch {branch} already exists; pick a different name"
        ));
    }
    let created = repo
        .branch(branch, &head, false)
        .map_err(|e| format!("failed to create branch {branch}: {e}"))?;
    let reference = created.into_reference();

    let mut opts = git2::WorktreeAddOptions::new();
    opts.reference(Some(&reference));
    let worktree = match repo.worktree(slug, path, Some(&opts)) {
        Ok(w) => w,
        Err(e) => {
            if let Ok(mut b) = repo.find_branch(branch, git2::BranchType::Local) {
                let _ = b.delete();
            }
            let _ = std::fs::remove_dir_all(path);
            return Err(format!("failed to create worktree: {e}"));
        }
    };

    let landed = worktree.path().to_path_buf();

    let mut notes = seed_local_files(main_repo, &landed);
    notes.extend(seed_included_files(&repo, main_repo, &landed));
    Ok((landed, notes))
}

const LOCAL_FILES: &[&str] = &[
    ".zlogic/settings.yaml",
    ".zlogic/policy.yaml",
    ".mcp.json",
    ".zlogic/mcp.json",
];

fn seed_local_files(main_repo: &Path, worktree: &Path) -> Vec<String> {
    let mut copied = Vec::new();
    let mut failed = Vec::new();
    for rel in LOCAL_FILES {
        let src = main_repo.join(rel);
        let dst = worktree.join(rel);
        if !src.is_file() || dst.exists() {
            continue;
        }
        match copy_file(&src, &dst) {
            Ok(()) => copied.push(*rel),
            Err(e) => failed.push(format!("{rel} ({e})")),
        }
    }

    let mut notes = Vec::new();
    if !copied.is_empty() {
        notes.push(format!(
            "Carried over {} local file(s) that git does not track: {}.",
            copied.len(),
            copied.join(", ")
        ));
    }
    if !failed.is_empty() {
        notes.push(format!(
            "Could not copy: {}. The worktree may behave differently from the main directory.",
            failed.join(", ")
        ));
    }
    notes
}

const INCLUDE_FILE: &str = ".worktreeinclude";

const MAX_INCLUDE_FILES: usize = 500;
const MAX_INCLUDE_BYTES: u64 = 64 * 1024 * 1024;

fn seed_included_files(repo: &git2::Repository, main_repo: &Path, worktree: &Path) -> Vec<String> {
    let text = match std::fs::read_to_string(main_repo.join(INCLUDE_FILE)) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };

    let mut builder = ignore::gitignore::GitignoreBuilder::new(main_repo);
    let mut bad = Vec::new();
    let mut literal_prefixes = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Err(e) = builder.add_line(None, line) {
            bad.push(format!("{line:?} ({e})"));
            continue;
        }
        if let Some(prefix) = literal_dir_prefix(line) {
            literal_prefixes.push(prefix);
        }
    }
    let include = match builder.build() {
        Ok(i) => i,
        Err(e) => return vec![format!("Could not read {INCLUDE_FILE}: {e}")],
    };

    let mut budget = Budget {
        files: MAX_INCLUDE_FILES,
        bytes: MAX_INCLUDE_BYTES,
        hit: false,
    };
    let mut copied = 0usize;

    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true)
        .include_ignored(true)
        .include_unmodified(false)
        .recurse_untracked_dirs(false)
        .recurse_ignored_dirs(false);
    let statuses = match repo.statuses(Some(&mut opts)) {
        Ok(s) => s,
        Err(e) => return vec![format!("Could not list ignored files: {e}")],
    };

    for entry in statuses.iter() {
        if !entry.status().contains(git2::Status::IGNORED) {
            continue;
        }
        let Some(rel) = entry.path() else { continue };
        let is_dir = rel.ends_with('/');
        let rel = rel.trim_end_matches('/');
        if rel.is_empty() || rel == ".git" {
            continue;
        }
        let src = main_repo.join(rel);

        let dst = worktree.join(rel);
        if include
            .matched_path_or_any_parents(Path::new(rel), is_dir)
            .is_ignore()
        {
            copied += match is_dir {
                true => copy_tree(&src, &dst, None, &mut budget),
                false => copy_one(&src, &dst, &mut budget) as usize,
            };
        } else if is_dir
            && literal_prefixes
                .iter()
                .any(|p| p.starts_with(&format!("{rel}/")))
        {
            copied += copy_tree(&src, &dst, Some((&include, Path::new(rel))), &mut budget);
        }
        if budget.hit {
            break;
        }
    }

    let mut notes = Vec::new();
    if copied > 0 {
        notes.push(format!(
            "Carried over {copied} gitignored file(s) selected by {INCLUDE_FILE}."
        ));
    }
    if budget.hit {
        notes.push(format!(
            "Stopped at the {INCLUDE_FILE} limit ({MAX_INCLUDE_FILES} files / \
             {}MB) — some files were not copied. Narrow the patterns, or symlink large \
             directories yourself.",
            MAX_INCLUDE_BYTES / (1024 * 1024)
        ));
    }
    if !bad.is_empty() {
        notes.push(format!(
            "Ignored unusable {INCLUDE_FILE} line(s): {}.",
            bad.join(", ")
        ));
    }
    notes
}

struct Budget {
    files: usize,
    bytes: u64,
    hit: bool,
}

fn copy_one(src: &Path, dst: &Path, budget: &mut Budget) -> bool {
    if budget.hit || dst.exists() || !src.is_file() {
        return false;
    }
    let size = src.metadata().map(|m| m.len()).unwrap_or(0);
    if budget.files == 0 || size > budget.bytes {
        budget.hit = true;
        return false;
    }
    match copy_file(src, dst) {
        Ok(()) => {
            budget.files -= 1;
            budget.bytes -= size;
            true
        }
        Err(_) => false,
    }
}

fn copy_tree(
    src: &Path,
    dst: &Path,
    filter: Option<(&ignore::gitignore::Gitignore, &Path)>,
    budget: &mut Budget,
) -> usize {
    let Ok(entries) = std::fs::read_dir(src) else {
        return 0;
    };
    let mut copied = 0;
    for entry in entries.flatten() {
        if budget.hit {
            break;
        }
        let path = entry.path();
        let child_dst = dst.join(entry.file_name());
        let child_rel = filter.map(|(_, rel)| rel.join(entry.file_name()));

        if path.is_dir() {
            let child_filter = match (filter, &child_rel) {
                (Some((include, _)), Some(rel)) => Some((include, rel.as_path())),
                _ => None,
            };
            copied += copy_tree(&path, &child_dst, child_filter, budget);
            continue;
        }
        let selected = match (filter, &child_rel) {
            (Some((include, _)), Some(rel)) => {
                include.matched_path_or_any_parents(rel, false).is_ignore()
            }
            _ => true,
        };
        if selected && copy_one(&path, &child_dst, budget) {
            copied += 1;
        }
    }
    copied
}

fn copy_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(src, dst)?;
    Ok(())
}

fn literal_dir_prefix(pattern: &str) -> Option<String> {
    let pattern = pattern.trim_start_matches('!').trim_start_matches('/');
    let head: String = pattern
        .chars()
        .take_while(|c| !matches!(c, '*' | '?' | '['))
        .collect();
    let cut = head.rfind('/')?;
    Some(head[..=cut].to_string())
}

fn count_changes(main_repo: &Path, path: &Path) -> Option<WorktreeChanges> {
    let repo = git2::Repository::open(path).ok()?;

    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(false)
        .include_ignored(false)
        .include_unmodified(false);
    let changed_files = repo.statuses(Some(&mut opts)).ok()?.len() as u32;

    let head = repo.head().ok()?.peel_to_commit().ok()?.id();
    let main = git2::Repository::open(main_repo).ok()?;
    let main_head = main.head().ok()?.peel_to_commit().ok()?.id();
    let commits = if head == main_head {
        0
    } else {
        let base = repo.merge_base(head, main_head).ok()?;
        let mut walk = repo.revwalk().ok()?;
        walk.push(head).ok()?;
        walk.hide(base).ok()?;
        walk.count() as u32
    };

    Some(WorktreeChanges {
        changed_files,
        commits,
    })
}

fn remove_worktree(main_repo: &Path, path: &Path, branch: Option<&str>) -> Result<(), String> {
    let repo = git2::Repository::open(main_repo)
        .map_err(|e| format!("failed to open repository {}: {e}", main_repo.display()))?;

    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if let Ok(names) = repo.worktrees() {
        for name in names.iter().flatten() {
            let Ok(worktree) = repo.find_worktree(name) else {
                continue;
            };
            let registered = std::fs::canonicalize(worktree.path())
                .unwrap_or_else(|_| worktree.path().to_path_buf());
            if registered != target {
                continue;
            }
            let mut opts = git2::WorktreePruneOptions::new();
            opts.valid(true).working_tree(true);
            worktree
                .prune(Some(&mut opts))
                .map_err(|e| format!("failed to prune the worktree registration: {e}"))?;
            break;
        }
    }

    if target.exists() {
        std::fs::remove_dir_all(&target)
            .map_err(|e| format!("failed to remove {}: {e}", target.display()))?;
    }

    if let Some(branch) = branch
        && let Ok(mut b) = repo.find_branch(branch, git2::BranchType::Local)
    {
        if let Err(e) = b.delete() {
            tracing::warn!(target: "zlogic::engine", "failed to delete worktree branch {branch}: {e}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zlogic_store::{Db, NewSession};

    fn repo_with_commit(dir: &Path) {
        let repo = git2::Repository::init(dir).unwrap();
        // A host's `core.autocrlf` decides what a checkout contains — Git for Windows ships `true`
        // by default, and a runner has it — while these tests are about which version of a file
        // lands in the worktree, not about how the host spells line endings.
        let mut config = repo.config().unwrap();
        config.set_bool("core.autocrlf", false).unwrap();
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("a.txt")).unwrap();
        index.write().unwrap();
        let tree = index.write_tree().unwrap();
        let tree = repo.find_tree(tree).unwrap();
        let sig = git2::Signature::now("t", "t@test").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        dir: PathBuf,
        store: SharedStore,
        session_id: SessionId,
        host: Arc<SessionWorktree>,
    }

    fn fixture(with_commit: bool) -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let base = PathBuf::from(zlogic_store::normalise(tmp.path()));
        let root = base.join("repo");
        std::fs::create_dir_all(&root).unwrap();
        if with_commit {
            repo_with_commit(&root);
        }

        let store = SharedStore::new(Db::open_in_memory().unwrap());
        let session = store
            .with(|db| {
                db.sessions()
                    .create(NewSession::root(zlogic_protocol::WorkspaceId::new()))
            })
            .unwrap();

        let worktrees = Worktrees::new(store.clone(), "../{workspace}-worktrees");
        let host = worktrees.host(session.session_id, root.clone());
        Fixture {
            _tmp: tmp,
            dir: base.join("repo-worktrees"),
            root,
            store,
            session_id: session.session_id,
            host,
        }
    }

    fn exec_cwd(f: &Fixture) -> Option<String> {
        f.store
            .with(|db| db.sessions().get(f.session_id))
            .unwrap()
            .exec_cwd
    }

    #[tokio::test]
    async fn entering_moves_the_session_and_leaving_moves_it_back() {
        let f = fixture(true);
        assert!(f.host.current().await.is_none(), "we start out at the root");

        let state = f.host.enter(Some("login-retry")).await.unwrap();
        assert!(
            state.path.join(".git").exists(),
            "the checkout was not created: {}",
            state.path.display()
        );
        assert_eq!(
            state.path,
            f.dir.join("login-retry"),
            "by default it lands in a sibling directory of the repository"
        );
        assert!(
            !state.path.starts_with(&f.root),
            "not one byte is written into the user's working tree"
        );
        assert_eq!(state.branch.as_deref(), Some("login-retry"));
        assert_eq!(state.base_dir, f.root);
        assert_eq!(
            exec_cwd(&f).as_deref(),
            Some(state.path.to_string_lossy().as_ref())
        );
        assert_eq!(f.host.current().await.unwrap().path, state.path);

        let exited = f.host.exit(ExitAction::Keep).await.unwrap();
        assert!(!exited.removed);
        assert!(
            exited.path.exists(),
            "after keep the checkout is still there"
        );
        assert_eq!(exec_cwd(&f), None, "cleared, not set to the root path");
        assert!(f.host.current().await.is_none());
    }

    #[tokio::test]
    async fn a_deviation_we_did_not_create_is_not_reported_as_ours() {
        let f = fixture(true);
        let elsewhere = f.root.join("somewhere-else");
        std::fs::create_dir_all(&elsewhere).unwrap();
        f.store
            .with(|db| {
                db.sessions()
                    .set_exec_cwd(f.session_id, elsewhere.to_string_lossy().as_ref())
            })
            .unwrap();

        assert!(
            f.host.current().await.is_none(),
            "we did not create it, so it must not show up here"
        );
        assert!(
            f.host.exit(ExitAction::Remove).await.is_err(),
            "so it cannot be deleted either"
        );
        assert!(elsewhere.exists());
    }

    #[tokio::test]
    async fn removing_deletes_the_checkout_and_its_branch() {
        let f = fixture(true);
        let state = f.host.enter(Some("scratch")).await.unwrap();

        let exited = f.host.exit(ExitAction::Remove).await.unwrap();
        assert!(exited.removed);
        assert!(!state.path.exists(), "the checkout should be gone");
        assert_eq!(exec_cwd(&f), None);

        let repo = git2::Repository::open(&f.root).unwrap();
        assert!(
            repo.find_branch("scratch", git2::BranchType::Local)
                .is_err(),
            "the branch should be gone too"
        );
        assert!(
            repo.worktrees().unwrap().iter().flatten().count() == 0,
            "the registration is cleaned up as well"
        );
    }

    #[tokio::test]
    async fn changes_counts_uncommitted_files() {
        let f = fixture(true);
        let state = f.host.enter(Some("wip")).await.unwrap();
        assert_eq!(
            f.host.changes().await,
            Some(WorktreeChanges {
                changed_files: 0,
                commits: 0
            }),
            "freshly created, so it is clean"
        );

        std::fs::write(state.path.join("a.txt"), "changed").unwrap();
        std::fs::write(state.path.join("new.txt"), "new").unwrap();
        let changes = f.host.changes().await.unwrap();
        assert_eq!(changes.changed_files, 2, "one modified, one untracked");
        assert_eq!(changes.commits, 0);
        assert!(!changes.is_clean());
    }

    #[tokio::test]
    async fn changes_outside_a_worktree_is_unknown_not_clean() {
        let f = fixture(true);
        assert_eq!(f.host.changes().await, None);
    }

    #[tokio::test]
    async fn entering_twice_is_refused() {
        let f = fixture(true);
        f.host.enter(Some("first")).await.unwrap();
        assert!(f.host.enter(Some("second")).await.is_err());
    }

    #[tokio::test]
    async fn a_taken_name_is_refused_rather_than_reused() {
        let f = fixture(true);
        f.host.enter(Some("dup")).await.unwrap();
        f.host.exit(ExitAction::Keep).await.unwrap();

        let err = f.host.enter(Some("dup")).await.unwrap_err();
        assert!(err.contains("already exists"), "{err}");
        assert_eq!(
            exec_cwd(&f),
            None,
            "a failed creation must not move the session"
        );
    }

    #[tokio::test]
    async fn a_directory_without_git_says_so() {
        let f = fixture(false);
        let err = f.host.enter(None).await.unwrap_err();
        assert!(err.contains("git"), "{err}");
        assert_eq!(exec_cwd(&f), None);
    }

    #[tokio::test]
    async fn a_repository_without_commits_is_refused() {
        let f = fixture(false);
        git2::Repository::init(&f.root).unwrap();
        let err = f.host.enter(None).await.unwrap_err();
        assert!(err.contains("commit"), "{err}");
    }

    #[tokio::test]
    async fn a_generated_name_is_derived_from_the_session() {
        let f = fixture(true);
        let state = f.host.enter(None).await.unwrap();
        assert!(state.name.starts_with("session-"), "{}", state.name);
        assert!(
            f.session_id
                .to_string()
                .starts_with(&state.name["session-".len()..])
        );
    }

    #[tokio::test]
    async fn local_files_are_carried_into_a_fresh_checkout() {
        let f = fixture(true);
        std::fs::create_dir_all(f.root.join(".zlogic")).unwrap();
        std::fs::write(f.root.join(".zlogic/settings.yaml"), "locale: zh-CN\n").unwrap();
        std::fs::write(
            f.root.join(".zlogic/policy.yaml"),
            "policy: { version: 1, default: ask }\n",
        )
        .unwrap();

        let state = f.host.enter(Some("seeded")).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(state.path.join(".zlogic/settings.yaml")).unwrap(),
            "locale: zh-CN\n"
        );
        assert!(state.path.join(".zlogic/policy.yaml").is_file());
        assert!(
            state
                .notes
                .iter()
                .any(|n| n.contains(".zlogic/settings.yaml")),
            "{:?}",
            state.notes
        );
    }

    #[tokio::test]
    async fn a_tracked_file_is_not_overwritten_by_the_working_copy() {
        let f = fixture(true);
        let repo = git2::Repository::open(&f.root).unwrap();
        std::fs::create_dir_all(f.root.join(".zlogic")).unwrap();
        std::fs::write(f.root.join(".zlogic/settings.yaml"), "committed\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new(".zlogic/settings.yaml")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("t", "t@test").unwrap();
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "add settings", &tree, &[&head])
            .unwrap();
        std::fs::write(f.root.join(".zlogic/settings.yaml"), "uncommitted\n").unwrap();

        let state = f.host.enter(Some("tracked")).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(state.path.join(".zlogic/settings.yaml")).unwrap(),
            "committed\n",
            "the checkout should hold this branch's version, not a dirty copy from the main working tree"
        );
        assert!(
            state.notes.is_empty(),
            "nothing was copied, so there should be no note: {:?}",
            state.notes
        );
    }

    #[tokio::test]
    async fn worktreeinclude_carries_the_selected_ignored_files_only() {
        let f = fixture(true);
        std::fs::write(f.root.join(".gitignore"), ".env*\nsecrets/\nbuild/\n").unwrap();
        std::fs::write(f.root.join(INCLUDE_FILE), ".env\nsecrets/**\n").unwrap();
        std::fs::write(f.root.join(".env"), "TOKEN=1\n").unwrap();
        std::fs::write(f.root.join(".env.backup"), "old\n").unwrap();
        std::fs::create_dir_all(f.root.join("secrets")).unwrap();
        std::fs::write(f.root.join("secrets/key.pem"), "k\n").unwrap();
        std::fs::create_dir_all(f.root.join("build")).unwrap();
        std::fs::write(f.root.join("build/artifact.bin"), "big\n").unwrap();

        let state = f.host.enter(Some("inc")).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(state.path.join(".env")).unwrap(),
            "TOKEN=1\n"
        );
        assert!(
            state.path.join("secrets/key.pem").is_file(),
            "a directory pattern should recurse into it"
        );
        assert!(
            !state.path.join(".env.backup").exists(),
            "an ignored file that was not selected must not come along"
        );
        assert!(
            !state.path.join("build").exists(),
            "a whole build output directory even less so"
        );
        assert!(
            state.notes.iter().any(|n| n.contains(INCLUDE_FILE)),
            "{:?}",
            state.notes
        );
    }

    #[tokio::test]
    async fn no_include_file_means_nothing_to_say() {
        let f = fixture(true);
        std::fs::write(f.root.join(".gitignore"), ".env\n").unwrap();
        std::fs::write(f.root.join(".env"), "x\n").unwrap();

        let state = f.host.enter(Some("plain")).await.unwrap();
        assert!(
            !state.path.join(".env").exists(),
            "no include file means nothing is carried over"
        );
        assert!(state.notes.is_empty(), "{:?}", state.notes);
    }

    #[test]
    fn only_a_pattern_with_a_directory_prefix_reaches_into_a_collapsed_dir() {
        assert_eq!(
            literal_dir_prefix("config/local/*.env"),
            Some("config/local/".into())
        );
        assert_eq!(
            literal_dir_prefix("/vendor/cache/x"),
            Some("vendor/cache/".into())
        );
        assert_eq!(
            literal_dir_prefix("!config/skip/a"),
            Some("config/skip/".into())
        );
        assert_eq!(literal_dir_prefix(".env"), None);
        assert_eq!(literal_dir_prefix("**/.env"), None);
        assert_eq!(literal_dir_prefix("*.local"), None);
    }

    #[tokio::test]
    async fn hitting_the_copy_limit_is_reported() {
        let f = fixture(true);
        std::fs::write(f.root.join(".gitignore"), "blob/\n").unwrap();
        std::fs::write(f.root.join(INCLUDE_FILE), "blob/**\n").unwrap();
        std::fs::create_dir_all(f.root.join("blob")).unwrap();
        for i in 0..(MAX_INCLUDE_FILES + 10) {
            std::fs::write(f.root.join("blob").join(format!("f{i}.txt")), "x").unwrap();
        }

        let state = f.host.enter(Some("capped")).await.unwrap();
        let copied = std::fs::read_dir(state.path.join("blob")).unwrap().count();
        assert!(
            copied <= MAX_INCLUDE_FILES,
            "carried over {copied}, which is over the limit"
        );
        assert!(
            state.notes.iter().any(|n| n.contains("limit")),
            "hitting the limit must leave a trace: {:?}",
            state.notes
        );
    }

    #[test]
    fn the_directory_template_expands_workspace_home_and_relative_paths() {
        let root = Path::new("/home/u/code/zlogic");

        assert_eq!(
            resolve_dir("../{workspace}-worktrees", root),
            PathBuf::from("/home/u/code/zlogic-worktrees")
        );
        assert_eq!(
            resolve_dir(".zlogic/worktrees", root),
            PathBuf::from("/home/u/code/zlogic/.zlogic/worktrees")
        );
        assert_eq!(
            resolve_dir("/tmp/wt/{workspace}", root),
            PathBuf::from("/tmp/wt/zlogic")
        );
        if let Some(home) = std::env::var_os("HOME") {
            assert_eq!(
                resolve_dir("~/worktrees/{workspace}", root),
                PathBuf::from(home).join("worktrees/zlogic")
            );
        }
    }

    #[tokio::test]
    async fn a_configured_directory_is_where_the_checkout_lands() {
        let f = fixture(true);
        let elsewhere = f.root.parent().unwrap().join("pool");
        let worktrees = Worktrees::new(f.store.clone(), elsewhere.to_string_lossy().to_string());
        let host = worktrees.host(f.session_id, f.root.clone());

        let state = host.enter(Some("custom")).await.unwrap();
        assert_eq!(state.path, elsewhere.join("custom"));
        assert_eq!(
            host.current().await.unwrap().name,
            "custom",
            "the scope is computed from the configured home directory"
        );
    }

    #[tokio::test]
    async fn a_slashed_branch_name_gets_a_flat_directory() {
        let f = fixture(true);
        let state = f.host.enter(Some("feature/login")).await.unwrap();
        assert_eq!(state.branch.as_deref(), Some("feature/login"));
        assert_eq!(state.name, "feature-login");
        assert!(
            state.path.ends_with("feature-login"),
            "{}",
            state.path.display()
        );
    }
}
