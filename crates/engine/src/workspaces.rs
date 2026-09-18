use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use zlogic_core::SharedStore;
use zlogic_protocol::WorkspaceId;
use zlogic_protocol::query::{
    ApiError, ApiResult, WorkspaceKind, WorkspaceSelector, WorkspaceSummary, WorkspaceToolsUpdate,
    WorkspaceUpdateReq,
};
use zlogic_protocol::stream::{InitPhase, WorkspaceInitProgress};

use crate::hub::EventHub;
use crate::service::WorkspaceService;
use crate::{EngineError, Result};

/// A workspace read slower than this is worth a log line; see [`Workspaces::get`].
/// Normal cost is a sub-millisecond query, so anything at this order is contention or a stall,
/// never the work itself.
const SLOW_WORKSPACE_GET_MS: u64 = 100;

pub struct Workspaces {
    store: SharedStore,
    hub: Option<Arc<EventHub>>,
    chat_dir: Option<PathBuf>,
    /// The chats root resolved to its real path, captured only when it already existed.
    /// [`Workspaces::is_managed`] runs on every workspace read, and asking the filesystem for the
    /// same answer on each of those calls is a syscall on a hot path for a value that cannot
    /// change while the process lives.
    managed_base: Option<PathBuf>,
}

impl Workspaces {
    pub fn new(store: SharedStore) -> Self {
        Self {
            store,
            hub: None,
            chat_dir: None,
            managed_base: None,
        }
    }

    /// Wired by the bootstrap; see [`Workspaces::report_init`].
    pub fn with_hub(mut self, hub: Arc<EventHub>) -> Self {
        self.hub = Some(hub);
        self
    }

    /// Report the open of `root` to the UI as `Started` immediately followed by `Done`.
    ///
    /// The UI keeps "opening workspace" on screen until it sees a terminal phase, so the pair has
    /// to be sent on every open — a re-open and a first registration alike. Nothing is scanned or
    /// checksummed any more, hence the empty result.
    fn report_init(&self, workspace_id: WorkspaceId, root: &Path) {
        let Some(hub) = &self.hub else {
            return;
        };
        let root = root.to_string_lossy().into_owned();
        let progress = |phase| WorkspaceInitProgress {
            workspace_id: workspace_id.to_string(),
            root: root.clone(),
            phase,
        };
        hub.init_progress(progress(InitPhase::Started));
        hub.init_progress(progress(InitPhase::Done {
            files: 0,
            bytes: 0,
            skipped: 0,
            elapsed_ms: 0,
            unchanged: false,
        }));
    }

