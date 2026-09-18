//! ```ignore
//! let boot = Engine::bootstrap(BootstrapOptions::new("cli"))?;
//! ```
//! ```text
//!                       ├─→ objects               ├─→ CoreServices ─→ Dispatcher
//!                       └─→ workspaces ─→ sessions ──→ Engine
//! ```

use std::path::PathBuf;
use std::sync::Arc;

use zlogic_config::{AppConfig, Dirs, load_env_file};
use zlogic_core::{ContextPolicy, CoreServices, Limits, PolicyGate, SharedStore};
use zlogic_credential::{CredentialStore, SystemCredentialStore, credential_candidates};
use zlogic_objects::{FileObjectStore, ObjectStore};
use zlogic_store::Db;
use zlogic_tools::{
    MemoryUpdate, SearchKeySource, SearchProvider, Shell, ShellPreference as ToolShellPreference,
    ToolRegistry, WebSearchSettings,
};

use crate::hub::EventHub;
use crate::{
    Auxiliary, BypassFlag, BypassGate, Config, Credentials, Dispatcher, Engine, EngineInteractions,
    Memories, ModelRouter, PolicyCoreGate, SessionLocks, Sessions, Workspaces, Worktrees,
};

pub struct BootstrapOptions {
    pub host: &'static str,
    pub approval_mode: bool,
    pub dirs: Option<Dirs>,
    pub maintenance: bool,
    /// Can the host render a tool's HTML widget output? Declared here because the `widget` tool
    /// belongs to the closed tool set, which reads this capability through the tool injection seam.
    pub html_widgets: bool,
    pub renders_math: bool,
    /// Directories the host wants on the PATH of every process the engine spawns. A host that
    /// ships its own interpreter — or anything else the model is expected to call — passes that
    /// bin directory here; the CLI passes none and relies on the ambient PATH.
    pub runtime_paths: Vec<PathBuf>,
    /// Set by a host that brings the parts of the product which are not open source: the closed
    /// tool set, the administration of MCP servers, plugins and skills, and the management of
    /// managed external resources. See [`ProHosts`] for the handles such a host receives and
    /// [`ProWiring`] for what it hands back.
    pub pro: Option<ProFactory>,
}

/// The engine's handles a host-side pro wiring plugs into.
///
/// Everything the closed half of the product needs is reachable from here: the closed tool set is
/// registered as ordinary tools, and the two administrative surfaces return their own
/// implementations of the engine's service traits. Keeping them on one seam is deliberate — the
/// host builds its managed-resource service once and hands the same value to its tools, so no
/// tool can be constructed against a host that does not exist yet. A host that brings none of
/// this (the CLI) is a fully working engine with the open tool set.
pub struct ProHosts {
    pub dirs: Dirs,
    pub store: SharedStore,
    pub credentials: Arc<dyn CredentialStore>,
    pub extensions: Arc<crate::Extensions>,
    /// The content-addressed store every turn writes through: the closed half reads objects back
    /// for its viewers and its own drafting calls.
    pub objects: Arc<dyn ObjectStore>,
    /// Workspace access, including the read-only Git context the commit-message call builds.
    pub workspaces: Arc<Workspaces>,
    /// The engine's own auxiliary calls, exposed so the closed half can send its prompts — a
    /// commit message, a background job draft — through the same no-turn model path.
    pub auxiliary: Arc<Auxiliary>,
    /// Whether the host can render a tool's HTML widget output: the closed `widget` tool exists
    /// only where it can.
    pub html_widgets: bool,
}

/// What a host returns from [`ProFactory`]: the tools it adds to the registry, and the services it
/// implements itself.
///
/// Anything left empty is simply not wired, and the engine answers with its "not wired" error
/// rather than pretending the capability exists.
#[derive(Default)]
pub struct ProWiring {
    pub tools: Vec<Arc<dyn zlogic_tools::Tool>>,
    pub managed_resources: Option<Arc<dyn crate::service::ManagedResourceService>>,
    pub extension_service: Option<Arc<dyn crate::service::ExtensionService>>,
    pub objects: Option<Arc<dyn crate::service::ObjectService>>,
    pub auxiliary: Option<Arc<dyn crate::service::AuxiliaryService>>,
    pub files: Option<Arc<dyn crate::service::WorkspaceFilesService>>,
    pub git: Option<Arc<dyn crate::service::WorkspaceGitService>>,
    pub agent_profiles: Option<Arc<dyn crate::service::AgentProfileService>>,
}

