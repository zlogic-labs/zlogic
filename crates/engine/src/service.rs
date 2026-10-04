use async_trait::async_trait;
use tokio::sync::mpsc;
use zlogic_protocol::extensions::{
    ExtensionCatalog, ExtensionCatalogReq, ExtensionDescriptor, ExtensionInspectReq,
    ExtensionInstallPlan, ExtensionInstallReq, ExtensionRemoveReq, ExtensionSetEnabledReq,
    McpImportReq, McpImportResult, McpLogoutReq, McpOAuthBeginReq, McpOAuthBeginResult,
    McpOAuthCancelReq, McpOAuthStatusReq, McpOAuthStatusResult, McpSetKeyReq,
    McpSetOAuthClientSecretReq, McpSetScopeReq, McpSetTokenReq, McpSetupReq, McpSetupResult,
    McpTestReq, McpTestResult, McpUpsertReq,
};
use zlogic_protocol::query::{
    ApiResult, CatalogCheck, ConfigRemoveProviderReq, ConfigUpdateReq, ConfigView,
    CredentialDeleteReq, CredentialSetReq, CredentialState, CredentialVerifyReq,
    CredentialVerifyResult, EntriesReq, EnvGetReq, EnvSetReq, EnvView, ObjectData, ObjectDataReq,
    ObjectRange, ObjectRangeReq, ObjectReadReq, ObjectText, OpenAiCompatibleProviderReq, Page,
    ProviderCatalog, ProviderModels,
    ProviderModelsReq, ProviderSignInBegin, ProviderSignInBeginReq, ProviderSignInCancelReq,
    ProviderSignInStatus, ProviderSignInStatusReq, RuntimeTask, RuntimeTaskDeleteReq,
    RuntimeTaskListReq, RuntimeTaskLog, RuntimeTaskLogReq, RuntimeTaskPage, RuntimeTaskStopReq,
    SessionForkReq, SessionListReq, SessionOpenReq, SessionOpened, SessionRenameReq,
    SessionSearchHit, SessionSearchReq, SessionSummary, TaskJob, TaskJobCreateReq,
    TaskJobDeleteReq, TaskJobDraft, TaskJobDraftReq, TaskJobListReq, TaskJobRunReq, TaskJobRunsReq,
    TaskJobSetEnabledReq,
    TextCompleteStreamReq, TextDelta, TextTranslateReq, TextTranslateResp, ToolInfo,
    TranscriptEntry, TranscriptReq, TranslationDeleteReq, TranslationEntry, TranslationListReq,
    TurnItem, TurnState, TurnsReq, UsageSummary, UsageSummaryReq, WorkspaceCheckpoint,
    WorkspaceCheckpointCaptureReq, WorkspaceCheckpointClearReq, WorkspaceCheckpointCleared,
    WorkspaceCheckpointFileDiff, WorkspaceCheckpointFileDiffReq, WorkspaceCheckpointList,
    WorkspaceCheckpointListReq, WorkspaceCheckpointPlan, WorkspaceCheckpointPlanReq,
    WorkspaceCheckpointRestore, WorkspaceCheckpointRestoreReq, WorkspaceCheckpointStep,
    WorkspaceCheckpointStepReq, WorkspaceCheckpointSteps,
    WorkspaceCheckpointStepsReq, WorkspaceFileBase64, WorkspaceFileCreateReq,
    WorkspaceFileDeleteReq, WorkspaceFileEntry, WorkspaceFileListReq, WorkspaceFileRange,
    WorkspaceFileRangeReq, WorkspaceFileReadReq, WorkspaceFileRenameReq, WorkspaceFileSearchReq,
    WorkspaceFileText, WorkspaceFileWriteReq, WorkspaceGitBranchReq, WorkspaceGitCommitDetail,
    WorkspaceGitCommitDetailReq, WorkspaceGitCommitReq, WorkspaceGitDiff, WorkspaceGitDiffReq,
    WorkspaceGitGenerateCommitMessageReq, WorkspaceGitInfo, WorkspaceGitInitReq,
    WorkspaceGitOverview, WorkspaceGitOverviewReq, WorkspaceGitStageReq, WorkspaceGitSyncReq,
    WorkspaceGitTrustReq, WorkspaceKind, WorkspaceSelector, WorkspaceSummary, WorkspaceUpdateReq,
};
use zlogic_protocol::usage::QuotaStatus;
use zlogic_protocol::{
    AgentProfile, AgentProfileCreateReq, AgentProfileDeleteReq, AgentProfileListReq,
    AgentProfileListRes, AgentProfileUpdateReq, Command, MemoryAddReq, MemoryEditReq,
    MemoryListReq, MemoryRecord, MemoryRemoveReq, SessionId, Submission, SubmitAck, TurnId,
    WorkspaceId,
};