    pub fn with_chat_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        // Resolve the managed root once, here, instead of on every `is_managed` call.
        // Only a root that already exists is cached: while the directory is missing `normalise`
        // falls back to a lexical form, and caching that could later disagree with the real path
        // of the directory once it has been created under a symlinked or junctioned parent.
        self.managed_base = dir
            .is_dir()
            .then(|| PathBuf::from(zlogic_store::normalise(&dir)));
        self.chat_dir = Some(dir);
        self
    }

    /// The managed root in the same form the stored records carry, resolved at most once.
    /// Falls back to resolving per call, which is what the cache holds when it is populated.
    fn managed_base(&self) -> Option<PathBuf> {
        let base = self.chat_dir.as_ref()?;
        Some(match &self.managed_base {
            Some(resolved) => resolved.clone(),
            None => PathBuf::from(zlogic_store::normalise(base)),
        })
    }

    fn is_managed(&self, record: &zlogic_store::WorkspaceRecord) -> bool {
        self.managed_base()
            .is_some_and(|base| record.as_path().starts_with(base))
    }

    fn lookup_existing(&self, sel: &WorkspaceSelector) -> Result<zlogic_store::WorkspaceRecord> {
        match sel {
            WorkspaceSelector::Id { workspace_id } => self
                .store
                .with(|db| db.workspaces().find(*workspace_id))?
                .ok_or_else(|| EngineError::NotFound(format!("workspace {workspace_id}"))),
            WorkspaceSelector::Path { root } => {
                if root.trim().is_empty() {
                    return Err(EngineError::Invalid(
                        "workspace path must not be empty".into(),
                    ));
                }
                self.store
                    .with(|db| db.workspaces().find_by_path(root))?
                    .ok_or_else(|| EngineError::NotFound(format!("workspace at {root}")))
            }
        }
    }

    fn lookup(
        &self,
        sel: &WorkspaceSelector,
        name: Option<&str>,
        tools: Option<&[String]>,
    ) -> Result<(zlogic_store::WorkspaceRecord, bool)> {
        match sel {
            WorkspaceSelector::Id { workspace_id } => {
                let record = self.store.with(|db| db.workspaces().find(*workspace_id))?;
                record
                    .map(|r| (r, false))
                    .ok_or_else(|| EngineError::NotFound(format!("workspace {workspace_id}")))
            }
            WorkspaceSelector::Path { root } => {
                if root.trim().is_empty() {
                    return Err(EngineError::Invalid(
                        "workspace path must not be empty".into(),
                    ));
                }
                Ok(self
                    .store
                    .with(|db| db.workspaces().resolve_with(root, name, tools))?)
            }
        }
    }

    pub fn root_of(&self, workspace_id: WorkspaceId) -> Option<PathBuf> {
        self.store
            .with(|db| db.workspaces().find(workspace_id))
            .ok()
            .flatten()
            .map(|w| PathBuf::from(w.path))
    }

    pub fn rename(&self, workspace_id: WorkspaceId, name: &str) -> Result<WorkspaceSummary> {
        let record = self
            .store
            .with(|db| db.workspaces().rename(workspace_id, name))?;
        self.summarise(&record)
    }

    pub fn rebind(
        &self,
        workspace_id: WorkspaceId,
        root: impl AsRef<Path>,
    ) -> Result<WorkspaceSummary> {
        let record = self
            .store
            .with(|db| db.workspaces().rebind(workspace_id, root.as_ref()))?;
        self.report_init(workspace_id, record.as_path());
        self.summarise(&record)
    }

    pub fn open_at(&self, cwd: impl AsRef<Path>) -> Result<(WorkspaceSummary, Option<PathBuf>)> {
        let cwd =
            std::fs::canonicalize(cwd.as_ref()).unwrap_or_else(|_| cwd.as_ref().to_path_buf());

        let anchor = zlogic_objects::RepoFacts::main_repo_root(&cwd)
            .or_else(|| self.registered_ancestor(&cwd))
            .unwrap_or_else(|| cwd.clone());

        let (record, _created) = self.store.with(|db| db.workspaces().resolve(&anchor))?;
        self.report_init(record.workspace_id, &cwd);
        self.store
            .with(|db| db.workspaces().touch(record.workspace_id))?;
        let record = self
            .store
            .with(|db| db.workspaces().get(record.workspace_id))?;

        let cwd_key = PathBuf::from(zlogic_store::normalise(&cwd));
        let deviation = (cwd_key != record.as_path()).then_some(cwd_key);
        Ok((self.summarise(&record)?, deviation))
    }

    pub fn locate(&self, cwd: impl AsRef<Path>) -> Option<WorkspaceSummary> {
        let cwd =
            std::fs::canonicalize(cwd.as_ref()).unwrap_or_else(|_| cwd.as_ref().to_path_buf());
        let anchor = zlogic_objects::RepoFacts::main_repo_root(&cwd)
            .or_else(|| self.registered_ancestor(&cwd))?;
        let record = self
            .store
            .with(|db| db.workspaces().find_by_path(&anchor))
            .ok()
            .flatten()?;
        self.summarise(&record).ok()
    }

    fn registered_ancestor(&self, cwd: &Path) -> Option<PathBuf> {
        let mut cursor = Some(cwd);
        while let Some(dir) = cursor {
            if let Ok(Some(found)) = self.store.with(|db| db.workspaces().find_by_path(dir)) {
                return Some(PathBuf::from(found.path));
            }
            cursor = dir.parent();
        }
        None
    }

    fn summarise(&self, record: &zlogic_store::WorkspaceRecord) -> Result<WorkspaceSummary> {
        let session_count = self
            .store
            .with(|db| db.workspaces().session_count(record.workspace_id))?;
        Ok(summary(record, session_count, self.is_managed(record)))
    }
}

#[async_trait]
impl WorkspaceService for Workspaces {
    async fn resolve(
        &self,
        sel: WorkspaceSelector,
        name: Option<String>,
        kind: Option<WorkspaceKind>,
    ) -> ApiResult<WorkspaceSummary> {
        if kind == Some(WorkspaceKind::Custom) {
            return Err(ApiError::invalid_code(
                "workspace_custom_preset_invalid",
                "custom is not a creatable workspace preset; create it as coding/chat first, then update \
                 the tool allowlist",
            ));
        }
        let tools = kind.and_then(WorkspaceKind::preset_tools);
        let (record, _created) = self
            .lookup(&sel, name.as_deref(), tools.as_deref())
            .map_err(ApiError::from)?;

        self.report_init(record.workspace_id, record.as_path());

        self.store
            .with(|db| db.workspaces().touch(record.workspace_id))
            .map_err(EngineError::from)?;

        let record = self
            .store
            .with(|db| db.workspaces().get(record.workspace_id))
            .map_err(EngineError::from)?;
        self.summarise(&record).map_err(ApiError::from)
    }

    async fn list(&self, include_hidden: bool) -> ApiResult<Vec<WorkspaceSummary>> {
        let records = self
            .store
            .with(|db| db.workspaces().list(include_hidden))
            .map_err(EngineError::from)?;
        records
            .iter()
            .map(|r| self.summarise(r))
            .collect::<Result<_>>()
            .map_err(ApiError::from)
    }