pub type ProFactory = Box<dyn FnOnce(ProHosts) -> ProWiring + Send>;

/// The host-supplied directories, read on every turn so a host that changes its mind takes effect
/// without a restart.
struct HostRuntimePaths(Vec<PathBuf>);

impl zlogic_tools::RuntimePathProvider for HostRuntimePaths {
    fn bin_dirs(&self) -> Vec<PathBuf> {
        self.0.clone()
    }
}

impl BootstrapOptions {
    pub fn new(host: &'static str) -> Self {
        Self {
            host,
            approval_mode: false,
            dirs: None,
            maintenance: true,
            html_widgets: false,
            renders_math: false,
            runtime_paths: Vec::new(),
            pro: None,
        }
    }

    /// Hand the directories of a host-shipped tool — typically its Python — to the engine. They are
    /// prepended to the PATH of every process a tool spawns.
    pub fn runtime_paths(mut self, paths: Vec<PathBuf>) -> Self {
        self.runtime_paths = paths;
        self
    }

    pub fn html_widgets(mut self, on: bool) -> Self {
        self.html_widgets = on;
        self
    }

    pub fn renders_math(mut self, on: bool) -> Self {
        self.renders_math = on;
        self
    }

    pub fn approval_mode(mut self, on: bool) -> Self {
        self.approval_mode = on;
        self
    }

    pub fn dirs(mut self, dirs: Dirs) -> Self {
        self.dirs = Some(dirs);
        self
    }

    pub fn maintenance(mut self, on: bool) -> Self {
        self.maintenance = on;
        self
    }

    /// Hand the closed half of the product to the host through `factory`. A host that passes
    /// nothing keeps a fully working engine: the open tool set, the loading path of extensions and
    /// no administrative surfaces.
    pub fn pro(mut self, factory: ProFactory) -> Self {
        self.pro = Some(factory);
        self
    }
}

pub struct Bootstrapped {
    pub engine: Engine,
    pub interactions: Arc<EngineInteractions>,
    pub workspaces: Arc<Workspaces>,
    pub store: SharedStore,
    pub objects: Arc<dyn ObjectStore>,
    pub dirs: Dirs,
    pub config: Arc<AppConfig>,
    pub approval_mode: bool,
    pub warnings: Vec<String>,
    pub extensions: Arc<crate::Extensions>,
    /// Process-local runtime registry backed by durable `task` rows.
    pub tasks: Arc<crate::TaskManager>,
    pub grants: Arc<crate::Grants>,
}

#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error("could not infer data directory: {0}")]
    Dirs(String),
    #[error("cannot create {path}: {reason}")]
    Ensure { path: String, reason: String },
    #[error("cannot read config ({path}): {reason}")]
    Config { path: String, reason: String },
    #[error("cannot open {path}: {reason}")]
    Open { path: String, reason: String },
}

impl Engine {
    pub fn bootstrap(opts: BootstrapOptions) -> Result<Bootstrapped, BootstrapError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dirs = match opts.dirs {
            Some(d) => d,
            None => Dirs::discover().map_err(|e| BootstrapError::Dirs(e.to_string()))?,
        };
        dirs.ensure().map_err(|e| BootstrapError::Ensure {
            path: dirs.data.display().to_string(),
            reason: e.to_string(),
        })?;

        let env_file_warnings = match load_env_file(&dirs) {
            Ok(env) => {
                zlogic_credential::set_env_overlay(env);
                Vec::new()
            }
            Err(e) => vec![format!("failed to load env.yaml: {e}")],
        };

        zlogic_credential::init_secret_vault(dirs.master_key_file(), dirs.secrets_blob());

        let credential_store: Arc<dyn CredentialStore> = Arc::new(SystemCredentialStore);
        let config = AppConfig::load(&dirs, |reference| credential_store.is_available(reference))
            .map_err(|e| BootstrapError::Config {
            path: dirs.config_file().display().to_string(),
            reason: e.to_string(),
        })?;
        let mut warnings = config.warnings.clone();
        warnings.extend(env_file_warnings);
        let approval_mode = opts.approval_mode
            || config.session.approval_mode == zlogic_protocol::settings::ApprovalMode::Bypass;
        let config = Arc::new(config);

