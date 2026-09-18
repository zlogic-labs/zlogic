//! `Command` — what the UI sends to the backend, mirroring the engine's
//! submission/control surface (`turn_start` / `turn_cancel` / `respond` /
//! `mailbox_enqueue`). `EngineSession` forwards these to `zlogic_engine::EngineApi`.

use serde::{Deserialize, Serialize};

use super::message::Message;

/// Non-interactive approval posture for a turn. Maps onto core's
/// `autoApprovePermissions` / `bypassPermissions` / classifier plumbing.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    /// core auto-approval classifier: safe ops pass, catastrophic escalate to ask.
    #[default]
    Auto,
    /// deny anything needing approval — agent works read-only.
    Deny,
    /// bypassPermissions (= godMode full access): emit no permission requests. Dangerous.
    ApproveAll,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    TurnStart {
        /// Structured input (composer output). A bare-text turn is one message with
        /// one text part.
        messages: Vec<Message>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default)]
        plan: bool,
        #[serde(default)]
        permission: PermissionMode,
    },
    TurnCancel,
    CompactContext,
    /// Request core-owned aggregate statistics after `TurnDone`.
    RequestTurnSummary {
        turn_id: String,
    },
    /// Queue structured user input in core's mailbox without aborting the turn.
    MailboxEnqueue {
        messages: Vec<Message>,
    },
    /// Answer to a blocking interactive event.
    Respond {
        id: String,
        answer: Answer,
    },
}

/// Answer to a `PermissionRequest` / `ConfirmationRequest` / `InputRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Answer {
    Allow {
        #[serde(default)]
        scope: GrantScope,
    },
    Deny {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// Free-text reply to an `input_request`.
    Input { text: String },
    Form {
        values: serde_json::Map<String, serde_json::Value>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum GrantScope {
    #[default]
    Once,
    Session,
    Always,
}