    async fn update(&self, req: WorkspaceUpdateReq) -> ApiResult<WorkspaceSummary> {
        self.store
            .with(|db| db.workspaces().get(req.workspace_id))
            .map_err(EngineError::from)?;

        if let Some(name) = &req.name {
            self.store
                .with(|db| db.workspaces().rename(req.workspace_id, name))
                .map_err(EngineError::from)?;
        }
        if let Some(tools) = &req.tools {
            let tools = match tools {
                WorkspaceToolsUpdate::All => None,
                WorkspaceToolsUpdate::Only { tools } => Some(tools.as_slice()),
            };
            self.store
                .with(|db| db.workspaces().set_tools(req.workspace_id, tools))
                .map_err(EngineError::from)?;
        }
        let record = self
            .store
            .with(|db| {
                db.workspaces().set_preferences(
                    req.workspace_id,
                    req.pinned,
                    req.sort_order,
                    req.hidden,
                )
            })
            .map_err(EngineError::from)?;
        self.summarise(&record).map_err(ApiError::from)
    }

    async fn create_chat(&self, name: String) -> ApiResult<WorkspaceSummary> {
        let name = name.trim().to_owned();
        if name.is_empty() {
            return Err(ApiError::invalid_code(
                "workspace_chat_name_required",
                "chat workspace needs a name",
            ));
        }
        let Some(base) = &self.chat_dir else {
            return Err(ApiError::invalid_code(
                "workspace_chat_dir_unconfigured",
                "this deployment has no managed workspace directory",
            ));
        };
        std::fs::create_dir_all(base)
            .map_err(|e| ApiError::internal(format!("failed to create {}: {e}", base.display())))?;

        let slug = dir_slug(&name);
        let dir = (0..100)
            .map(|i| {
                if i == 0 {
                    base.join(&slug)
                } else {
                    base.join(format!("{slug}-{i}"))
                }
            })
            .find(|candidate| std::fs::create_dir(candidate).is_ok())
            .ok_or_else(|| {
                ApiError::internal(format!(
                    "could not create a new directory under {}",
                    base.display()
                ))
            })?;

        self.resolve(
            WorkspaceSelector::Path {
                root: dir.to_string_lossy().into_owned(),
            },
            Some(name),
            Some(WorkspaceKind::Chat),
        )
        .await
    }

    async fn delete(&self, workspace_id: WorkspaceId) -> ApiResult<()> {
        let record = self
            .store
            .with(|db| db.workspaces().get(workspace_id))
            .map_err(EngineError::from)?;

        if self.is_managed(&record) {
            let dir = PathBuf::from(record.path.clone());
            if dir.exists() {
                tokio::task::spawn_blocking(move || std::fs::remove_dir_all(&dir))
                    .await
                    .map_err(|e| {
                        ApiError::internal(format!("failed to clean up managed directory: {e}"))
                    })?
                    .map_err(|e| {
                        ApiError::internal(format!("failed to clean up managed directory: {e}"))
                    })?;
            }
        }
        self.store
            .with(|db| db.workspaces().delete(workspace_id))
            .map_err(EngineError::from)?;
        Ok(())
    }

    async fn get(&self, sel: WorkspaceSelector) -> ApiResult<WorkspaceSummary> {
        // Timed in stages on purpose: this call is the first step of every `session_list` poll and
        // every `session_open`, so when a list turns out slow this is where it becomes visible
        // whether the connection pool was queueing (`store_ms`), the filesystem stalled
        // (`managed_ms`), or the work here was trivial and the wall clock went elsewhere
        // (`total_ms` far above the three stages: the calling thread was starved or descheduled).
        let started = std::time::Instant::now();
        let (record, session_count) = self
            .store
            .with_named("workspaces.get", |db| {
                let record = match &sel {
                    WorkspaceSelector::Id { workspace_id } => {
                        db.workspaces().find(*workspace_id)?.ok_or_else(|| {
                            EngineError::NotFound(format!("workspace {workspace_id}"))
                        })?
                    }
                    WorkspaceSelector::Path { root } => {
                        if root.trim().is_empty() {
                            return Err(EngineError::Invalid(
                                "workspace path must not be empty".into(),
                            ));
                        }
                        db.workspaces()
                            .find_by_path(root)?
                            .ok_or_else(|| EngineError::NotFound(format!("workspace at {root}")))?
                    }
                };
                let session_count = db.workspaces().session_count(record.workspace_id)?;
                Ok::<_, EngineError>((record, session_count))
            })
            .map_err(ApiError::from)?;
        let stored = std::time::Instant::now();

        let managed = self.is_managed(&record);
        let resolved = std::time::Instant::now();

        let summary = summary(&record, session_count, managed);
        let finished = std::time::Instant::now();

        let store_ms = stored.duration_since(started).as_millis() as u64;
        let managed_ms = resolved.duration_since(stored).as_millis() as u64;
        let summary_ms = finished.duration_since(resolved).as_millis() as u64;
        let total_ms = finished.duration_since(started).as_millis() as u64;

        if total_ms >= SLOW_WORKSPACE_GET_MS {
            tracing::warn!(
                target: "zlogic::engine",
                workspace = %record.workspace_id,
                store_ms,
                managed_ms,
                summary_ms,
                total_ms,
                "workspaces.get slow"
            );
        }
        Ok(summary)
    }
}

