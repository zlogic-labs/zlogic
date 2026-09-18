use async_trait::async_trait;
use zlogic_protocol::extensions::{
    ExtensionCatalog, ExtensionCatalogReq, ExtensionDescriptor, ExtensionInspectReq,
    ExtensionInstallPlan, ExtensionInstallReq, ExtensionRemoveReq, ExtensionSetEnabledReq,
    McpImportReq, McpImportResult, McpLogoutReq, McpOAuthBeginReq, McpOAuthBeginResult,
    McpOAuthCancelReq, McpOAuthStatusReq, McpOAuthStatusResult, McpSetKeyReq,
    McpSetOAuthClientSecretReq, McpSetTokenReq, McpUpsertReq,
};
use zlogic_protocol::query::{
    ApiResult, CatalogCheck, ConfigRemoveProviderReq, ConfigUpdateReq, ConfigView,
    CredentialDeleteReq, CredentialSetReq, CredentialState, CredentialVerifyReq,
    CredentialVerifyResult, EntriesReq, ObjectData, ObjectDataReq, ObjectReadReq, ObjectText,
    OpenAiCompatibleProviderReq, Page, ProviderCatalog, RuntimeTask, RuntimeTaskDeleteReq,
    RuntimeTaskListReq, RuntimeTaskLog, RuntimeTaskLogReq, RuntimeTaskPage, RuntimeTaskStopReq,
    SessionListReq, SessionOpenReq, SessionOpened, SessionRenameReq, SessionSearchHit,
    SessionSearchReq, SessionSummary, TaskJob, TaskJobCreateReq, TaskJobDeleteReq, TaskJobDraft,
    TaskJobDraftReq, TaskJobListReq, TaskJobRunReq, TaskJobRunsReq, TaskJobSetEnabledReq, ToolInfo,
    TranscriptEntry, TranscriptReq, TurnItem, TurnState, TurnsReq, UsageSummary, UsageSummaryReq,
    WorkspaceFileBase64, WorkspaceFileCreateReq, WorkspaceFileDeleteReq, WorkspaceFileEntry,
    WorkspaceFileListReq, WorkspaceFileRange, WorkspaceFileRangeReq, WorkspaceFileReadReq,
    WorkspaceFileRenameReq, WorkspaceFileSearchReq, WorkspaceFileText, WorkspaceFileWriteReq,
    WorkspaceGitBranchReq, WorkspaceGitCommitDetail, WorkspaceGitCommitDetailReq,
    WorkspaceGitCommitReq, WorkspaceGitDiff, WorkspaceGitDiffReq,
    WorkspaceGitGenerateCommitMessageReq, WorkspaceGitInfo, WorkspaceGitOverview,
    WorkspaceGitOverviewReq, WorkspaceGitStageReq, WorkspaceGitSyncReq, WorkspaceKind,
    WorkspaceSelector, WorkspaceSummary, WorkspaceUpdateReq,
};
use zlogic_protocol::usage::QuotaStatus;
use zlogic_protocol::{
    AgentProfile, AgentProfileCreateReq, AgentProfileDeleteReq, AgentProfileListReq,
    AgentProfileListRes, AgentProfileUpdateReq, Command, MemoryAddReq, MemoryEditReq,
    MemoryListReq, MemoryRecord, MemoryRemoveReq, MemoryUndoReq, SessionId, Submission, SubmitAck,
    TurnId, WorkspaceId,
};

#[async_trait]
pub trait AuxiliaryService: Send + Sync {
    async fn generate_commit_message(
        &self,
        req: WorkspaceGitGenerateCommitMessageReq,
    ) -> ApiResult<String>;
    async fn draft_task_job(&self, req: TaskJobDraftReq) -> ApiResult<TaskJobDraft>;
}

#[async_trait]
pub trait MemoryService: Send + Sync {
    async fn list(&self, req: MemoryListReq) -> ApiResult<Vec<MemoryRecord>>;
    async fn add(&self, req: MemoryAddReq) -> ApiResult<MemoryRecord>;
    async fn update(&self, req: MemoryEditReq) -> ApiResult<MemoryRecord>;
    async fn remove(&self, req: MemoryRemoveReq) -> ApiResult<MemoryRecord>;
    async fn undo(&self, req: MemoryUndoReq) -> ApiResult<Option<MemoryRecord>>;
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
    async fn git_sync(&self, req: WorkspaceGitSyncReq) -> ApiResult<WorkspaceGitOverview>;
    async fn git_branch(&self, req: WorkspaceGitBranchReq) -> ApiResult<WorkspaceGitOverview>;
    async fn git_commit_detail(
        &self,
        req: WorkspaceGitCommitDetailReq,
    ) -> ApiResult<WorkspaceGitCommitDetail>;
    async fn git_diff(&self, req: WorkspaceGitDiffReq) -> ApiResult<WorkspaceGitDiff>;
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

#[async_trait]
pub trait CredentialService: Send + Sync {
    async fn list(&self) -> ApiResult<Vec<CredentialState>>;
    async fn set(&self, req: CredentialSetReq) -> ApiResult<CredentialState>;
    async fn delete(&self, req: CredentialDeleteReq) -> ApiResult<CredentialState>;
    async fn verify(&self, req: CredentialVerifyReq) -> ApiResult<CredentialVerifyResult>;
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
    async fn mcp_set_oauth_client_secret(&self, req: McpSetOAuthClientSecretReq) -> ApiResult<()>;
    async fn mcp_oauth_begin(&self, req: McpOAuthBeginReq) -> ApiResult<McpOAuthBeginResult>;
    async fn mcp_oauth_status(&self, req: McpOAuthStatusReq) -> ApiResult<McpOAuthStatusResult>;
    async fn mcp_oauth_cancel(&self, req: McpOAuthCancelReq) -> ApiResult<()>;
    async fn mcp_logout(&self, req: McpLogoutReq) -> ApiResult<()>;
}