#[async_trait]
pub trait AuxiliaryService: Send + Sync {
    async fn generate_commit_message(
        &self,
        req: WorkspaceGitGenerateCommitMessageReq,
    ) -> ApiResult<String>;
    async fn draft_task_job(&self, req: TaskJobDraftReq) -> ApiResult<TaskJobDraft>;
    async fn text_translate(&self, req: TextTranslateReq) -> ApiResult<TextTranslateResp>;
    /// A completion whose answer is handed over as it arrives. The receiver ends when the call
    /// does; an error inside the call arrives as its last item.
    async fn text_complete_stream(
        &self,
        req: TextCompleteStreamReq,
    ) -> ApiResult<mpsc::UnboundedReceiver<TextDelta>>;
}

/// Quick translate's history: what was translated before, and the cache that comes with it.
///
/// Split from [`AuxiliaryService`], which is the model path. These three never call a model — they
/// are the store half of the same feature, and the closed half implements both on one object
/// because one feature owns them.
#[async_trait]
pub trait TranslationService: Send + Sync {
    async fn translation_list(&self, req: TranslationListReq) -> ApiResult<Vec<TranslationEntry>>;
    async fn translation_delete(&self, req: TranslationDeleteReq) -> ApiResult<()>;
    async fn translation_clear(&self) -> ApiResult<()>;
}

#[async_trait]
pub trait MemoryService: Send + Sync {
    async fn list(&self, req: MemoryListReq) -> ApiResult<Vec<MemoryRecord>>;
    async fn add(&self, req: MemoryAddReq) -> ApiResult<MemoryRecord>;
    async fn update(&self, req: MemoryEditReq) -> ApiResult<MemoryRecord>;
    async fn remove(&self, req: MemoryRemoveReq) -> ApiResult<MemoryRecord>;
}

#[async_trait]
pub trait AgentProfileService: Send + Sync {
    async fn list(&self, req: AgentProfileListReq) -> ApiResult<AgentProfileListRes>;
    async fn create(&self, req: AgentProfileCreateReq) -> ApiResult<AgentProfile>;
    async fn update(&self, req: AgentProfileUpdateReq) -> ApiResult<AgentProfile>;
    async fn delete(&self, req: AgentProfileDeleteReq) -> ApiResult<()>;
}

#[async_trait]
pub trait SessionService: Send + Sync {
    async fn list(&self, req: SessionListReq) -> ApiResult<Page<SessionSummary>>;

    async fn open(&self, req: SessionOpenReq) -> ApiResult<SessionOpened>;

    async fn rename(&self, req: SessionRenameReq) -> ApiResult<SessionSummary>;

    async fn fork(&self, req: SessionForkReq) -> ApiResult<SessionSummary>;

    async fn delete(&self, session_id: SessionId) -> ApiResult<()>;

    async fn set_model(
        &self,
        session_id: SessionId,
        model_ref: String,
    ) -> ApiResult<SessionSummary>;

    async fn set_effort(&self, session_id: SessionId, effort: String) -> ApiResult<SessionSummary>;