        let db_path = dirs.data.join("state.db");
        let db = Db::open(&db_path).map_err(|e| BootstrapError::Open {
            path: db_path.display().to_string(),
            reason: e.to_string(),
        })?;
        let store = SharedStore::new(db);

        let objects_dir = dirs.data.join("objects");
        let objects: Arc<dyn ObjectStore> = Arc::new(FileObjectStore::open(&objects_dir).map_err(
            |e| BootstrapError::Open {
                path: objects_dir.display().to_string(),
                reason: e.to_string(),
            },
        )?);
        let tasks = Arc::new(
            crate::TaskManager::new(
                store.clone(),
                objects.clone(),
                dirs.data.join("task-output"),
            )
            .map_err(|e| BootstrapError::Open {
                path: db_path.display().to_string(),
                reason: format!("task runtime: {e}"),
            })?,
        );
        match tasks.reconcile_interrupted() {
            Ok(count) if count > 0 => tracing::info!(
                target: "zlogic::task",
                count,
                "marked leftover Tasks from the previous run as interrupted"
            ),
            Ok(_) => {}
            Err(error) => warnings.push(format!(
                "failed to reconcile left-over Tasks from the previous run: {error}"
            )),
        }
        // The CLI bootstrap runs inside Tokio and can start immediately. A GUI host may bootstrap
        // before its runtime exists; it calls this same idempotent method from setup.
        tasks.start_scheduler();

