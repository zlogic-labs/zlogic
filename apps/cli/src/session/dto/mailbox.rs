//! Core-owned mailbox DTOs. Event-bus notifications only invalidate this data;
//! the UI always calls `CoreSession::mailbox()` to read the current contents.

use serde::{Deserialize, Serialize};

use super::{Message, Part};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MailboxEntry {
    pub id: String,
    pub messages: Vec<Message>,
}

impl MailboxEntry {
    pub fn preview(&self) -> String {
        messages_preview(&self.messages)
    }
}

/// Produce a compact UI preview from the final structured payload. Queue UI must
/// inspect Parts rather than infer their kind from composer chips: a File chip is
/// `@file`, while pasted text remains ordinary text.
pub fn messages_preview(messages: &[Message]) -> String {
    messages
        .iter()
        .flat_map(|message| message.parts.iter())
        .map(|part| match part {
            Part::Text { text } => text.clone(),
            Part::File { display, .. }
            | Part::Directory { display, .. }
            | Part::Image { display, .. } => format!("@{}", display.trim_start_matches('@')),
            Part::Url { url, display } => display.clone().unwrap_or_else(|| url.clone()),
            Part::Command { name, text, .. } => format!("/{name} {text}").trim_end().to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CoreBusEvent {
    MailboxChanged,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_preview_keeps_file_syntax_instead_of_paste_label() {
        let messages = vec![Message::from_parts(vec![
            Part::File {
                path: "Cargo.toml".into(),
                display: "Cargo.toml".into(),
                preview: None,
            },
            Part::Text {
                text: "check this".into(),
            },
        ])];
        assert_eq!(messages_preview(&messages), "@Cargo.toml check this");
    }
}
