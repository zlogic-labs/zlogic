use async_trait::async_trait;
use zlogic_protocol::extensions::{
    ExtensionCatalog, ExtensionCatalogReq, ExtensionDescriptor, ExtensionInspectReq,
    ExtensionInstallPlan, ExtensionInstallReq, ExtensionRemoveReq, ExtensionSetEnabledReq,
    McpImportReq, McpImportResult, McpLogoutReq, McpOAuthBeginReq, McpOAuthBeginResult,
    McpOAuthCancelReq, McpOAuthStatusReq, McpOAuthStatusResult, McpSetKeyReq,
    McpSetOAuthClientSecretReq, McpSetTokenReq, McpUpsertReq,
};
use zlogic_protocol::query::{
    ApiError, ApiResult, CatalogCheck, ConfigRemoveProviderReq, ConfigUpdateReq, ConfigView,
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
use zlogic_protocol::{
    AgentProfile, AgentProfileCreateReq, AgentProfileDeleteReq, AgentProfileListReq,
    AgentProfileListRes, AgentProfileUpdateReq, Command, ManagedResource, ManagedResourceDeleteReq,
    ManagedResourceListReq, ManagedResourceTestReq, ManagedResourceTestResult,
    ManagedResourceUpsertReq, MemoryAddReq, MemoryEditReq, MemoryListReq, MemoryRecord,
    MemoryRemoveReq, MemoryUndoReq, SessionId, Submission, SubmitAck, TurnId, WorkspaceId,
};

use crate::service::{
    AgentProfileService, AuxiliaryService, ConfigService, CredentialService, ExtensionService,
    ManagedResourceService, MemoryService, ObjectService, SessionService, TaskService,
    ToolCatalogService, TurnService, WorkspaceFilesService, WorkspaceGitService, WorkspaceService,
};

pub struct NotWired;

macro_rules! nope {
    ($op:literal) => {
        Err(ApiError::not_wired($op))
    };
}

#[async_trait]
impl AuxiliaryService for NotWired {
    async fn generate_commit_message(
        &self,
        _req: WorkspaceGitGenerateCommitMessageReq,
    ) -> ApiResult<String> {
        nope!("workspace_git_generate_commit_message")
    }

    async fn draft_task_job(&self, _req: TaskJobDraftReq) -> ApiResult<TaskJobDraft> {
        nope!("task_job_draft")
    }
}

#[async_trait]
impl MemoryService for NotWired {
    async fn list(&self, _req: MemoryListReq) -> ApiResult<Vec<MemoryRecord>> {
        nope!("memory_list")
    }
    async fn add(&self, _req: MemoryAddReq) -> ApiResult<MemoryRecord> {
        nope!("memory_add")
    }
    async fn update(&self, _req: MemoryEditReq) -> ApiResult<MemoryRecord> {
        nope!("memory_update")
    }
    async fn remove(&self, _req: MemoryRemoveReq) -> ApiResult<MemoryRecord> {
        nope!("memory_remove")
    }
    async fn undo(&self, _req: MemoryUndoReq) -> ApiResult<Option<MemoryRecord>> {
        nope!("memory_undo")
    }
}

#[async_trait]
impl AgentProfileService for NotWired {
    async fn list(&self, _req: AgentProfileListReq) -> ApiResult<AgentProfileListRes> {
        nope!("agent_profile_list")
    }
    async fn create(&self, _req: AgentProfileCreateReq) -> ApiResult<AgentProfile> {
        nope!("agent_profile_create")
    }
    async fn update(&self, _req: AgentProfileUpdateReq) -> ApiResult<AgentProfile> {
        nope!("agent_profile_update")
    }
    async fn delete(&self, _req: AgentProfileDeleteReq) -> ApiResult<()> {
        nope!("agent_profile_delete")
    }
}

#[async_trait]
impl SessionService for NotWired {
    async fn list(&self, _req: SessionListReq) -> ApiResult<Page<SessionSummary>> {
        nope!("session_list")
    }
    async fn open(&self, _req: SessionOpenReq) -> ApiResult<SessionOpened> {
        nope!("session_open")
    }
    async fn rename(&self, _req: SessionRenameReq) -> ApiResult<SessionSummary> {
        nope!("session_rename")
    }
    async fn delete(&self, _session_id: SessionId) -> ApiResult<()> {
        nope!("session_delete")
    }
    async fn set_model(
        &self,
        _session_id: SessionId,
        _model_ref: String,
    ) -> ApiResult<SessionSummary> {
        nope!("session_set_model")
    }

    async fn set_effort(
        &self,
        _session_id: SessionId,
        _effort: String,
    ) -> ApiResult<SessionSummary> {
        nope!("session_set_effort")
    }
    async fn search(&self, _req: SessionSearchReq) -> ApiResult<Vec<SessionSearchHit>> {
        nope!("session_search")
    }
    async fn transcript(&self, _req: TranscriptReq) -> ApiResult<Page<TranscriptEntry>> {
        nope!("session_transcript")
    }

    async fn turns(&self, _req: TurnsReq) -> ApiResult<Page<TurnItem>> {
        nope!("session_turns")
    }

    async fn entries(&self, _req: EntriesReq) -> ApiResult<Page<TranscriptEntry>> {
        nope!("session_entries")
    }
}

#[async_trait]
impl WorkspaceService for NotWired {
    async fn resolve(
        &self,
        _sel: WorkspaceSelector,
        _name: Option<String>,
        _kind: Option<WorkspaceKind>,
    ) -> ApiResult<WorkspaceSummary> {
        nope!("workspace_resolve")
    }
    async fn get(&self, _sel: WorkspaceSelector) -> ApiResult<WorkspaceSummary> {
        nope!("workspace_get")
    }
    async fn list(&self, _include_hidden: bool) -> ApiResult<Vec<WorkspaceSummary>> {
        nope!("workspace_list")
    }
    async fn update(&self, _req: WorkspaceUpdateReq) -> ApiResult<WorkspaceSummary> {
        nope!("workspace_update")
    }
    async fn create_chat(&self, _name: String) -> ApiResult<WorkspaceSummary> {
        nope!("workspace_create_chat")
    }
    async fn delete(&self, _workspace_id: WorkspaceId) -> ApiResult<()> {
        nope!("workspace_delete")
    }
}

#[async_trait]
impl WorkspaceFilesService for NotWired {
    async fn file_list(&self, _req: WorkspaceFileListReq) -> ApiResult<Vec<WorkspaceFileEntry>> {
        nope!("workspace_file_list")
    }
    async fn file_search(
        &self,
        _req: WorkspaceFileSearchReq,
    ) -> ApiResult<Vec<WorkspaceFileEntry>> {
        nope!("workspace_file_search")
    }
    async fn file_read(&self, _req: WorkspaceFileReadReq) -> ApiResult<WorkspaceFileText> {
        nope!("workspace_file_read")
    }
    async fn file_read_base64(&self, _req: WorkspaceFileReadReq) -> ApiResult<WorkspaceFileBase64> {
        nope!("workspace_file_read_base64")
    }
    async fn file_range(&self, _req: WorkspaceFileRangeReq) -> ApiResult<WorkspaceFileRange> {
        nope!("workspace_file_range")
    }
    async fn file_write(&self, _req: WorkspaceFileWriteReq) -> ApiResult<WorkspaceFileText> {
        nope!("workspace_file_write")
    }
    async fn file_create(&self, _req: WorkspaceFileCreateReq) -> ApiResult<()> {
        nope!("workspace_file_create")
    }
    async fn file_rename(&self, _req: WorkspaceFileRenameReq) -> ApiResult<()> {
        nope!("workspace_file_rename")
    }
    async fn file_delete(&self, _req: WorkspaceFileDeleteReq) -> ApiResult<()> {
        nope!("workspace_file_delete")
    }
}

#[async_trait]
impl WorkspaceGitService for NotWired {
    async fn git_info(&self, _sel: WorkspaceSelector) -> ApiResult<WorkspaceGitInfo> {
        nope!("workspace_git_info")
    }
    async fn git_overview(&self, _req: WorkspaceGitOverviewReq) -> ApiResult<WorkspaceGitOverview> {
        nope!("workspace_git_overview")
    }
    async fn git_commit(&self, _req: WorkspaceGitCommitReq) -> ApiResult<WorkspaceGitOverview> {
        nope!("workspace_git_commit")
    }
    async fn git_stage(&self, _req: WorkspaceGitStageReq) -> ApiResult<WorkspaceGitOverview> {
        nope!("workspace_git_stage")
    }
    async fn git_sync(&self, _req: WorkspaceGitSyncReq) -> ApiResult<WorkspaceGitOverview> {
        nope!("workspace_git_sync")
    }
    async fn git_branch(&self, _req: WorkspaceGitBranchReq) -> ApiResult<WorkspaceGitOverview> {
        nope!("workspace_git_branch")
    }
    async fn git_commit_detail(
        &self,
        _req: WorkspaceGitCommitDetailReq,
    ) -> ApiResult<WorkspaceGitCommitDetail> {
        nope!("workspace_git_commit_detail")
    }
    async fn git_diff(&self, _req: WorkspaceGitDiffReq) -> ApiResult<WorkspaceGitDiff> {
        nope!("workspace_git_diff")
    }
}

#[async_trait]
impl TurnService for NotWired {
    async fn submit(&self, _submission: Submission) -> ApiResult<SubmitAck> {
        nope!("submit")
    }
    async fn control(&self, _command: Command) -> ApiResult<()> {
        nope!("control")
    }
    async fn cancel_all_turns(&self) -> ApiResult<usize> {
        Ok(0)
    }
    async fn live_turn_count(&self) -> ApiResult<usize> {
        Ok(0)
    }
    async fn state(&self, _turn_id: TurnId) -> ApiResult<TurnState> {
        nope!("turn_state")
    }
}

#[async_trait]
impl TaskService for NotWired {
    async fn list_tasks(&self, _req: RuntimeTaskListReq) -> ApiResult<RuntimeTaskPage> {
        nope!("runtime_task_list")
    }
    async fn stop_task(&self, _req: RuntimeTaskStopReq) -> ApiResult<()> {
        nope!("runtime_task_stop")
    }
    async fn delete_task(&self, _req: RuntimeTaskDeleteReq) -> ApiResult<()> {
        nope!("runtime_task_delete")
    }
    async fn task_log(&self, _req: RuntimeTaskLogReq) -> ApiResult<RuntimeTaskLog> {
        nope!("runtime_task_log")
    }
    async fn list_jobs(&self, _req: TaskJobListReq) -> ApiResult<Vec<TaskJob>> {
        nope!("task_job_list")
    }
    async fn job_runs(&self, _req: TaskJobRunsReq) -> ApiResult<RuntimeTaskPage> {
        nope!("task_job_runs")
    }
    async fn create_job(&self, _req: TaskJobCreateReq) -> ApiResult<TaskJob> {
        nope!("task_job_create")
    }
    async fn set_job_enabled(&self, _req: TaskJobSetEnabledReq) -> ApiResult<TaskJob> {
        nope!("task_job_set_enabled")
    }
    async fn run_job(&self, _req: TaskJobRunReq) -> ApiResult<RuntimeTask> {
        nope!("task_job_run")
    }
    async fn delete_job(&self, _req: TaskJobDeleteReq) -> ApiResult<()> {
        nope!("task_job_delete")
    }
}

#[async_trait]
impl ObjectService for NotWired {
    async fn object_read(&self, _req: ObjectReadReq) -> ApiResult<ObjectText> {
        nope!("object_read")
    }

    async fn object_data(&self, _req: ObjectDataReq) -> ApiResult<ObjectData> {
        nope!("object_data")
    }
}

#[async_trait]
impl ConfigService for NotWired {
    async fn get(&self) -> ApiResult<ConfigView> {
        nope!("config_get")
    }
    async fn reload(&self) -> ApiResult<ConfigView> {
        nope!("config_reload")
    }
    async fn update(&self, _req: ConfigUpdateReq) -> ApiResult<ConfigView> {
        nope!("config_update")
    }
    async fn upsert_openai_compatible(
        &self,
        _req: OpenAiCompatibleProviderReq,
    ) -> ApiResult<ConfigView> {
        nope!("config_upsert_openai_compatible")
    }
    async fn remove_provider(&self, _req: ConfigRemoveProviderReq) -> ApiResult<ConfigView> {
        nope!("config_remove_provider")
    }
    async fn catalog(&self) -> ApiResult<ProviderCatalog> {
        nope!("config_catalog")
    }
    async fn refresh_prices(&self) -> ApiResult<ProviderCatalog> {
        nope!("config_refresh_prices")
    }
    async fn check_catalog(&self) -> ApiResult<CatalogCheck> {
        nope!("config_check_catalog")
    }
    async fn apply_catalog(&self) -> ApiResult<ProviderCatalog> {
        nope!("config_apply_catalog")
    }
    async fn usage_summary(&self, _req: UsageSummaryReq) -> ApiResult<UsageSummary> {
        nope!("usage_summary")
    }

    async fn usage_quotas(&self) -> ApiResult<Vec<zlogic_protocol::QuotaStatus>> {
        nope!("usage_quotas")
    }
}

#[async_trait]
impl CredentialService for NotWired {
    async fn list(&self) -> ApiResult<Vec<CredentialState>> {
        nope!("credential_list")
    }
    async fn set(&self, _req: CredentialSetReq) -> ApiResult<CredentialState> {
        nope!("credential_set")
    }
    async fn delete(&self, _req: CredentialDeleteReq) -> ApiResult<CredentialState> {
        nope!("credential_delete")
    }
    async fn verify(&self, _req: CredentialVerifyReq) -> ApiResult<CredentialVerifyResult> {
        nope!("credential_verify")
    }
}

#[async_trait]
impl ManagedResourceService for NotWired {
    async fn list(&self, _req: ManagedResourceListReq) -> ApiResult<Vec<ManagedResource>> {
        nope!("managed_resource_list")
    }
    async fn upsert(&self, _req: ManagedResourceUpsertReq) -> ApiResult<ManagedResource> {
        nope!("managed_resource_upsert")
    }
    async fn delete(&self, _req: ManagedResourceDeleteReq) -> ApiResult<()> {
        nope!("managed_resource_delete")
    }
    async fn test(&self, _req: ManagedResourceTestReq) -> ApiResult<ManagedResourceTestResult> {
        nope!("managed_resource_test")
    }
}

#[async_trait]
impl ExtensionService for NotWired {
    async fn catalog(&self, _req: ExtensionCatalogReq) -> ApiResult<ExtensionCatalog> {
        nope!("extension_catalog")
    }
    async fn inspect(&self, _req: ExtensionInspectReq) -> ApiResult<ExtensionInstallPlan> {
        nope!("extension_inspect")
    }
    async fn install(&self, _req: ExtensionInstallReq) -> ApiResult<ExtensionDescriptor> {
        nope!("extension_install")
    }
    async fn set_enabled(&self, _req: ExtensionSetEnabledReq) -> ApiResult<ExtensionDescriptor> {
        nope!("extension_set_enabled")
    }
    async fn remove(&self, _req: ExtensionRemoveReq) -> ApiResult<()> {
        nope!("extension_remove")
    }
    async fn mcp_upsert(&self, _req: McpUpsertReq) -> ApiResult<ExtensionDescriptor> {
        nope!("extension_mcp_upsert")
    }
    async fn mcp_import(&self, _req: McpImportReq) -> ApiResult<McpImportResult> {
        nope!("extension_mcp_import")
    }
    async fn mcp_set_token(&self, _req: McpSetTokenReq) -> ApiResult<()> {
        nope!("extension_mcp_set_token")
    }
    async fn mcp_set_key(&self, _req: McpSetKeyReq) -> ApiResult<()> {
        nope!("extension_mcp_set_key")
    }
    async fn mcp_set_oauth_client_secret(&self, _req: McpSetOAuthClientSecretReq) -> ApiResult<()> {
        nope!("extension_mcp_set_oauth_client_secret")
    }
    async fn mcp_oauth_begin(&self, _req: McpOAuthBeginReq) -> ApiResult<McpOAuthBeginResult> {
        nope!("extension_mcp_oauth_begin")
    }
    async fn mcp_oauth_status(&self, _req: McpOAuthStatusReq) -> ApiResult<McpOAuthStatusResult> {
        nope!("extension_mcp_oauth_status")
    }
    async fn mcp_oauth_cancel(&self, _req: McpOAuthCancelReq) -> ApiResult<()> {
        nope!("extension_mcp_oauth_cancel")
    }
    async fn mcp_logout(&self, _req: McpLogoutReq) -> ApiResult<()> {
        nope!("extension_mcp_logout")
    }
}

#[async_trait]
impl ToolCatalogService for NotWired {
    async fn list(&self) -> ApiResult<Vec<ToolInfo>> {
        nope!("tool_list")
    }
}

#[cfg(test)]
mod tests {
    use crate::{Engine, EngineApi};

    #[tokio::test]
    async fn every_op_reports_not_wired_with_its_own_name() {
        let engine = Engine::not_wired();
        match engine.workspace_list(false).await {
            Err(error) if error.category == zlogic_protocol::ErrorCategory::NotWired => {
                assert_eq!(error.details["op"], "workspace_list")
            }
            other => panic!("expected NotWired, got {other:?}"),
        }
        match engine.config_get().await {
            Err(error) if error.category == zlogic_protocol::ErrorCategory::NotWired => {
                assert_eq!(error.details["op"], "config_get")
            }
            other => panic!("expected NotWired, got {other:?}"),
        }
    }
}