    async fn search(&self, req: SessionSearchReq) -> ApiResult<Vec<SessionSearchHit>>;

    async fn transcript(&self, req: TranscriptReq) -> ApiResult<Page<TranscriptEntry>>;

    async fn turns(&self, req: TurnsReq) -> ApiResult<Page<TurnItem>>;

    async fn entries(&self, req: EntriesReq) -> ApiResult<Page<TranscriptEntry>>;
}

#[async_trait]
pub trait WorkspaceService: Send + Sync {
    async fn resolve(
        &self,
        sel: WorkspaceSelector,
        name: Option<String>,
        kind: Option<WorkspaceKind>,
    ) -> ApiResult<WorkspaceSummary>;

    async fn get(&self, sel: WorkspaceSelector) -> ApiResult<WorkspaceSummary>;
    async fn list(&self, include_hidden: bool) -> ApiResult<Vec<WorkspaceSummary>>;

    async fn update(&self, req: WorkspaceUpdateReq) -> ApiResult<WorkspaceSummary>;

    async fn create_chat(&self, name: String) -> ApiResult<WorkspaceSummary>;

    async fn delete(&self, workspace_id: WorkspaceId) -> ApiResult<()>;
}

/// The file half of the workspace surface: listing, search, reading and writing *inside* a
/// registered root.
///
/// It is split from [`WorkspaceService`] because the engine never touches a workspace's files
/// itself — the turn path works through tools — while the closed half implements this on top of
/// the registry: it asks for the root and then works on the filesystem below it.
#[async_trait]
pub trait WorkspaceFilesService: Send + Sync {
    async fn file_list(&self, req: WorkspaceFileListReq) -> ApiResult<Vec<WorkspaceFileEntry>>;
    async fn file_search(&self, req: WorkspaceFileSearchReq) -> ApiResult<Vec<WorkspaceFileEntry>>;
    async fn file_read(&self, req: WorkspaceFileReadReq) -> ApiResult<WorkspaceFileText>;
    async fn file_read_base64(&self, req: WorkspaceFileReadReq) -> ApiResult<WorkspaceFileBase64>;
    async fn file_range(&self, req: WorkspaceFileRangeReq) -> ApiResult<WorkspaceFileRange>;
    async fn file_write(&self, req: WorkspaceFileWriteReq) -> ApiResult<WorkspaceFileText>;
    async fn file_create(&self, req: WorkspaceFileCreateReq) -> ApiResult<()>;
    async fn file_rename(&self, req: WorkspaceFileRenameReq) -> ApiResult<()>;
    async fn file_delete(&self, req: WorkspaceFileDeleteReq) -> ApiResult<()>;
}

/// The Git half of the workspace surface: the status overview a sidebar shows, the commit, stage,
/// sync and branch actions, and a commit's detail and diff.
///
/// Split from [`WorkspaceService`] for the same reason as [`WorkspaceFilesService`].
#[async_trait]
pub trait WorkspaceGitService: Send + Sync {
    async fn git_info(&self, sel: WorkspaceSelector) -> ApiResult<WorkspaceGitInfo>;
    async fn git_overview(&self, req: WorkspaceGitOverviewReq) -> ApiResult<WorkspaceGitOverview>;
    async fn git_commit(&self, req: WorkspaceGitCommitReq) -> ApiResult<WorkspaceGitOverview>;
    async fn git_stage(&self, req: WorkspaceGitStageReq) -> ApiResult<WorkspaceGitOverview>;
    async fn git_init(&self, req: WorkspaceGitInitReq) -> ApiResult<WorkspaceGitOverview>;
    /// Trust a repository zlogic was refused: see [`WorkspaceGitTrustReq`]. Returns the overview
    /// as it reads once the trust is in place, so a caller can show the result without re-reading.
    async fn git_trust(&self, req: WorkspaceGitTrustReq) -> ApiResult<WorkspaceGitOverview>;
    async fn git_sync(&self, req: WorkspaceGitSyncReq) -> ApiResult<WorkspaceGitOverview>;
    async fn git_branch(&self, req: WorkspaceGitBranchReq) -> ApiResult<WorkspaceGitOverview>;
    async fn git_commit_detail(
        &self,
        req: WorkspaceGitCommitDetailReq,
    ) -> ApiResult<WorkspaceGitCommitDetail>;
    async fn git_diff(&self, req: WorkspaceGitDiffReq) -> ApiResult<WorkspaceGitDiff>;
}

