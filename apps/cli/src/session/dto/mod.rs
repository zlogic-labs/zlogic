//! DTOs split by domain. One file per concern; no monolithic `types.rs`.

pub mod command;
pub mod command_catalog;
pub mod event;
pub mod mailbox;
pub mod message;
pub mod model;
pub mod session;
pub mod usage;
pub mod workspace;

pub use command::{Answer, Command, GrantScope, PermissionMode};
pub use command_catalog::{builtins, CommandKind, CommandSpec};
pub use event::{CoreEvent, FormFieldSpec, NotificationLevel, Risk};
pub use mailbox::{messages_preview, CoreBusEvent, MailboxEntry};
pub use message::{Message, Part};
pub use model::{
    ConfigOp, ConfigView, ConnResult, KeyEntry, KeyStatus, ModelEntry, ProviderEntry, Tier,
};
pub use session::{HistoryItem, MessageRole, SessionSummary};
pub use usage::{ModelUsage, RoundSummary, TurnSummary, TurnUsage, UsageSnapshot};
pub use workspace::WorkspaceSummary;
