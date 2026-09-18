//! # zlogic-engine
//!
//! The in-process dispatch layer. A host talks to this, not to the pieces underneath it.
//!
//! ```text
//! host (the CLI, or another front end) ──> Engine ──> session / workspace / usage / config services
//!                                              └──> core (runs a turn) ──> llm
//! ```
//!
//! # The host contract
//!
//! [`EngineApi`] is what a host compiles against: what a session in a terminal needs — open a
//! session, submit and steer its turns, read the transcript and the usage, read and write provider
//! configuration and credentials, list workspaces. A front end with more surface than that (a file
//! browser, a git panel, a task centre) is built on the same [`Engine`] and the same [`service`]
//! traits rather than by widening this trait: the smaller the contract, the more implementations of
//! it are possible — a transport proxy, a test double, a headless runner.
//!
//! # Why hosts do not call the services directly
//!
//! Flattening an operation like `session_list` onto a front end is convenient, but its *boundary* —
//! its transaction scope, its lock order, which data has to come from one snapshot — would then be
//! frozen into the protocol. [`EngineApi::session_open`] returns the session together with its
//! workspace, its working directory and the live turn state precisely because fetching those
//! separately tears them apart; if a host called four services itself, there would be nowhere left
//! to state that constraint.
//!
//! The services are the engine's internal structure: they can be merged, split or replaced without
//! a host noticing.
//!
//! [`hub`] is the other half of the boundary. Turn-event fan-out is transport, not execution, and
//! the engine owns that edge from the start.

pub mod agent_profile;
pub mod auxiliary;
pub mod bootstrap;
pub mod budget;
pub mod config;
pub mod credentials;
pub mod dispatch;
pub mod extensions;
pub mod gc;
pub mod grants;
pub mod hub;
pub mod interaction;
pub mod lifecycle;
pub mod lock;
pub mod memory;
pub mod not_wired;
pub mod policy;
pub mod prompt;
pub mod router;
pub mod service;
pub mod sessions;
pub mod skills;
pub mod store_call;
pub mod task;
pub mod tool_catalog;
pub mod transcript;
pub mod usage;
pub mod usage_cache;
pub mod workspaces;
pub mod worktree;

use std::sync::Arc;

use zlogic_protocol::query::{
    ApiResult, ConfigRemoveProviderReq, ConfigView, CredentialDeleteReq, CredentialSetReq,
    CredentialState, CredentialVerifyReq, CredentialVerifyResult, EntriesReq,
    OpenAiCompatibleProviderReq, Page, ProviderCatalog, SessionListReq, SessionOpenReq,
    SessionOpened, SessionRenameReq, SessionSearchHit, SessionSearchReq, SessionSummary,
    TranscriptEntry, TranscriptReq, TurnItem, TurnsReq, UsageSummary, UsageSummaryReq,
    WorkspaceSelector, WorkspaceSummary,
};
use zlogic_protocol::{Command, SessionId, Submission, SubmitAck, WorkspaceId};

use crate::hub::EventHub;
use crate::service::{
    AgentProfileService, AuxiliaryService, ConfigService, CredentialService, ExtensionService,
    ManagedResourceService, MemoryService, ObjectService, SessionService, TaskService,
    ToolCatalogService, TurnService, WorkspaceFilesService, WorkspaceGitService, WorkspaceService,
};