/// The restore-point timeline: what snapshots exist for a workspace, what restoring one would
/// change, a manual point, and the restore itself.
///
/// Split from [`WorkspaceGitService`] because it is a different thing wearing git's clothes — the
/// snapshots live in zlogic's own data directory, not in the user's repository, and writing one
/// never touches their `HEAD` or index. A host that cannot render the timeline still wants the
/// captures, and those do not go through here: they are taken by the turn itself.
#[async_trait]
pub trait WorkspaceCheckpointsService: Send + Sync {
    async fn checkpoint_list(
        &self,
        req: WorkspaceCheckpointListReq,
    ) -> ApiResult<WorkspaceCheckpointList>;
    /// What restoring `id` would do. Cheap enough to call on expand, expensive enough not to call
    /// for every row in a list.
    async fn checkpoint_plan(
        &self,
        req: WorkspaceCheckpointPlanReq,
    ) -> ApiResult<WorkspaceCheckpointPlan>;
    /// The step that ended at a point, against the point before it. Its own call because it
    /// needs no working tree: two commits the store already holds are the whole input.
    async fn checkpoint_step(
        &self,
        req: WorkspaceCheckpointStepReq,
    ) -> ApiResult<WorkspaceCheckpointStep>;
    /// The same step for several points at once, without the file rows. A list view that shows a
    /// number per row asks for its rows together; making it a call per row is what makes such a
    /// view fill in one row at a time.
    async fn checkpoint_steps(
        &self,
        req: WorkspaceCheckpointStepsReq,
    ) -> ApiResult<WorkspaceCheckpointSteps>;
    /// The patch for one file of a plan. Its own call because it is per row and per file: a plan
    /// for a thousand changed files must not carry a thousand patches.
    async fn checkpoint_diff(
        &self,
        req: WorkspaceCheckpointFileDiffReq,
    ) -> ApiResult<WorkspaceCheckpointFileDiff>;
    /// A point the user asked for by hand.
    async fn checkpoint_capture(
        &self,
        req: WorkspaceCheckpointCaptureReq,
    ) -> ApiResult<WorkspaceCheckpoint>;
    async fn checkpoint_restore(
        &self,
        req: WorkspaceCheckpointRestoreReq,
    ) -> ApiResult<WorkspaceCheckpointRestore>;
    /// Deletes every snapshot of this workspace, after the user confirmed it twice. Not a
    /// retention path: retention drops what the policy ages out, and this is the user saying
    /// they want the copies gone now.
    async fn checkpoint_clear(
        &self,
        req: WorkspaceCheckpointClearReq,
    ) -> ApiResult<WorkspaceCheckpointCleared>;
}

#[async_trait]
pub trait TurnService: Send + Sync {
    async fn submit(&self, submission: Submission) -> ApiResult<SubmitAck>;

    async fn control(&self, command: Command) -> ApiResult<()>;

    async fn cancel_all_turns(&self) -> ApiResult<usize>;

    async fn live_turn_count(&self) -> ApiResult<usize>;

    async fn state(&self, turn_id: TurnId) -> ApiResult<TurnState>;
}