impl Workspaces {
    /// Root named by a selector, for the closed half's workspace file API.
    ///
    /// An id is looked up in the registry; a path is taken as given, because the file API is also
    /// reachable for a directory the user pointed at without registering it.
    pub fn file_root(&self, workspace: &WorkspaceSelector) -> ApiResult<PathBuf> {
        match workspace {
            WorkspaceSelector::Id { .. } => self
                .lookup_existing(workspace)
                .map(|record| PathBuf::from(record.path))
                .map_err(ApiError::from),
            WorkspaceSelector::Path { root } => {
                if root.trim().is_empty() {
                    Err(ApiError::invalid_code(
                        "workspace_path_empty",
                        "workspace path must not be empty",
                    ))
                } else {
                    Ok(PathBuf::from(root))
                }
            }
        }
    }
}

impl Workspaces {
    /// Root of a *registered* workspace, for the closed half's Git probe, which stays strict about
    /// registration the way the file API deliberately is not.
    pub fn registered_root(&self, sel: &WorkspaceSelector) -> ApiResult<PathBuf> {
        self.lookup_existing(sel)
            .map(|record| PathBuf::from(record.path))
            .map_err(ApiError::from)
    }
}

fn summary(
    record: &zlogic_store::WorkspaceRecord,
    session_count: u32,
    managed: bool,
) -> WorkspaceSummary {
    let kind = WorkspaceKind::of_tools(record.tools.as_deref());
    WorkspaceSummary {
        workspace_id: record.workspace_id,
        root: record.path.clone(),
        name: record.name.clone(),
        exists: record.exists(),
        pinned: record.pinned,
        sort_order: record.sort_order,
        last_opened_at: record.last_opened_at,
        hidden: record.hidden,
        session_count,
        tools: record.tools.clone(),
        kind,
        managed,
    }
}

fn dir_slug(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            c if c.is_alphanumeric() || c == '-' || c == '_' => Some(c),
            _ => None,
        })
        .take(40)
        .collect();
    let cleaned = cleaned.trim_matches(['-', '_', '.']).to_owned();
    if cleaned.is_empty() {
        "chat".to_owned()
    } else {
        cleaned
    }
}

impl crate::WorkspaceRoots for Workspaces {
    fn root_of(&self, workspace_id: WorkspaceId) -> Option<PathBuf> {
        Workspaces::root_of(self, workspace_id)
    }

