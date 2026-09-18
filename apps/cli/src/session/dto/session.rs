//! Session-overlay DTOs. Minimal.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    /// User-renamed titles get the ★ marker in the overlay.
    pub renamed: bool,
    /// Relative time group label, e.g. "Today" / "Yesterday" / "This week".
    pub group: String,
    /// Display time, e.g. "14:02" / "Mon".
    pub when: String,
    pub cost: f64,
    pub model: String,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
    #[serde(default)]
    pub context_used_tokens: u64,
    #[serde(default)]
    pub context_limit_tokens: u64,
    /// Archived sessions are kept (history stays loadable) but drop out of the
    /// default `list_sessions` / `search_sessions` results.
    #[serde(default)]
    pub archived: bool,
}

use super::{CoreEvent, Message, Part};

/// Role carried by a persisted message entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
}

/// One ordered item returned by the core session-history API. Messages retain
/// their structured parts; stream/runtime records are interleaved as events.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HistoryItem {
    Message {
        id: String,
        turn_index: usize,
        role: MessageRole,
        message: Message,
    },
    Event {
        id: String,
        turn_index: usize,
        event: CoreEvent,
    },
}

impl HistoryItem {
    pub fn id(&self) -> &str {
        match self {
            Self::Message { id, .. } | Self::Event { id, .. } => id,
        }
    }

    pub fn turn_index(&self) -> usize {
        match self {
            Self::Message { turn_index, .. } | Self::Event { turn_index, .. } => *turn_index,
        }
    }

    pub fn preview(&self) -> String {
        match self {
            Self::Message { message, .. } => message
                .parts
                .iter()
                .map(|part| match part {
                    Part::Text { text } => text.clone(),
                    Part::File { display, .. }
                    | Part::Directory { display, .. }
                    | Part::Image { display, .. } => display.clone(),
                    Part::Url { url, display } => display.clone().unwrap_or_else(|| url.clone()),
                    Part::Command { name, text, .. } => format!("/{name} {text}"),
                })
                .collect::<Vec<_>>()
                .join(" "),
            Self::Event { event, .. } => match event {
                CoreEvent::ThinkingDelta { text, .. } | CoreEvent::TextDelta { text, .. } => {
                    text.clone()
                }
                CoreEvent::ToolCallStart { name, .. } => format!("tool · {name}"),
                CoreEvent::ToolCallEnd { summary, .. } => summary.clone(),
                CoreEvent::Mailbox { entry } => format!("mailbox · {}", entry.id),
                CoreEvent::PermissionRequest { action, .. } => format!("interaction · {action}"),
                CoreEvent::ConfirmationRequest { message, .. } => {
                    format!("interaction · {message}")
                }
                CoreEvent::InputRequest { prompt, .. } => format!("interaction · {prompt}"),
                CoreEvent::FormRequest { title, .. } => format!("interaction · {title}"),
                _ => format!("{event:?}"),
            },
        }
    }

    pub fn messages_only(entries: impl IntoIterator<Item = HistoryItem>) -> Vec<HistoryItem> {
        entries
            .into_iter()
            .filter(|item| matches!(item, HistoryItem::Message { .. }))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: MessageRole, text: &str) -> HistoryItem {
        HistoryItem::Message {
            id: format!("m-{text}"),
            turn_index: 0,
            role,
            message: Message::text(text.to_string()),
        }
    }

    #[test]
    fn messages_only_drops_event_records_keeps_order() {
        let entries = vec![
            msg(MessageRole::User, "hi"),
            HistoryItem::Event {
                id: "e1".into(),
                turn_index: 0,
                event: CoreEvent::ToolCallStart {
                    id: "c1".into(),
                    name: "shell".into(),
                    args: Default::default(),
                },
            },
            msg(MessageRole::Assistant, "hello"),
            HistoryItem::Event {
                id: "e2".into(),
                turn_index: 1,
                event: CoreEvent::ThinkingDelta {
                    text: "hmm".into(),
                    depth: None,
                },
            },
        ];
        let kept = HistoryItem::messages_only(entries);
        assert_eq!(kept.len(), 2, "events dropped, messages kept in order");
        assert_eq!(kept[0].preview(), "hi");
        assert_eq!(kept[1].preview(), "hello");
    }
}
