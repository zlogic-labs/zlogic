//! Structured conversation messages exchanged with core.
//! A message is an ordered collection of content parts. Runtime events such as
//! permissions, tools, thinking, and mailbox notifications are deliberately not
//! represented here; they live in the stream/history event protocol.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Message {
    #[serde(default)]
    pub parts: Vec<Part>,
}

impl Message {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            parts: vec![Part::Text { text: text.into() }],
        }
    }

    pub fn from_parts(parts: Vec<Part>) -> Self {
        Self { parts }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Part {
    Text {
        text: String,
    },
    File {
        path: String,
        display: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preview: Option<String>,
    },
    Directory {
        path: String,
        display: String,
    },
    Image {
        path: String,
        display: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime: Option<String>,
    },
    Url {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display: Option<String>,
    },
    Command {
        command: String,
        name: String,
        #[serde(default)]
        text: String,
    },
}
