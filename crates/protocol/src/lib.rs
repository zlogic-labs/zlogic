//! # zlogic-protocol
//! ```text
//! UI ──①──> engine ──> core ──> loop ──②──> llm
//!  ^                     │
//!  └──────────③──────────┘
//! ```
//! |---|---|---|
//! # id

pub mod agent_profile;
pub mod config;
pub mod error;
pub mod extensions;
pub mod ids;
pub mod input;
pub mod interaction;
pub mod llm;
pub mod memory;
pub mod message;
pub mod query;
pub mod resources;
pub mod roles;
pub mod settings;
pub mod stream;
pub mod usage;

pub use agent_profile::{
    AgentProfile, AgentProfileCreateReq, AgentProfileDeleteReq, AgentProfileListReq,
    AgentProfileListRes, AgentProfileUpdateReq,
};
pub use config::{
    ClientSpec, ModelCapabilities, ModelConfig, ProviderConfig, ProviderOrigin, ResolvedModel, Sdk,
};
pub use error::{ApiError, ApiResult, ErrorCategory, LocalizedMessage, RetryPolicy};
pub use ids::{
    CallId, EntryId, HolderId, MemoryEventId, MemoryId, ResourceId, RoundId, SessionId,
    SubmissionId, TurnId, UsageId, WorkspaceId,
};
pub use input::{
    Command, Delivery, MessagePart, SkillLoadSource, Submission, SubmitAck, TaskUpdatePart,
};
pub use interaction::{
    Choice, Control, DecisionSource, FieldValue, Form, FormAnswer, FormField, GrantScope,
    InteractionBody, InteractionDecision, InteractionPort, InteractionRequest,
};
pub use llm::{
    Effort, FinishReason, LlmError, LlmErrorKind, LlmEvent, LlmRequest, PartKind, ThinkingIntent,
    ThinkingMode, ToolDefinition,
};
pub use memory::{
    MemoryAddReq, MemoryCategory, MemoryEditReq, MemoryListReq, MemoryRecord, MemoryRemoveReq,
    MemoryScope, MemoryStatus, MemoryUndoReq,
};
pub use message::{
    ContentPart, Message, RawPolicy, ReasoningPart, Role, Source, TextPart, ToolCall, ToolCallPart,
    ToolResultFile, ToolResultPart, gate_message_for_target, raw_policy,
};
pub use query::{
    ConfigCreateScope, ConfigUpdateReq, ConfigView, CredentialDeleteReq, CredentialSetReq,
    CredentialSource, CredentialState, CredentialVerifyReq, CredentialVerifyResult, EditProtection,
    EntriesReq, EntryRole, ModelSelection, ModelSelectionSource, OpenAiCompatibleProviderReq, Page,
    PendingInteraction, PendingOrigin, PendingSubmission, SessionListReq, SessionOpenReq,
    SessionOpened, SessionRenameReq, SessionSearchHit, SessionSearchReq, SessionSummary,
    SettingsView, TitleSource, ToolInfo, ToolUsageGroup, TranscriptBody, TranscriptEntry,
    TranscriptKind, TranscriptPart, TranscriptReq, TranscriptToolCall, TurnAnswer, TurnAnswerKind,
    TurnItem, TurnPhase, TurnState, TurnsReq, UploadedObject, UsageGroup, UsageSessionKind,
    UsageSummary, UsageSummaryReq, WorkspaceGitInfo, WorkspaceKind, WorkspaceSelector,
    WorkspaceSummary, WorkspaceToolsUpdate, WorkspaceUpdateReq, chat_workspace_tools,
};
pub use resources::{
    ManagedResource, ManagedResourceDeleteReq, ManagedResourceEnvironment, ManagedResourceKind,
    ManagedResourceListReq, ManagedResourceTestReq, ManagedResourceTestResult,
    ManagedResourceUpsertReq,
};
pub use stream::{
    AgentRef, BlockFinal, BlockKind, OutputSink, OutputStream, RoundStats, StreamEvent,
    StreamPayload, TaskOutputDelta, ToolStats, ToolStatus, TurnStats, TurnStatus,
};
pub use usage::{
    ContextUsage, CostSource, CostTotal, CostView, CurrencyAmount, Purpose, QuotaMetric,
    QuotaScope, QuotaState, QuotaStatus, TokenUsage, UsageReport,
};