        match crate::Lifecycle::new(store.clone()).reconcile() {
            Ok(done) if !done.is_empty() => tracing::info!(
                target: "zlogic::engine",
                locks = done.locks_released,
                interactions = done.interactions_closed,
                "reconciled state left by the previous run"
            ),
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(target: "zlogic::engine", "startup reconciliation failed: {e}")
            }
        }

        let hub = Arc::new(EventHub::new());
        let interactions = Arc::new(EngineInteractions::new(hub.clone()));
        tasks.bind_hub(Arc::downgrade(&hub));

        let workspaces = Arc::new(
            Workspaces::new(store.clone())
                .with_hub(hub.clone())
                .with_chat_dir(dirs.data.join("chats")),
        );

        let worktrees = Arc::new(Worktrees::new(store.clone(), config.worktree.dir.clone()));

        let transport: Arc<dyn zlogic_llm::transport::HttpTransport> = Arc::new(
            zlogic_llm::transport::ReqwestTransport::with_network(&config.network),
        );

        let router = Arc::new(ModelRouter::new(
            config.clone(),
            transport.clone(),
            credential_store.clone(),
        ));
        let auxiliary = Arc::new(Auxiliary::new(store.clone(), router.clone(), hub.clone()));
        let (mut tools, shell_dialect, mut tool_warnings) = tool_registry(
            &config,
            Some(Arc::new(CredentialSearchKeys {
                store: credential_store.clone(),
                auto_detect_env: config.auto_detect_env,
            })),
        );
        warnings.append(&mut tool_warnings);
        let memories = Arc::new(Memories::new(store.clone(), objects.clone()));
        tools.add(Arc::new(MemoryUpdate::new(memories.clone())));

        // The closed half of the product arrives in one piece: its tools join the same registry as
        // the built-ins, and its services replace the engine's "not wired" defaults. Everything
        // that follows — the prompt, the dispatcher, the approval gate — then treats a closed tool
        // like any other, without knowing where it came from.
        let extensions = crate::Extensions::new(&dirs);
        let pro = opts.pro.map(|factory| {
            factory(ProHosts {
                dirs: dirs.clone(),
                store: store.clone(),
                credentials: credential_store.clone(),
                extensions: extensions.clone(),
                objects: objects.clone(),
                workspaces: workspaces.clone(),
                auxiliary: auxiliary.clone(),
                html_widgets: opts.html_widgets,
            })
        });
        if let Some(pro) = &pro {
            for tool in &pro.tools {
                tools.add(tool.clone());
            }
        }
        let managed_resources: Arc<dyn crate::service::ManagedResourceService> = pro
            .as_ref()
            .and_then(|pro| pro.managed_resources.clone())
            .unwrap_or_else(|| Arc::new(crate::not_wired::NotWired));
        let extension_service: Arc<dyn crate::service::ExtensionService> = pro
            .as_ref()
            .and_then(|pro| pro.extension_service.clone())
            .unwrap_or_else(|| Arc::new(crate::not_wired::NotWired));
        let objects_service: Arc<dyn crate::service::ObjectService> = pro
            .as_ref()
            .and_then(|pro| pro.objects.clone())
            .unwrap_or_else(|| Arc::new(crate::not_wired::NotWired));
        let auxiliary_service: Arc<dyn crate::service::AuxiliaryService> = pro
            .as_ref()
            .and_then(|pro| pro.auxiliary.clone())
            .unwrap_or_else(|| Arc::new(crate::not_wired::NotWired));
        // The registry keeps serving `WorkspaceService`: the file and Git halves are separate
        // services, so a build without a closed half simply has none.
        let files_service: Arc<dyn crate::service::WorkspaceFilesService> = pro
            .as_ref()
            .and_then(|pro| pro.files.clone())
            .unwrap_or_else(|| Arc::new(crate::not_wired::NotWired));
        let git_service: Arc<dyn crate::service::WorkspaceGitService> = pro
            .as_ref()
            .and_then(|pro| pro.git.clone())
            .unwrap_or_else(|| Arc::new(crate::not_wired::NotWired));
        // The built-in profiles stay in the engine; only the stored, user-edited ones need a
        // closed half, because editing them is a desktop surface.
        let agent_profile_service: Arc<dyn crate::service::AgentProfileService> = pro
            .as_ref()
            .and_then(|pro| pro.agent_profiles.clone())
            .unwrap_or_else(|| Arc::new(crate::not_wired::NotWired));

        let tool_catalog = Arc::new(crate::ToolCatalog::new(tools.clone()));

        let grants = Arc::new(crate::Grants::new(dirs.clone()));

        let bypass_cell = BypassFlag::new(approval_mode);
        let policy: Arc<dyn PolicyGate> = Arc::new(BypassGate::new(
            bypass_cell.clone(),
            Arc::new(PolicyCoreGate::new(
                shell_dialect,
                dirs.clone(),
                router.clone(),
                store.clone(),
                objects.clone(),
                grants.clone(),
            )),
        ));

        let services = Arc::new(CoreServices {
            store: store.clone(),
            objects: objects.clone(),
            attachment_dir: dirs.data.join("attachments"),
            tools,
            policy,
            interaction: Some(interactions.clone()),
            tasks: Some(tasks.clone()),
            runtime_paths: Some(Arc::new(HostRuntimePaths(opts.runtime_paths.clone()))),
            model_resolver: Some(Arc::new(crate::dispatch::RouterModelResolver::new(
                router.clone(),
            ))),
            limits: Limits {
                max_rounds: config.limits.max_rounds,
                max_depth: config.limits.max_depth,
                max_result_chars: config.tools.max_result_chars,
                tool_timeout_secs: config.tools.timeout_secs,
                max_parallel_tools: config.limits.max_parallel_tools,
                task_wait_secs: config.limits.task_wait_secs,
            },
            context: ContextPolicy::default(),
        });

        let skills = Arc::new(crate::SkillLibrary::new(dirs.clone()));

        let prompts = Arc::new(
            crate::SystemPrompts::new(dirs.clone(), config.clone(), shell_dialect)
                .with_math_rendering(opts.renders_math),
        );

        let dispatcher = Arc::new(
            Dispatcher::new(
                store.clone(),
                hub.clone(),
                router.clone(),
                Arc::new(SessionLocks::new(store.clone(), opts.host)),
                services,
                workspaces.clone(),
                interactions.clone(),
            )
            .with_dirs(dirs.clone())
            .with_worktrees(worktrees.clone())
            .with_extensions(extensions.clone())
            // Auxiliary calls are isolated from the turn, prompt, transcript and tool pipeline.
            .with_auxiliary(auxiliary.clone())
            .with_prompts(prompts.clone())
            .with_skills(skills.clone())
            // `general` is always available; `agent:<name>` role entries add named profiles whose
            // model/thinking settings are resolved at the start of each turn.
            .with_agents(agent_profile_names(&config)),
        );
        let task_waker: Arc<dyn crate::task::TaskWake> = dispatcher.clone();
        tasks.bind_waker(Arc::downgrade(&task_waker));
        drop(task_waker);
        let task_agent_factory: Arc<dyn crate::task::ScheduledAgentFactory> = dispatcher.clone();
        tasks.bind_agent_factory(Arc::downgrade(&task_agent_factory));
        drop(task_agent_factory);
        // A task may commit its terminal state and mailbox notification immediately before the
        // process exits. The row is the recovery point: on the next boot, claim the normal session
        // lock and feed it through the same dispatcher path as live notifications.
        match store.with(|db| db.mailbox().pending_sessions()) {
            Ok(session_ids) => {
                if session_ids.is_empty() {
                    // Nothing to resume, and importantly no need for a Tokio runtime.
                } else if tokio::runtime::Handle::try_current().is_err() {
                    // A host may construct the engine before starting its runtime. Calling
                    // start_if_idle there would panic inside tokio::spawn; keep the durable rows
                    // untouched so the first later dispatch trigger can claim them.
                    warnings.push(format!(
                        "{} sessions have pending mailbox input; the current host has not \
                         started an async runtime yet, so they were kept for recovery",
                        session_ids.len()
                    ));
                } else {
                    for session_id in session_ids {
                        if let Err(error) = dispatcher.start_if_idle(session_id) {
                            warnings.push(format!(
                                "session {session_id} has pending mailbox input, but startup \
                                 recovery failed: {error}"
                            ));
                        }
                    }
                }
            }
            Err(error) => warnings.push(format!(
                "failed to scan mailboxes pending recovery: {error}"
            )),
        }

        let sessions = Arc::new(
            Sessions::new(
                store.clone(),
                objects.clone(),
                workspaces.clone(),
                dirs.data.join("attachments"),
            )
            .with_router(router.clone())
            .with_grants(grants.clone())
            .with_registry(dispatcher.registry().clone()),
        );

        let config_service = Arc::new(
            Config::new(
                config.clone(),
                dirs.clone(),
                credential_store.clone(),
                store.clone(),
                workspaces.clone(),
            )
            .with_bypass_flag(bypass_cell)
            .with_router(router.clone())
            .with_transport(transport.clone()),
        );
        let credential_service = Arc::new(Credentials::new(
            config_service.clone(),
            credential_store,
            transport,
        ));

        let engine = Engine::not_wired()
            .with_hub(hub)
            .with_workspaces(workspaces.clone())
            .with_workspace_files(files_service)
            .with_workspace_git(git_service)
            .with_auxiliary(auxiliary_service)
            .with_memories(memories)
            .with_agent_profiles(agent_profile_service)
            .with_sessions(sessions)
            .with_objects(objects_service)
            .with_turns(dispatcher)
            .with_config(config_service)
            .with_credentials(credential_service)
            .with_managed_resources(managed_resources)
            .with_extension_service(extension_service)
            .with_tasks(tasks.clone())
            .with_tool_catalog(tool_catalog);

        if opts.maintenance {
            let _ = crate::gc::spawn_detached(
                store.clone(),
                objects.clone(),
                chrono::Duration::hours(24),
                std::time::Duration::from_secs(30),
            );
        }

        Ok(Bootstrapped {
            engine,
            interactions,
            workspaces,
            store,
            objects,
            dirs,
            config,
            approval_mode,
            warnings,
            extensions,
            tasks,
            grants,
        })
    }
}