#[async_trait]
pub trait TaskService: Send + Sync {
    async fn list_tasks(&self, req: RuntimeTaskListReq) -> ApiResult<RuntimeTaskPage>;
    async fn stop_task(&self, req: RuntimeTaskStopReq) -> ApiResult<()>;
    async fn delete_task(&self, req: RuntimeTaskDeleteReq) -> ApiResult<()>;
    async fn task_log(&self, req: RuntimeTaskLogReq) -> ApiResult<RuntimeTaskLog>;
    async fn list_jobs(&self, req: TaskJobListReq) -> ApiResult<Vec<TaskJob>>;
    async fn job_runs(&self, req: TaskJobRunsReq) -> ApiResult<RuntimeTaskPage>;
    async fn create_job(&self, req: TaskJobCreateReq) -> ApiResult<TaskJob>;
    async fn set_job_enabled(&self, req: TaskJobSetEnabledReq) -> ApiResult<TaskJob>;
    async fn run_job(&self, req: TaskJobRunReq) -> ApiResult<RuntimeTask>;
    async fn delete_job(&self, req: TaskJobDeleteReq) -> ApiResult<()>;
}

#[async_trait]
pub trait ObjectService: Send + Sync {
    async fn object_read(&self, req: ObjectReadReq) -> ApiResult<ObjectText>;
    async fn object_data(&self, req: ObjectDataReq) -> ApiResult<ObjectData>;
    async fn object_range(&self, req: ObjectRangeReq) -> ApiResult<ObjectRange>;
}

#[async_trait]
pub trait ConfigService: Send + Sync {
    async fn get(&self) -> ApiResult<ConfigView>;
    async fn reload(&self) -> ApiResult<ConfigView>;
    async fn update(&self, req: ConfigUpdateReq) -> ApiResult<ConfigView>;
    async fn upsert_openai_compatible(
        &self,
        req: OpenAiCompatibleProviderReq,
    ) -> ApiResult<ConfigView>;
    async fn remove_provider(&self, req: ConfigRemoveProviderReq) -> ApiResult<ConfigView>;
    async fn catalog(&self) -> ApiResult<ProviderCatalog>;
    async fn refresh_prices(&self) -> ApiResult<ProviderCatalog>;
    async fn check_catalog(&self) -> ApiResult<CatalogCheck>;
    async fn apply_catalog(&self) -> ApiResult<ProviderCatalog>;
    async fn usage_summary(&self, req: UsageSummaryReq) -> ApiResult<UsageSummary>;
    async fn usage_quotas(&self) -> ApiResult<Vec<QuotaStatus>>;
}

#[async_trait]
pub trait ToolCatalogService: Send + Sync {
    async fn list(&self) -> ApiResult<Vec<ToolInfo>>;
}

/// The three layers of environment variables, for the hosts that show them.
///
/// Separate from [`ConfigService`] because two of the three layers are not configuration the
/// process owns: a workspace's `settings.yaml` belongs to a repository and a session's variables
/// belong to a conversation, and neither is reachable through the one global config file.
#[async_trait]
pub trait EnvService: Send + Sync {
    async fn get(&self, req: EnvGetReq) -> ApiResult<EnvView>;
    async fn set(&self, req: EnvSetReq) -> ApiResult<EnvView>;
}

#[async_trait]
pub trait CredentialService: Send + Sync {
    async fn list(&self) -> ApiResult<Vec<CredentialState>>;
    async fn set(&self, req: CredentialSetReq) -> ApiResult<CredentialState>;
    async fn delete(&self, req: CredentialDeleteReq) -> ApiResult<CredentialState>;
    async fn verify(&self, req: CredentialVerifyReq) -> ApiResult<CredentialVerifyResult>;

    /// Begin signing a subscription provider in. The host shows the URL (or the code) and polls
    /// [`CredentialService::sign_in_status`]; nothing here opens a browser.
    async fn sign_in_begin(&self, req: ProviderSignInBeginReq) -> ApiResult<ProviderSignInBegin>;
    async fn sign_in_status(&self, req: ProviderSignInStatusReq)
    -> ApiResult<ProviderSignInStatus>;
    async fn sign_in_cancel(&self, req: ProviderSignInCancelReq) -> ApiResult<()>;