    fn tools_of(&self, workspace_id: WorkspaceId) -> Option<Vec<String>> {
        self.store
            .with(|db| db.workspaces().find(workspace_id))
            .ok()
            .flatten()
            .and_then(|w| w.tools)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::EventHub;
    use zlogic_protocol::stream::{InitPhase, WorkspaceInitProgress};

    fn store() -> SharedStore {
        SharedStore::new(zlogic_store::Db::open_in_memory().unwrap())
    }

    fn path_sel(dir: &Path) -> WorkspaceSelector {
        WorkspaceSelector::Path {
            root: dir.to_string_lossy().into_owned(),
        }
    }

    #[tokio::test]
    async fn resolving_a_directory_registers_it_and_names_it_after_the_folder() {
        let ws = Workspaces::new(store());
        let dir = tempfile::tempdir().unwrap();

        let summary = ws.resolve(path_sel(dir.path()), None, None).await.unwrap();
        assert_eq!(
            summary.name,
            dir.path().file_name().unwrap().to_string_lossy()
        );
        assert_eq!(
            summary.root,
            zlogic_store::normalise(dir.path()),
            "root is the normalized key (realpath plus stripping the verbatim prefix), the same convention as workspaces.path"
        );
        assert!(summary.exists);
    }

    #[tokio::test]
    async fn the_same_directory_always_resolves_to_the_same_id() {
        let ws = Workspaces::new(store());
        let dir = tempfile::tempdir().unwrap();

        let a = ws.resolve(path_sel(dir.path()), None, None).await.unwrap();
        let b = ws
            .resolve(path_sel(&dir.path().join(".")), None, None)
            .await
            .unwrap();
        assert_eq!(a.workspace_id, b.workspace_id);

        let by_id = ws
            .resolve(
                WorkspaceSelector::Id {
                    workspace_id: a.workspace_id,
                },
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(by_id.root, a.root);
    }

    #[tokio::test]
    async fn an_unknown_id_is_not_found_rather_than_silently_created() {
        let ws = Workspaces::new(store());
        let err = ws
            .resolve(
                WorkspaceSelector::Id {
                    workspace_id: WorkspaceId::new(),
                },
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(
            err.category,
            zlogic_protocol::ErrorCategory::NotFound,
            "{err:?}"
        );
        assert!(
            ws.list(true).await.unwrap().is_empty(),
            "must not leave a single row behind"
        );
    }

    #[tokio::test]
    async fn an_empty_path_is_rejected() {
        let ws = Workspaces::new(store());
        let err = ws
            .resolve(WorkspaceSelector::Path { root: "   ".into() }, None, None)
            .await
            .unwrap_err();
        assert_eq!(
            err.category,
            zlogic_protocol::ErrorCategory::InvalidArgument,
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn a_moved_away_directory_is_reported_as_missing() {
        let ws = Workspaces::new(store());
        let dir = tempfile::tempdir().unwrap();
        let id = ws
            .resolve(path_sel(dir.path()), None, None)
            .await
            .unwrap()
            .workspace_id;
        let root = dir.path().to_path_buf();
        drop(dir);

        let again = ws
            .resolve(WorkspaceSelector::Id { workspace_id: id }, None, None)
            .await
            .unwrap();
        assert!(!again.exists, "{}", root.display());
    }

    #[tokio::test]
    async fn rebinding_keeps_the_id_so_history_survives_a_move() {
        let st = store();
        let ws = Workspaces::new(st.clone());
        let old = tempfile::tempdir().unwrap();
        let new = tempfile::tempdir().unwrap();

        let id = ws
            .resolve(path_sel(old.path()), None, None)
            .await
            .unwrap()
            .workspace_id;
        let session = st
            .with(|db| db.sessions().create(zlogic_store::NewSession::root(id)))
            .unwrap()
            .session_id;

        let moved = ws.rebind(id, new.path()).unwrap();
        assert_eq!(moved.workspace_id, id);
        assert_eq!(moved.root, zlogic_store::normalise(new.path()));
        let s = st.with(|db| db.sessions().get(session)).unwrap();
        assert_eq!(s.workspace_id, id);
    }

    #[tokio::test]
    async fn renaming_changes_the_label_only() {
        let ws = Workspaces::new(store());
        let dir = tempfile::tempdir().unwrap();
        let before = ws.resolve(path_sel(dir.path()), None, None).await.unwrap();

        let after = ws.rename(before.workspace_id, "zlogic").unwrap();
        assert_eq!(after.name, "zlogic");
        assert_eq!(after.workspace_id, before.workspace_id);
        assert_eq!(after.root, before.root);
    }

    fn init_repo(dir: &Path) {
        let repo = git2::Repository::init(dir).unwrap();
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("a.txt")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("t", "t@test").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
    }

    #[tokio::test]
    async fn starting_from_a_subdirectory_reuses_the_repository_root() {
        let ws = Workspaces::new(store());
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let deep = root.path().join("crates").join("core");
        std::fs::create_dir_all(&deep).unwrap();

        let (from_root, dev_root) = ws.open_at(root.path()).unwrap();
        let (from_deep, dev_deep) = ws.open_at(&deep).unwrap();

        assert_eq!(
            from_deep.workspace_id, from_root.workspace_id,
            "the same project, the same id"
        );
        assert_eq!(dev_root, None, "starting at the root is no deviation");
        assert_eq!(
            dev_deep,
            Some(PathBuf::from(zlogic_store::normalise(&deep))),
            "starting in a subdirectory records it as exec_cwd (the normalized key)"
        );
    }

    #[tokio::test]
    async fn the_identity_does_not_depend_on_where_it_was_first_run() {
        let ws = Workspaces::new(store());
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let deep = root.path().join("sub");
        std::fs::create_dir_all(&deep).unwrap();

        let first = ws.open_at(&deep).unwrap().0;
        let second = ws.open_at(root.path()).unwrap().0;

        assert_eq!(first.workspace_id, second.workspace_id);
        assert_eq!(
            ws.list(true).await.unwrap().len(),
            1,
            "two nested workspaces must not appear"
        );
    }

    #[tokio::test]
    async fn a_worktree_folds_into_its_main_repository() {
        let ws = Workspaces::new(store());
        let main = tempfile::tempdir().unwrap();
        init_repo(main.path());
        let repo = git2::Repository::open(main.path()).unwrap();
        let wt_dir = main.path().join("..").join(format!(
            "zlogic-wt-{}",
            main.path().file_name().unwrap().to_string_lossy()
        ));
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("feat", &head, false).unwrap();
        let mut opts = git2::WorktreeAddOptions::new();
        let reference = repo.find_reference("refs/heads/feat").unwrap();
        opts.reference(Some(&reference));
        let Ok(wt) = repo.worktree("feat", &wt_dir, Some(&opts)) else {
            return;
        };

        let (from_main, _) = ws.open_at(main.path()).unwrap();
        let (from_wt, deviation) = ws.open_at(wt.path()).unwrap();

        assert_eq!(
            from_wt.workspace_id, from_main.workspace_id,
            "a worktree does not produce a new workspace"
        );
        assert!(deviation.is_some(), "but it is a different timeline");
        let _ = std::fs::remove_dir_all(&wt_dir);
    }

    #[tokio::test]
    async fn a_plain_directory_reuses_a_registered_ancestor() {
        let ws = Workspaces::new(store());
        let root = tempfile::tempdir().unwrap();
        let deep = root.path().join("notes").join("2026");
        std::fs::create_dir_all(&deep).unwrap();

        let registered = ws.resolve(path_sel(root.path()), None, None).await.unwrap();

        let (found, deviation) = ws.open_at(&deep).unwrap();
        assert_eq!(found.workspace_id, registered.workspace_id);
        assert_eq!(
            deviation,
            Some(PathBuf::from(zlogic_store::normalise(&deep)))
        );
    }

    #[tokio::test]
    async fn an_unrelated_plain_directory_becomes_its_own_workspace() {
        let ws = Workspaces::new(store());
        let dir = tempfile::tempdir().unwrap();

        let (created, deviation) = ws.open_at(dir.path()).unwrap();
        assert_eq!(created.root, zlogic_store::normalise(dir.path()));
        assert_eq!(deviation, None);
    }

    #[tokio::test]
    async fn a_nested_repository_is_its_own_workspace() {
        let ws = Workspaces::new(store());
        let outer = tempfile::tempdir().unwrap();
        let outer_ws = ws
            .resolve(path_sel(outer.path()), None, None)
            .await
            .unwrap();

        let inner = outer.path().join("vendor").join("thing");
        std::fs::create_dir_all(&inner).unwrap();
        init_repo(&inner);

        let (found, _) = ws.open_at(&inner).unwrap();
        assert_ne!(found.workspace_id, outer_ws.workspace_id);
        assert_eq!(found.root, zlogic_store::normalise(&inner));
    }

    #[tokio::test]
    async fn locate_finds_a_registered_workspace_without_registering() {
        let st = store();
        let ws = Workspaces::new(st.clone());
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let deep = root.path().join("crates").join("core");
        std::fs::create_dir_all(&deep).unwrap();

        let registered = ws.open_at(root.path()).unwrap().0;

        let from_root = ws.locate(root.path()).expect("must match at the root");
        let from_deep = ws
            .locate(&deep)
            .expect("a subdirectory must match the same one");
        assert_eq!(from_root.workspace_id, registered.workspace_id);
        assert_eq!(from_deep.workspace_id, registered.workspace_id);
        assert_eq!(
            st.with(|db| db.workspaces().list(true)).unwrap().len(),
            1,
            "locate must not register a new row"
        );
    }

    #[tokio::test]
    async fn locate_finds_a_registered_ancestor() {
        let ws = Workspaces::new(store());
        let root = tempfile::tempdir().unwrap();
        let deep = root.path().join("notes").join("2026");
        std::fs::create_dir_all(&deep).unwrap();

        let registered = ws.resolve(path_sel(root.path()), None, None).await.unwrap();

        let found = ws.locate(&deep).expect("a registered ancestor must match");
        assert_eq!(found.workspace_id, registered.workspace_id);
        assert!(ws.locate(root.path()).is_some());
    }

    #[tokio::test]
    async fn locate_on_an_unrelated_directory_is_none_and_read_only() {
        let st = store();
        let ws = Workspaces::new(st.clone());
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(ws.locate(dir.path()), None);

        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        assert_eq!(
            ws.locate(repo.path()),
            None,
            "the git root was never registered → None"
        );

        assert_eq!(
            st.with(|db| db.workspaces().list(true)).unwrap().len(),
            0,
            "locate must never register, not even on a miss"
        );
    }

    #[tokio::test]
    async fn locate_prefers_the_git_root_over_a_registered_outer_ancestor() {
        let ws = Workspaces::new(store());
        let outer = tempfile::tempdir().unwrap();
        let outer_ws = ws
            .resolve(path_sel(outer.path()), None, None)
            .await
            .unwrap();

        let inner = outer.path().join("vendor").join("thing");
        std::fs::create_dir_all(&inner).unwrap();
        init_repo(&inner);

        assert_eq!(ws.locate(&inner), None);

        let (registered_inner, _) = ws.open_at(&inner).unwrap();
        let deep = inner.join("sub");
        std::fs::create_dir_all(&deep).unwrap();
        let found = ws.locate(&deep).unwrap();
        assert_eq!(found.workspace_id, registered_inner.workspace_id);
        assert_ne!(
            found.workspace_id, outer_ws.workspace_id,
            "it matches the workspace of the git root itself, not the outer one"
        );
    }

    #[tokio::test]
    async fn resolving_records_the_open_so_the_sidebar_can_sort_by_it() {
        let ws = Workspaces::new(store());
        let dir = tempfile::tempdir().unwrap();

        let opened = ws.resolve(path_sel(dir.path()), None, None).await.unwrap();
        assert!(
            opened.last_opened_at.is_some(),
            "having opened it, there must be a time"
        );
        assert_eq!(opened.session_count, 0);
    }

    #[tokio::test]
    async fn getting_a_workspace_is_read_only() {
        let st = store();
        let ws = Workspaces::new(st.clone());
        let dir = tempfile::tempdir().unwrap();
        let record = st.with(|db| db.workspaces().resolve(dir.path())).unwrap().0;
        ws.update(WorkspaceUpdateReq {
            workspace_id: record.workspace_id,
            name: None,
            pinned: None,
            sort_order: None,
            hidden: Some(true),
            tools: None,
        })
        .await
        .unwrap();

        let found = ws
            .get(WorkspaceSelector::Id {
                workspace_id: record.workspace_id,
            })
            .await
            .unwrap();
        assert!(
            found.hidden,
            "a read-only query must not make the workspace visible again"
        );
        assert_eq!(
            found.last_opened_at, None,
            "a read-only query must not update the sidebar sort time"
        );

        let missing = tempfile::tempdir().unwrap();
        assert!(
            ws.get(path_sel(missing.path())).await.is_err(),
            "a read-only query must not register a new path"
        );
    }

    #[tokio::test]
    async fn hiding_removes_it_from_the_list_without_losing_anything() {
        let st = store();
        let ws = Workspaces::new(st.clone());
        let dir = tempfile::tempdir().unwrap();
        let id = ws
            .resolve(path_sel(dir.path()), None, None)
            .await
            .unwrap()
            .workspace_id;
        let session = st
            .with(|db| db.sessions().create(zlogic_store::NewSession::root(id)))
            .unwrap()
            .session_id;

        let hidden = ws
            .update(WorkspaceUpdateReq {
                workspace_id: id,
                name: None,
                pinned: None,
                sort_order: None,
                hidden: Some(true),
                tools: None,
            })
            .await
            .unwrap();
        assert!(hidden.hidden);
        assert!(
            ws.list(false).await.unwrap().is_empty(),
            "invisible in the default list"
        );
        assert_eq!(
            ws.list(true).await.unwrap().len(),
            1,
            "still visible in the management view"
        );
        assert!(
            st.with(|db| db.sessions().find(session)).unwrap().is_some(),
            "the session was not deleted"
        );

        let back = ws.resolve(path_sel(dir.path()), None, None).await.unwrap();
        assert_eq!(back.workspace_id, id);
        assert!(
            !back.hidden,
            "the user just opened it by hand, so it must not stay hidden"
        );
    }

    #[tokio::test]
    async fn updating_can_change_the_name_and_the_preferences_together() {
        let ws = Workspaces::new(store());
        let dir = tempfile::tempdir().unwrap();
        let id = ws
            .resolve(path_sel(dir.path()), None, None)
            .await
            .unwrap()
            .workspace_id;

        let updated = ws
            .update(WorkspaceUpdateReq {
                workspace_id: id,
                name: Some("zlogic".into()),
                pinned: Some(true),
                sort_order: Some(3),
                hidden: None,
                tools: None,
            })
            .await
            .unwrap();

        assert_eq!(updated.name, "zlogic");
        assert!(updated.pinned);
        assert_eq!(updated.sort_order, 3);
        assert_eq!(updated.workspace_id, id, "preferences are not identity");
    }

    #[tokio::test]
    async fn updating_an_unknown_workspace_is_not_found() {
        let ws = Workspaces::new(store());
        let err = ws
            .update(WorkspaceUpdateReq {
                workspace_id: WorkspaceId::new(),
                name: None,
                pinned: Some(true),
                sort_order: None,
                hidden: None,
                tools: None,
            })
            .await
            .unwrap_err();
        assert_eq!(
            err.category,
            zlogic_protocol::ErrorCategory::NotFound,
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn the_list_reports_how_many_sessions_each_workspace_has() {
        let st = store();
        let ws = Workspaces::new(st.clone());
        let dir = tempfile::tempdir().unwrap();
        let id = ws
            .resolve(path_sel(dir.path()), None, None)
            .await
            .unwrap()
            .workspace_id;
        for _ in 0..3 {
            st.with(|db| {
                db.sessions()
                    .create(zlogic_store::NewSession::root(id))
                    .unwrap()
            });
        }

        let list = ws.list(false).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].session_count, 3);
    }

    // ── git_info ──

    async fn terminal(
        rx: &mut tokio::sync::broadcast::Receiver<WorkspaceInitProgress>,
    ) -> Vec<InitPhase> {
        let mut seen = Vec::new();
        loop {
            let p = tokio::time::timeout(std::time::Duration::from_secs(20), rx.recv())
                .await
                .expect("timed out")
                .expect("the channel is closed");
            let last = matches!(p.phase, InitPhase::Done { .. } | InitPhase::Failed { .. });
            seen.push(p.phase);
            if last {
                return seen;
            }
        }
    }

    #[tokio::test]
    async fn opening_a_workspace_reports_started_then_done() {
        let st = store();
        let work = tempfile::tempdir().unwrap();
        std::fs::write(work.path().join("main.rs"), "fn main() {}").unwrap();

        let hub = Arc::new(EventHub::new());
        let ws = Workspaces::new(st).with_hub(hub.clone());
        let mut rx = hub.subscribe_inits();

        ws.resolve(path_sel(work.path()), None, None).await.unwrap();

        let seen = terminal(&mut rx).await;
        assert_eq!(
            seen,
            vec![InitPhase::Started, done()],
            "an open has to announce `Started` and then finish with `Done`"
        );
    }

    #[tokio::test]
    async fn reopening_an_existing_workspace_reports_the_pair_again() {
        let st = store();
        let work = tempfile::tempdir().unwrap();
        std::fs::write(work.path().join("a.rs"), "x").unwrap();

        let hub = Arc::new(EventHub::new());
        let ws = Workspaces::new(st).with_hub(hub.clone());

        for _ in 0..2 {
            let mut rx = hub.subscribe_inits();
            ws.resolve(path_sel(work.path()), None, None).await.unwrap();
            assert_eq!(terminal(&mut rx).await, vec![InitPhase::Started, done()]);
        }
    }

    fn done() -> InitPhase {
        InitPhase::Done {
            files: 0,
            bytes: 0,
            skipped: 0,
            elapsed_ms: 0,
            unchanged: false,
        }
    }

    #[tokio::test]
    async fn resolving_works_without_a_hub_wired() {
        let ws = Workspaces::new(store());
        let dir = tempfile::tempdir().unwrap();
        assert!(ws.resolve(path_sel(dir.path()), None, None).await.is_ok());
    }

    #[tokio::test]
    async fn the_registry_answers_root_of_for_the_dispatcher() {
        use crate::WorkspaceRoots;
        let ws = Workspaces::new(store());
        let dir = tempfile::tempdir().unwrap();
        let id = ws
            .resolve(path_sel(dir.path()), None, None)
            .await
            .unwrap()
            .workspace_id;

        assert_eq!(
            WorkspaceRoots::root_of(&ws, id),
            Some(PathBuf::from(zlogic_store::normalise(dir.path())))
        );
        assert_eq!(WorkspaceRoots::root_of(&ws, WorkspaceId::new()), None);
    }

    #[tokio::test]
    async fn tools_of_reflects_the_preset_and_later_updates() {
        use crate::WorkspaceRoots;
        use zlogic_protocol::query::WorkspaceKind;
        let ws = Workspaces::new(store());
        let dir = tempfile::tempdir().unwrap();
        let id = ws
            .resolve(path_sel(dir.path()), None, Some(WorkspaceKind::Chat))
            .await
            .unwrap()
            .workspace_id;

        assert_eq!(
            WorkspaceRoots::tools_of(&ws, id),
            Some(zlogic_protocol::chat_workspace_tools())
        );
        assert_eq!(WorkspaceRoots::tools_of(&ws, WorkspaceId::new()), None);

        ws.update(WorkspaceUpdateReq {
            workspace_id: id,
            name: None,
            pinned: None,
            sort_order: None,
            hidden: None,
            tools: Some(WorkspaceToolsUpdate::All),
        })
        .await
        .unwrap();
        assert_eq!(
            WorkspaceRoots::tools_of(&ws, id),
            None,
            "restoring means no restriction"
        );
    }

    #[tokio::test]
    async fn resolving_with_the_custom_preset_is_rejected() {
        use zlogic_protocol::query::WorkspaceKind;
        let ws = Workspaces::new(store());
        let dir = tempfile::tempdir().unwrap();
        let err = ws
            .resolve(path_sel(dir.path()), None, Some(WorkspaceKind::Custom))
            .await
            .unwrap_err();
        assert_eq!(
            err.category,
            zlogic_protocol::ErrorCategory::InvalidArgument,
            "{err:?}"
        );
        assert!(
            ws.list(true).await.unwrap().is_empty(),
            "must not leave a single row behind"
        );
    }

    #[tokio::test]
    async fn a_managed_chat_workspace_lives_and_dies_with_its_generated_dir() {
        use zlogic_protocol::query::WorkspaceKind;
        let base = tempfile::tempdir().unwrap();
        let ws = Workspaces::new(store()).with_chat_dir(base.path().join("chats"));

        let first = ws.create_chat("daily questions".into()).await.unwrap();
        assert_eq!(first.kind, WorkspaceKind::Chat);
        assert!(first.managed, "root is under the managed base");
        assert!(
            std::path::Path::new(&first.root).is_dir(),
            "the directory really was created"
        );
        assert_eq!(first.name, "daily questions");

        let second = ws.create_chat("daily questions".into()).await.unwrap();
        assert_ne!(first.workspace_id, second.workspace_id);
        assert_ne!(first.root, second.root);

        let user_dir = tempfile::tempdir().unwrap();
        let user = ws
            .resolve(path_sel(user_dir.path()), None, None)
            .await
            .unwrap();
        assert!(!user.managed);
        ws.delete(user.workspace_id).await.unwrap();
        assert!(
            user_dir.path().is_dir(),
            "not a single byte of the user's folder is touched"
        );

        ws.delete(first.workspace_id).await.unwrap();
        assert!(
            !std::path::Path::new(&first.root).exists(),
            "the directory should be cleaned up"
        );
        assert!(std::path::Path::new(&second.root).is_dir());
        let ids: Vec<_> = ws
            .list(true)
            .await
            .unwrap()
            .into_iter()
            .map(|w| w.workspace_id)
            .collect();
        assert_eq!(ids, [second.workspace_id]);
    }

    #[tokio::test]
    async fn chat_dir_names_are_sanitised_but_display_names_are_not() {
        let base = tempfile::tempdir().unwrap();
        let ws = Workspaces::new(store()).with_chat_dir(base.path().join("chats"));
        let created = ws
            .create_chat("a/b:c?  weekly report ".into())
            .await
            .unwrap();
        assert_eq!(
            created.name, "a/b:c?  weekly report",
            "the display name only loses its leading and trailing whitespace"
        );
        let dir_name = std::path::Path::new(&created.root)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(!dir_name.contains(['/', ':', '?']), "{dir_name}");
        assert!(dir_name.contains("weekly-report"), "{dir_name}");
    }

    #[tokio::test]
    async fn create_chat_without_a_chat_dir_is_an_explicit_error() {
        let ws = Workspaces::new(store());
        let err = ws.create_chat("x".into()).await.unwrap_err();
        assert_eq!(
            err.category,
            zlogic_protocol::ErrorCategory::InvalidArgument
        );
    }
}