fn agent_profile_names(config: &AppConfig) -> Vec<String> {
    let mut names: Vec<String> = crate::agent_profile::BUILTIN_AGENTS
        .iter()
        .map(|n| (*n).to_string())
        .collect();
    let configured: Vec<String> = config
        .llm_roles
        .keys()
        .filter_map(|role| role.strip_prefix("agent:"))
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect();
    for name in configured {
        if !names.iter().any(|n| n == &name) {
            names.push(name);
        }
    }
    names.sort();
    names.dedup();
    names
}

pub(crate) fn tool_registry(
    config: &AppConfig,
    search_keys: Option<Arc<dyn SearchKeySource>>,
) -> (
    ToolRegistry,
    Option<zlogic_tools::ShellDialect>,
    Vec<String>,
) {
    let mut registry = ToolRegistry::with_builtins();
    let mut warnings = Vec::new();
    let mut shell_dialect = None;

    // The generic built-in registry has a platform-shaped shell so tests and lightweight hosts can
    // construct it without configuration. Production replaces it with the backend resolved from
    // config exactly once: definition text, policy dialect and process launch then all refer to the
    // same Shell value.
    registry.remove("shell");
    match Shell::resolve(shell_preference(config.tools.default_shell)) {
        Ok(shell) => {
            shell_dialect = Some(shell.dialect());
            tracing::info!(
                target: "zlogic::engine",
                backend = shell.backend_name(),
                "selected the default shell"
            );
            registry.add(Arc::new(shell));
        }
        Err(reason) => {
            warnings.push(format!(
                "shell tool is not enabled: {reason}. Change tools.default_shell or install \
                 the corresponding shell"
            ));
        }
    }

    let cfg = &config.tools.web_search;

    let mut settings = WebSearchSettings {
        provider: cfg.provider_id().as_deref().and_then(SearchProvider::parse),
        key_source: search_keys,
        ..WebSearchSettings::default()
    };

    if let Some(id) = cfg.provider_id() {
        let has_key =
            resolve_search_key(&SystemCredentialStore, config.auto_detect_env, &id).is_some();
        if !has_key {
            let upper = id.to_ascii_uppercase();
            let page = SearchProvider::parse(&id)
                .map(SearchProvider::key_page)
                .unwrap_or_default();
            warnings.push(format!(
                "web_search is set to {id}, but no key was found for it (ZLOGIC_{upper}_API_KEY / \
                 {upper}_API_KEY / keychain entry {entry}) — requests will fall back to the free \
                 tier, which has a very low quota. Set one in Settings → Tools → web search, or \
                 get one at {page}",
                entry = zlogic_credential::keyring_entry(&id),
            ));
        }
    }

    if let Some(u) = &cfg.exa_url {
        settings.exa_url = u.clone();
    }
    if let Some(u) = &cfg.parallel_url {
        settings.parallel_url = u.clone();
    }
    if cfg.timeout_secs > 0 {
        settings.timeout = std::time::Duration::from_secs(cfg.timeout_secs);
    }

    registry.add(Arc::new(zlogic_tools::WebSearch::new(settings)));
    (registry, shell_dialect, warnings)
}