    /// Ask the subscription backend which models this account may call, and keep the answer.
    async fn models(&self, req: ProviderModelsReq) -> ApiResult<ProviderModels>;
    /// Forget a fetched model list, so the provider's built-in one applies again.
    async fn forget_models(&self, req: ProviderModelsReq) -> ApiResult<()>;
}

#[async_trait]
pub trait ManagedResourceService: Send + Sync {
    async fn list(
        &self,
        req: zlogic_protocol::ManagedResourceListReq,
    ) -> ApiResult<Vec<zlogic_protocol::ManagedResource>>;
    async fn upsert(
        &self,
        req: zlogic_protocol::ManagedResourceUpsertReq,
    ) -> ApiResult<zlogic_protocol::ManagedResource>;
    async fn delete(&self, req: zlogic_protocol::ManagedResourceDeleteReq) -> ApiResult<()>;
    async fn test(
        &self,
        req: zlogic_protocol::ManagedResourceTestReq,
    ) -> ApiResult<zlogic_protocol::ManagedResourceTestResult>;
}

#[async_trait]
pub trait ExtensionService: Send + Sync {
    async fn catalog(&self, req: ExtensionCatalogReq) -> ApiResult<ExtensionCatalog>;
    async fn inspect(&self, req: ExtensionInspectReq) -> ApiResult<ExtensionInstallPlan>;
    async fn install(&self, req: ExtensionInstallReq) -> ApiResult<ExtensionDescriptor>;
    async fn set_enabled(&self, req: ExtensionSetEnabledReq) -> ApiResult<ExtensionDescriptor>;
    async fn remove(&self, req: ExtensionRemoveReq) -> ApiResult<()>;
    async fn mcp_upsert(&self, req: McpUpsertReq) -> ApiResult<ExtensionDescriptor>;
    async fn mcp_import(&self, req: McpImportReq) -> ApiResult<McpImportResult>;
    async fn mcp_set_token(&self, req: McpSetTokenReq) -> ApiResult<()>;
    async fn mcp_set_key(&self, req: McpSetKeyReq) -> ApiResult<()>;
    async fn mcp_set_scope(&self, req: McpSetScopeReq) -> ApiResult<ExtensionDescriptor>;
    /// Connect to one server, report its tools, and change nothing.
    async fn mcp_test(&self, req: McpTestReq) -> ApiResult<McpTestResult>;
    /// Run the dependency install a definition implies for itself. Separate from the test so the
    /// user decides, having seen the failure it is meant to fix.
    async fn mcp_setup(&self, req: McpSetupReq) -> ApiResult<McpSetupResult>;
    async fn mcp_set_oauth_client_secret(&self, req: McpSetOAuthClientSecretReq) -> ApiResult<()>;
    async fn mcp_oauth_begin(&self, req: McpOAuthBeginReq) -> ApiResult<McpOAuthBeginResult>;
    async fn mcp_oauth_status(&self, req: McpOAuthStatusReq) -> ApiResult<McpOAuthStatusResult>;
    async fn mcp_oauth_cancel(&self, req: McpOAuthCancelReq) -> ApiResult<()>;
    async fn mcp_logout(&self, req: McpLogoutReq) -> ApiResult<()>;
}

/// Per-turn environment facts a host adds to the system prompt, keyed by the session's turn.
///
/// A capability the model cannot see is a capability it cannot use: a session bound to an Android
/// virtual device, for instance, is useless to the model unless the prompt names that device and
/// the adb target it must use. What goes in here is the closed half's business, so the engine only
/// asks the question and decides where the answer goes — the `<environment>` block, which is
/// rebuilt every turn and therefore always reflects the binding as it is right now.
///
/// Deliberately synchronous: it runs on the turn's own path, so a host implementation must read
/// state it already holds rather than shell out. A host with nothing to add returns `None`.
pub trait SessionEnvironmentService: Send + Sync {
    fn environment(&self, session_id: SessionId) -> Option<String>;
}
