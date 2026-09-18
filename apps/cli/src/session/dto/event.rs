//! `CoreEvent` — serde mirror of the engine's stream payloads (the in-process
//! translation lives in `session::engine`).

use serde::{Deserialize, Serialize};

use super::mailbox::MailboxEntry;
use super::usage::{TurnSummary, UsageSnapshot};

/// Risk hint on a permission request (mirrors core).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Risk {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NotificationLevel {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FormFieldSpec {
    Text {
        name: String,
        label: String,
        #[serde(default)]
        placeholder: String,
        #[serde(default)]
        value: String,
        #[serde(default)]
        hint: String,
        #[serde(default)]
        required: bool,
        /// "" / "any" | "number" | "integer" (advisory content format).
        #[serde(default)]
        format: String,
    },
    Checkbox {
        name: String,
        label: String,
        #[serde(default)]
        checked: bool,
        #[serde(default)]
        hint: String,
    },
    Select {
        name: String,
        label: String,
        options: Vec<String>,
        #[serde(default)]
        selected: usize,
        #[serde(default)]
        hint: String,
    },
    MultiSelect {
        name: String,
        label: String,
        options: Vec<String>,
        #[serde(default)]
        selected: Vec<usize>,
        #[serde(default)]
        hint: String,
        #[serde(default)]
        required: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormItemSpec {
    pub title: String,
    pub fields: Vec<FormFieldSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CoreEvent {
    // ── turn / model-round lifecycle ──
    TurnStart {
        turn_id: String,
        #[serde(default)]
        proactive: bool,
    },
    RoundStart {
        turn_id: String,
        round_id: String,
    },
    // ── streaming deltas ──
    TextDelta {
        text: String,
        #[serde(default)]
        depth: Option<u8>,
        #[serde(default)]
        agent_id: Option<String>,
    },
    ThinkingStart {
        #[serde(default)]
        depth: Option<u8>,
    },
    ThinkingDelta {
        text: String,
        #[serde(default)]
        depth: Option<u8>,
    },
    ThinkingEnd {
        #[serde(default)]
        depth: Option<u8>,
    },

    // ── tools ──
    ToolCallStart {
        id: String,
        name: String,
        #[serde(default)]
        args: serde_json::Value,
    },
    ToolCallEnd {
        id: String,
        ok: bool,
        summary: String,
    },

    // ── sub-agents ──
    SubAgentStart {
        id: String,
        agent_name: String,
        task: String,
    },
    SubAgentEnd {
        id: String,
        summary: String,
    },

    // ── side channels ──
    SessionTitleUpdate {
        title: String,
    },
    Compaction {
        replaces: (u32, u32),
        summary: String,
        summary_tokens: Option<u64>,
    },
    BusNotification {
        level: NotificationLevel,
        title: String,
        #[serde(default)]
        message: String,
        #[serde(default)]
        source: Option<String>,
    },
    Mailbox {
        entry: MailboxEntry,
    },
    Usage {
        snapshot: UsageSnapshot,
    },
    TurnSummaryLoaded {
        summary: TurnSummary,
    },

    // ── interactive (BLOCKING in core: runtime waits for a Respond) ──
    PermissionRequest {
        id: String,
        category: String,
        action: String,
        target: String,
        #[serde(default)]
        reason: String,
        #[serde(default)]
        risk: Option<Risk>,
    },
    ConfirmationRequest {
        id: String,
        #[serde(default)]
        message: String,
    },
    InputRequest {
        id: String,
        #[serde(default)]
        prompt: String,
    },
    FormRequest {
        id: String,
        title: String,
        #[serde(default)]
        fields: Vec<FormFieldSpec>,
        #[serde(default)]
        items: Vec<FormItemSpec>,
    },

    // ── terminal ──
    TurnDone {
        turn_id: String,
    },
    Error {
        message: String,
    },
}

impl CoreEvent {
    /// Interactive events block the core generator until a `Command::Respond`.
    /// Headless (oneshot) must answer these, never ignore them.
    pub fn is_interactive(&self) -> bool {
        matches!(
            self,
            CoreEvent::PermissionRequest { .. }
                | CoreEvent::ConfirmationRequest { .. }
                | CoreEvent::InputRequest { .. }
                | CoreEvent::FormRequest { .. }
        )
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, CoreEvent::TurnDone { .. } | CoreEvent::Error { .. })
    }
}