pub use agent_profile::{BUILTIN_AGENTS, builtin_system_prompt};
pub use auxiliary::Auxiliary;
pub use config::Config;
pub use credentials::Credentials;
pub use dispatch::{Dispatcher, HubSink, TurnRegistry, WorkspaceRoots};
pub use extensions::{Assembled, Extensions};
pub use grants::{GrantEntry, Grants, derive_rule, derive_rule_shape, grant_preview};
pub use interaction::{EngineInteractions, InteractionRouter};
pub use lifecycle::{Lifecycle, Reconciled};
pub use lock::{LockGuard, SessionLocks};
pub use memory::Memories;
pub use policy::{BypassFlag, BypassGate, PolicyCoreGate};
pub use prompt::{PromptRequest, SystemPrompts};
pub use router::{ModelRouter, Routed};
pub use sessions::Sessions;
pub use skills::{SessionSkills, SkillLibrary};
pub use task::TaskManager;
pub use tool_catalog::ToolCatalog;
pub use workspaces::Workspaces;
pub use worktree::{SessionWorktree, Worktrees};
pub use zlogic_credential::{AwsCredentials, CredentialStore, SystemCredentialStore};

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Config(#[from] zlogic_config::ConfigError),
    #[error(transparent)]
    Store(#[from] zlogic_store::StoreError),
    #[error(transparent)]
    Core(#[from] zlogic_core::CoreError),
    #[error(transparent)]
    Task(#[from] zlogic_task::StoreError),
    #[error("no available model for role {role} (tried: {})", tried.join(", "))]
    NoModel { role: String, tried: Vec<String> },
    #[error("could not get credential {credential_ref} for {model}")]
    NoCredential {
        model: String,
        credential_ref: String,
    },
    #[error("session {session_id} already has a running turn")]
    Busy { session_id: String },
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(String),
    #[error("not found: {0}")]
    NotFound(String),
}

pub type Result<T> = std::result::Result<T, EngineError>;

impl From<EngineError> for zlogic_protocol::query::ApiError {
    fn from(e: EngineError) -> Self {
        use zlogic_protocol::query::ApiError;
        match &e {
            EngineError::Busy { session_id } => ApiError::conflict_code(
                "session_busy",
                format!("session {session_id} already has a running turn"),
            )
            .with_detail("session_id", session_id.clone()),
            EngineError::NoModel { role, tried } => {
                ApiError::unavailable("model_unavailable", e.to_string())
                    .with_detail("role", role.clone())
                    .with_detail("tried", serde_json::json!(tried))
            }
            EngineError::NoCredential {
                model,
                credential_ref,
            } => ApiError::unavailable("credential_missing", e.to_string())
                .with_detail("model", model.clone())
                .with_detail("credential_ref", credential_ref.clone()),
            EngineError::Invalid(m) => ApiError::invalid_code("engine_invalid_argument", m.clone()),
            EngineError::Conflict(m) => ApiError::conflict_code("engine_state_conflict", m.clone()),
            EngineError::Config(zlogic_config::ConfigError::Invalid(m)) => {
                ApiError::invalid_code("config_invalid", m.clone())
            }
            EngineError::NotFound(m) => ApiError::not_found("resource", m),
            EngineError::Store(zlogic_store::StoreError::NotFound { kind, id }) => {
                ApiError::not_found(*kind, id)
            }
            EngineError::Store(zlogic_store::StoreError::NoSuchSession(id)) => {
                ApiError::not_found("session", id.to_string())
            }
            EngineError::Config(_)
            | EngineError::Store(_)
            | EngineError::Core(_)
            | EngineError::Task(_) => {
                let error = ApiError::internal(&e);
                tracing::error!(
                    target: "zlogic::engine",
                    code = %error.code,
                    incident_id = error.incident_id.as_deref().unwrap_or_default(),
                    error = %e,
                    "engine API failure"
                );
                error
            }
        }
    }
}

#[async_trait::async_trait]
pub trait EngineApi: Send + Sync {
    /// Read an existing workspace without registering, touching, or unhiding it.
    async fn workspace_get(&self, workspace_id: WorkspaceId) -> ApiResult<WorkspaceSummary>;

    async fn workspace_list(&self, include_hidden: bool) -> ApiResult<Vec<WorkspaceSummary>>;

    // ── session ──
    async fn session_list(&self, req: SessionListReq) -> ApiResult<Page<SessionSummary>>;
    async fn session_open(&self, req: SessionOpenReq) -> ApiResult<SessionOpened>;
    async fn session_rename(&self, req: SessionRenameReq) -> ApiResult<SessionSummary>;
    async fn session_delete(&self, session_id: SessionId) -> ApiResult<()>;
    async fn session_set_model(
        &self,
        session_id: SessionId,
        model_ref: String,
    ) -> ApiResult<SessionSummary>;
    async fn session_search(&self, req: SessionSearchReq) -> ApiResult<Vec<SessionSearchHit>>;
    async fn session_transcript(&self, req: TranscriptReq) -> ApiResult<Page<TranscriptEntry>>;
    async fn session_turns(&self, req: TurnsReq) -> ApiResult<Page<TurnItem>>;
    async fn session_entries(&self, req: EntriesReq) -> ApiResult<Page<TranscriptEntry>>;

    // ── turn ──
    async fn submit(&self, submission: Submission) -> ApiResult<SubmitAck>;
    async fn control(&self, command: Command) -> ApiResult<()>;
    async fn cancel_all_turns(&self) -> ApiResult<usize> {
        Ok(0)
    }
    async fn live_turn_count(&self) -> ApiResult<usize> {
        Ok(0)
    }

    // ── usage ──
    async fn usage_summary(&self, req: UsageSummaryReq) -> ApiResult<UsageSummary>;

    // ── config ──
    async fn config_get(&self) -> ApiResult<ConfigView>;
    async fn config_reload(&self) -> ApiResult<ConfigView>;
    async fn config_upsert_openai_compatible(
        &self,
        req: OpenAiCompatibleProviderReq,
    ) -> ApiResult<ConfigView>;
    async fn config_remove_provider(&self, req: ConfigRemoveProviderReq) -> ApiResult<ConfigView>;
    async fn config_catalog(&self) -> ApiResult<ProviderCatalog>;
    // ── credential ──
    async fn credential_list(&self) -> ApiResult<Vec<CredentialState>>;
    async fn credential_set(&self, req: CredentialSetReq) -> ApiResult<CredentialState>;
    async fn credential_delete(&self, req: CredentialDeleteReq) -> ApiResult<CredentialState>;
    async fn credential_verify(
        &self,
        req: CredentialVerifyReq,
    ) -> ApiResult<CredentialVerifyResult>;
}

#[derive(Clone)]
pub struct Engine {
    pub auxiliary: Arc<dyn AuxiliaryService>,
    pub memories: Arc<dyn MemoryService>,
    pub agent_profiles: Arc<dyn AgentProfileService>,
    pub sessions: Arc<dyn SessionService>,
    pub workspaces: Arc<dyn WorkspaceService>,
    pub files: Arc<dyn WorkspaceFilesService>,
    pub git: Arc<dyn WorkspaceGitService>,
    pub turns: Arc<dyn TurnService>,
    pub objects: Arc<dyn ObjectService>,
    pub config: Arc<dyn ConfigService>,
    pub credentials: Arc<dyn CredentialService>,
    pub managed_resources: Arc<dyn ManagedResourceService>,
    pub extension_service: Arc<dyn ExtensionService>,
    pub tasks: Arc<dyn TaskService>,
    pub tool_catalog: Arc<dyn ToolCatalogService>,
    pub hub: Arc<EventHub>,
}

impl Engine {
    pub fn not_wired() -> Self {
        Self {
            auxiliary: Arc::new(not_wired::NotWired),
            memories: Arc::new(not_wired::NotWired),
            agent_profiles: Arc::new(not_wired::NotWired),
            sessions: Arc::new(not_wired::NotWired),
            workspaces: Arc::new(not_wired::NotWired),
            files: Arc::new(not_wired::NotWired),
            git: Arc::new(not_wired::NotWired),
            turns: Arc::new(not_wired::NotWired),
            objects: Arc::new(not_wired::NotWired),
            config: Arc::new(not_wired::NotWired),
            credentials: Arc::new(not_wired::NotWired),
            managed_resources: Arc::new(not_wired::NotWired),
            extension_service: Arc::new(not_wired::NotWired),
            tasks: Arc::new(not_wired::NotWired),
            tool_catalog: Arc::new(not_wired::NotWired),
            hub: Arc::new(EventHub::new()),
        }
    }

    pub fn with_memories(mut self, memories: Arc<dyn MemoryService>) -> Self {
        self.memories = memories;
        self
    }

    pub fn with_agent_profiles(mut self, agent_profiles: Arc<dyn AgentProfileService>) -> Self {
        self.agent_profiles = agent_profiles;
        self
    }

    pub fn with_auxiliary(mut self, auxiliary: Arc<dyn AuxiliaryService>) -> Self {
        self.auxiliary = auxiliary;
        self
    }

    pub fn with_tool_catalog(mut self, tool_catalog: Arc<dyn ToolCatalogService>) -> Self {
        self.tool_catalog = tool_catalog;
        self
    }

    pub fn with_workspaces(mut self, workspaces: Arc<dyn WorkspaceService>) -> Self {
        self.workspaces = workspaces;
        self
    }

    pub fn with_workspace_files(mut self, files: Arc<dyn WorkspaceFilesService>) -> Self {
        self.files = files;
        self
    }

    pub fn with_workspace_git(mut self, git: Arc<dyn WorkspaceGitService>) -> Self {
        self.git = git;
        self
    }

    pub fn with_objects(mut self, objects: Arc<dyn ObjectService>) -> Self {
        self.objects = objects;
        self
    }

    pub fn with_sessions(mut self, sessions: Arc<dyn SessionService>) -> Self {
        self.sessions = sessions;
        self
    }

    pub fn with_config(mut self, config: Arc<dyn ConfigService>) -> Self {
        self.config = config;
        self
    }

    pub fn with_credentials(mut self, credentials: Arc<dyn CredentialService>) -> Self {
        self.credentials = credentials;
        self
    }

    pub fn with_managed_resources(mut self, resources: Arc<dyn ManagedResourceService>) -> Self {
        self.managed_resources = resources;
        self
    }

    pub fn with_extension_service(mut self, service: Arc<dyn ExtensionService>) -> Self {
        self.extension_service = service;
        self
    }

    pub fn with_tasks(mut self, tasks: Arc<dyn TaskService>) -> Self {
        self.tasks = tasks;
        self
    }

    pub fn with_turns(mut self, turns: Arc<dyn TurnService>) -> Self {
        self.turns = turns;
        self
    }

    pub fn with_hub(mut self, hub: Arc<EventHub>) -> Self {
        self.hub = hub;
        self
    }
}

#[async_trait::async_trait]
impl EngineApi for Engine {
    async fn workspace_get(&self, workspace_id: WorkspaceId) -> ApiResult<WorkspaceSummary> {
        self.workspaces
            .get(WorkspaceSelector::Id { workspace_id })
            .await
    }

    async fn workspace_list(&self, include_hidden: bool) -> ApiResult<Vec<WorkspaceSummary>> {
        self.workspaces.list(include_hidden).await
    }

    async fn session_list(&self, req: SessionListReq) -> ApiResult<Page<SessionSummary>> {
        self.sessions.list(req).await
    }

    async fn session_open(&self, req: SessionOpenReq) -> ApiResult<SessionOpened> {
        self.sessions.open(req).await
    }

    async fn session_rename(&self, req: SessionRenameReq) -> ApiResult<SessionSummary> {
        self.sessions.rename(req).await
    }

    async fn session_delete(&self, session_id: SessionId) -> ApiResult<()> {
        self.sessions.delete(session_id).await?;
        self.hub.drop_session(&session_id.to_string());
        Ok(())
    }

    async fn session_set_model(
        &self,
        session_id: SessionId,
        model_ref: String,
    ) -> ApiResult<SessionSummary> {
        self.sessions.set_model(session_id, model_ref).await
    }

    async fn session_search(&self, req: SessionSearchReq) -> ApiResult<Vec<SessionSearchHit>> {
        self.sessions.search(req).await
    }

    async fn session_transcript(&self, req: TranscriptReq) -> ApiResult<Page<TranscriptEntry>> {
        self.sessions.transcript(req).await
    }

    async fn session_turns(&self, req: TurnsReq) -> ApiResult<Page<TurnItem>> {
        self.sessions.turns(req).await
    }

    async fn session_entries(&self, req: EntriesReq) -> ApiResult<Page<TranscriptEntry>> {
        self.sessions.entries(req).await
    }

    async fn submit(&self, submission: Submission) -> ApiResult<SubmitAck> {
        self.turns.submit(submission).await
    }

    async fn control(&self, command: Command) -> ApiResult<()> {
        self.turns.control(command).await
    }

    async fn cancel_all_turns(&self) -> ApiResult<usize> {
        self.turns.cancel_all_turns().await
    }

    async fn live_turn_count(&self) -> ApiResult<usize> {
        self.turns.live_turn_count().await
    }

    async fn usage_summary(&self, req: UsageSummaryReq) -> ApiResult<UsageSummary> {
        self.config.usage_summary(req).await
    }

    async fn config_get(&self) -> ApiResult<ConfigView> {
        self.config.get().await
    }

    async fn config_reload(&self) -> ApiResult<ConfigView> {
        self.config.reload().await
    }

    async fn config_upsert_openai_compatible(
        &self,
        req: OpenAiCompatibleProviderReq,
    ) -> ApiResult<ConfigView> {
        self.config.upsert_openai_compatible(req).await
    }

    async fn config_remove_provider(&self, req: ConfigRemoveProviderReq) -> ApiResult<ConfigView> {
        self.config.remove_provider(req).await
    }

    async fn config_catalog(&self) -> ApiResult<ProviderCatalog> {
        self.config.catalog().await
    }

    async fn credential_list(&self) -> ApiResult<Vec<CredentialState>> {
        self.credentials.list().await
    }

    async fn credential_set(&self, req: CredentialSetReq) -> ApiResult<CredentialState> {
        self.credentials.set(req).await
    }

    async fn credential_delete(&self, req: CredentialDeleteReq) -> ApiResult<CredentialState> {
        self.credentials.delete(req).await
    }

    async fn credential_verify(
        &self,
        req: CredentialVerifyReq,
    ) -> ApiResult<CredentialVerifyResult> {
        self.credentials.verify(req).await
    }
}