fn shell_preference(preference: zlogic_config::ShellPreference) -> ToolShellPreference {
    match preference {
        zlogic_config::ShellPreference::Auto => ToolShellPreference::Auto,
        zlogic_config::ShellPreference::GitBash => ToolShellPreference::GitBash,
        zlogic_config::ShellPreference::Ps7 => ToolShellPreference::Ps7,
        zlogic_config::ShellPreference::Powershell => ToolShellPreference::Powershell,
        zlogic_config::ShellPreference::Cmd => ToolShellPreference::Cmd,
        zlogic_config::ShellPreference::Bash => ToolShellPreference::Bash,
    }
}

struct CredentialSearchKeys {
    store: Arc<dyn CredentialStore>,
    auto_detect_env: bool,
}

impl std::fmt::Debug for CredentialSearchKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialSearchKeys")
            .field("auto_detect_env", &self.auto_detect_env)
            .finish_non_exhaustive()
    }
}

impl SearchKeySource for CredentialSearchKeys {
    fn key(&self, backend: SearchProvider) -> Option<String> {
        resolve_search_key(&*self.store, self.auto_detect_env, backend.label())
    }
}

fn resolve_search_key(
    keys: &dyn CredentialStore,
    auto_detect_env: bool,
    id: &str,
) -> Option<String> {
    credential_candidates(id, auto_detect_env)
        .iter()
        .find_map(|c| keys.resolve(&c.to_string()))
        .filter(|v| !v.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EngineApi;

    fn opts(dir: &tempfile::TempDir) -> BootstrapOptions {
        let root = dir.path();
        BootstrapOptions::new("test")
            .dirs(Dirs {
                config: root.join("config"),
                data: root.join("data"),
                state: root.join("state"),
                cache: root.join("cache"),
            })
            .maintenance(false)
    }

    #[test]
    fn the_search_key_source_reaches_the_registered_tool() {
        #[derive(Debug)]
        struct OnlyParallel;

        impl SearchKeySource for OnlyParallel {
            fn key(&self, backend: SearchProvider) -> Option<String> {
                (backend == SearchProvider::Parallel).then(|| "sk-test".into())
            }
        }

        let config = AppConfig::default();
        assert!(
            config.tools.web_search.provider.is_none(),
            "this test is precisely about \"no named backend, the key decides\""
        );
        let (registry, _, _) = tool_registry(&config, Some(Arc::new(OnlyParallel)));
        let definition = registry.get("web_search").unwrap().definition();
        assert!(
            definition.description.contains("parallel"),
            "the key source was not wired up: {}",
            definition.description
        );
    }

    #[tokio::test]
    async fn every_service_is_wired_after_one_call() {
        let dir = tempfile::tempdir().unwrap();
        let boot = Engine::bootstrap(opts(&dir)).unwrap();

        let e = &boot.engine;
        assert!(
            !is_not_wired(e.workspace_list(false).await.err()),
            "workspaces"
        );
        assert!(!is_not_wired(e.config_get().await.err()), "config");
        assert!(
            !is_not_wired(
                e.memories
                    .list(zlogic_protocol::MemoryListReq {
                        scope: zlogic_protocol::MemoryScope::Global,
                        workspace_id: None,
                        include_removed: false,
                    })
                    .await
                    .err()
            ),
            "memory"
        );
        assert!(
            !is_not_wired(
                e.memories
                    .add(zlogic_protocol::MemoryAddReq {
                        scope: zlogic_protocol::MemoryScope::Global,
                        workspace_id: None,
                        category: zlogic_protocol::MemoryCategory::Preference,
                        fact: "concise replies".into(),
                        source_quote: "concise replies".into(),
                        source_session_id: None,
                        source_turn_id: None,
                    })
                    .await
                    .err()
            ),
            "memory add"
        );
        assert!(
            !is_not_wired(e.turns.state(zlogic_protocol::TurnId::new()).await.err()),
            "turns"
        );
    }

    #[tokio::test]
    async fn a_host_can_take_over_the_services_the_engine_keeps_seams_for() {
        let dir = tempfile::tempdir().unwrap();
        let extensions: Arc<dyn crate::service::ExtensionService> =
            Arc::new(crate::not_wired::NotWired);
        let resources: Arc<dyn crate::service::ManagedResourceService> =
            Arc::new(crate::not_wired::NotWired);
        let objects: Arc<dyn crate::service::ObjectService> = Arc::new(crate::not_wired::NotWired);
        let auxiliary: Arc<dyn crate::service::AuxiliaryService> =
            Arc::new(crate::not_wired::NotWired);
        let (handed_extensions, handed_resources) = (extensions.clone(), resources.clone());
        let (handed_objects, handed_auxiliary) = (objects.clone(), auxiliary.clone());

        let boot = Engine::bootstrap(opts(&dir).pro(Box::new(move |hosts| {
            assert!(
                hosts.dirs.data.join("state.db").exists(),
                "the host is handed live handles, not placeholders"
            );
            ProWiring {
                managed_resources: Some(handed_resources),
                extension_service: Some(handed_extensions),
                objects: Some(handed_objects),
                auxiliary: Some(handed_auxiliary),
                ..ProWiring::default()
            }
        })))
        .unwrap();

        assert!(
            Arc::ptr_eq(&boot.engine.extension_service, &extensions),
            "the engine kept its own extension administration after the host supplied one"
        );
        assert!(
            Arc::ptr_eq(&boot.engine.managed_resources, &resources),
            "the engine kept its own resource management after the host supplied one"
        );
        assert!(
            Arc::ptr_eq(&boot.engine.objects, &objects),
            "the engine kept its own object reading after the host supplied one"
        );
        assert!(
            Arc::ptr_eq(&boot.engine.auxiliary, &auxiliary),
            "the engine kept its own drafting after the host supplied one"
        );
    }

    fn is_not_wired(err: Option<zlogic_protocol::query::ApiError>) -> bool {
        err.is_some_and(|error| error.category == zlogic_protocol::ErrorCategory::NotWired)
    }

    #[tokio::test]
    async fn it_creates_its_directories_and_stores() {
        let dir = tempfile::tempdir().unwrap();
        let boot = Engine::bootstrap(opts(&dir)).unwrap();

        assert!(boot.dirs.data.join("state.db").exists());
        assert!(boot.dirs.data.join("objects").exists());
    }

    #[test]
    fn it_bootstraps_outside_a_tokio_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let boot = Engine::bootstrap(opts(&dir).maintenance(true)).unwrap();
        assert!(boot.dirs.data.join("state.db").exists());
    }

    #[tokio::test]
    async fn approval_mode_is_the_union_of_the_flag_and_the_config() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!Engine::bootstrap(opts(&dir)).unwrap().approval_mode);
        assert!(
            Engine::bootstrap(opts(&dir).approval_mode(true))
                .unwrap()
                .approval_mode
        );
    }

    #[tokio::test]
    async fn configuration_warnings_come_back_as_values() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_dir = dir.path().join("config");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(
            cfg_dir.join("config.yaml"),
            "tools:\n  web_search:\n    provider: exa\n",
        )
        .unwrap();

        let boot = Engine::bootstrap(opts(&dir)).unwrap();
        if std::env::var("EXA_API_KEY").is_err() && std::env::var("ZLOGIC_EXA_API_KEY").is_err() {
            assert!(
                boot.warnings
                    .iter()
                    .any(|w| w.contains("web_search") && w.contains("key")),
                "a missing key must leave a trace, otherwise the user only notices once the 429s start: {:?}",
                boot.warnings
            );
        }
    }

    #[tokio::test]
    async fn a_misspelled_search_provider_is_refused_with_the_field_name() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_dir = dir.path().join("config");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(
            cfg_dir.join("config.yaml"),
            "tools:\n  web_search:\n    provider: exaa\n",
        )
        .unwrap();

        match Engine::bootstrap(opts(&dir)) {
            Err(BootstrapError::Config { reason, .. }) => {
                assert!(reason.contains("web_search.provider"), "{reason}");
            }
            other => panic!("expected a Config error, got {:?}", other.map(|_| "ok")),
        }
    }

    #[tokio::test]
    async fn the_worktree_directory_is_configurable_and_defaults_to_a_sibling() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            Engine::bootstrap(opts(&dir)).unwrap().config.worktree.dir,
            "../{workspace}-worktrees"
        );

        let other = tempfile::tempdir().unwrap();
        let cfg_dir = other.path().join("config");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(
            cfg_dir.join("config.yaml"),
            "worktree:\n  dir: ~/wt/{workspace}\n",
        )
        .unwrap();
        assert_eq!(
            Engine::bootstrap(opts(&other)).unwrap().config.worktree.dir,
            "~/wt/{workspace}"
        );
    }

    #[test]
    fn the_configured_shell_reaches_the_definition_seen_by_the_model() {
        let mut config = AppConfig::default();
        #[cfg(windows)]
        {
            config.tools.default_shell = zlogic_config::ShellPreference::Cmd;
        }
        #[cfg(not(windows))]
        {
            config.tools.default_shell = zlogic_config::ShellPreference::Bash;
        }

        let (registry, dialect, warnings) = tool_registry(&config, None);
        assert!(
            warnings
                .iter()
                .all(|w| !w.contains("shell tool is not enabled")),
            "{warnings:?}"
        );
        assert!(dialect.is_some());
        let definition = registry.get("shell").unwrap().definition();
        #[cfg(windows)]
        assert!(definition.description.contains("Command Prompt"));
        #[cfg(not(windows))]
        assert!(definition.description.contains("Bash"));
    }

    #[tokio::test]
    async fn a_broken_config_names_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_dir = dir.path().join("config");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(
            cfg_dir.join("config.yaml"),
            "providers: [this is not a map\n",
        )
        .unwrap();

        match Engine::bootstrap(opts(&dir)) {
            Err(BootstrapError::Config { path, .. }) => {
                assert!(path.ends_with("config.yaml"), "{path}");
            }
            other => panic!("expected a Config error, got {:?}", other.map(|_| "ok")),
        }
    }

    #[tokio::test]
    async fn the_two_hosts_differ_only_in_who_holds_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let cli = Engine::bootstrap(opts(&dir)).unwrap();
        let desktop = Engine::bootstrap(BootstrapOptions {
            host: "desktop",
            ..opts(&dir)
        })
        .unwrap();

        assert_eq!(cli.dirs.data, desktop.dirs.data);
        assert!(!is_not_wired(cli.engine.config_get().await.err()));
        assert!(!is_not_wired(desktop.engine.config_get().await.err()));
    }
}
