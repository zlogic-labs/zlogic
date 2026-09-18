//! Structured detail for the UI.
//! **Never part of the model's context.** This is what lets a tool result render as a diff or a
//! file card instead of a wall of text, without spending tokens on the presentation.
//! Each variant carries what a renderer needs and nothing more. In particular a variant never
//! carries the full payload when that payload could be large — it carries an
//! [`ObjectId`] instead, so the UI fetches on demand and the row in the database stays small.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use zlogic_objects::ObjectId;

// One definition, in the crate that owns the UI boundary. The projection core sends over the
// stream reuses these verbatim; only the object id changes form (typed here, an id string there).
pub use zlogic_protocol::stream::{DiffStat, FileChange};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolDisplay {
    /// Plain text, rendered as-is. The fallback when nothing richer applies.
    /// `math` is an optional parallel rendering for clients that can typeset mathematics: the
    /// same content line by line, with the formulas as LaTeX. A terminal ignores it and prints
    /// `text`; a GUI renders real fraction bars and integral signs instead of `/` and `^`.
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        math: Vec<MathLine>,
    },

    /// A bounded relational result rendered as a real table by graphical clients.
    /// Rows are UI-only and never enter model context. Database tools keep binary values as
    /// explicit size placeholders, but preserve ordinary scalar/text values for the first N rows.
    /// `object_id` is set when a cell was too large to inline: the full table (every row, every
    /// cell, untruncated) is captured as an object and the UI fetches it on demand to power a
    /// "view full table" modal.
    Table {
        columns: Vec<String>,
        rows: Vec<Vec<Value>>,
        total_rows: u64,
        truncated: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        object_id: Option<ObjectId>,
    },

    /// A change to one file.
    /// # The diff itself is never here
    /// There is no inline `unified` field. It existed, with an `Option<String>` beside an
    /// `Option<ObjectId>`, and that was wrong in two ways at once.
    /// It made the type have four states where only two are meaningful — both absent and both
    /// present are nonsense every consumer nonetheless had to handle. And the inline text lived in
    /// a database row, so it was read **every time the transcript was rebuilt**, for every card,
    /// whether or not anyone expanded it. A session with two hundred edits paid for two hundred
    /// diffs to render a scrollback.
    /// So the card carries what a *collapsed* card needs — the path and the line counts — and
    /// `object_id` is how the text is reached when someone actually asks for it. `None` means there
    /// is no diff to show, not that it was inlined: `write_file` reports exact counts for a whole
    /// rewrite without producing a diff of the entire file.
    Diff {
        path: String,
        stat: DiffStat,
        /// Set when the file was created or deleted rather than modified, so the renderer can
        /// say so instead of showing an all-additions diff.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        change: Option<FileChange>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        object_id: Option<ObjectId>,
    },

    /// A file the result refers to.
    /// `mime` drives how the UI renders it (highlighting, an image preview). It is optional
    /// because guessing wrong is worse than not guessing: a mislabelled binary shown as text
    /// produces a screen of control characters.
    File {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime: Option<String>,
        bytes: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        total_lines: Option<u64>,
        /// Present when the content was captured into the object store, so the UI can show it
        /// even after the file on disk has changed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        object_id: Option<ObjectId>,
    },

    /// Captured output that was too large to hand to the model in full.
    /// This is what powers a "view full output" affordance: the model saw head and tail, the
    /// user can see everything.
    Output {
        object_id: ObjectId,
        total_chars: u64,
        truncated: bool,
    },

    /// A sub-agent run.
    /// `session_id` is what makes the child's transcript reachable — the UI opens it to show what
    /// the sub-agent actually did, rather than only its conclusion.
    Agent { agent: String, session_id: String },

    Task {
        task_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<String>,
    },

    /// A sandboxed HTML widget. Source is stored as an object and fetched only when the card is
    /// expanded; generated markup never enters the host application's document.
    Widget {
        object_id: ObjectId,
        title: String,
        height: u16,
        libraries: Vec<String>,
    },
}

/// One line of a [`ToolDisplay::Text`] card's mathematical rendering.
/// Two fields rather than one string because the prose and the mathematics are typeset
/// differently: `label` says what is happening ("Divide by n - 1"), `latex` is the formula that a
/// client passes to a math renderer. Either may be empty — a pure equation has no label, a step
/// that only names data has no formula.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MathLine {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    /// LaTeX **without** delimiters: the client decides whether it is inline or display math.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub latex: String,
}

impl MathLine {
    pub fn new(label: impl Into<String>, latex: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            latex: latex.into(),
        }
    }
}

impl ToolDisplay {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            math: Vec::new(),
        }
    }

    /// A text card that also carries a typesettable rendering of its mathematics.
    pub fn math(text: impl Into<String>, math: Vec<MathLine>) -> Self {
        Self::Text {
            text: text.into(),
            math,
        }
    }

    /// The object this card points at, if any.
    /// Used to check that a card and the result's reference list agree: a card naming an object
    /// that nothing keeps alive would render as a broken link once garbage collection ran.
    pub fn object_id(&self) -> Option<&ObjectId> {
        match self {
            ToolDisplay::Diff { object_id, .. }
            | ToolDisplay::File { object_id, .. }
            | ToolDisplay::Table { object_id, .. } => object_id.as_ref(),
            ToolDisplay::Output { object_id, .. } => Some(object_id),
            ToolDisplay::Widget { object_id, .. } => Some(object_id),
            ToolDisplay::Text { .. } | ToolDisplay::Agent { .. } | ToolDisplay::Task { .. } => None,
        }
    }

    /// Guesses a MIME type from the extension, for the common text formats only.
    /// Returns `None` when unsure. A wrong `text/*` label on a binary is worse than no label:
    /// the UI would try to render it.
    pub fn guess_mime(path: &str) -> Option<&'static str> {
        let ext = path.rsplit('.').next()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "rs" => "text/rust",
            "ts" | "tsx" => "text/typescript",
            "js" | "jsx" | "mjs" => "text/javascript",
            "py" => "text/x-python",
            "go" => "text/x-go",
            "java" => "text/x-java",
            "json" => "application/json",
            "yaml" | "yml" => "application/yaml",
            "toml" => "application/toml",
            "md" | "markdown" => "text/markdown",
            "html" | "htm" => "text/html",
            "css" => "text/css",
            "sh" | "bash" | "zsh" => "text/x-shellscript",
            "sql" => "application/sql",
            "txt" | "log" => "text/plain",
            "csv" => "text/csv",
            "tsv" | "tab" => "text/tab-separated-values",
            "pdf" => "application/pdf",
            "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            "xls" => "application/vnd.ms-excel",
            "xlsb" => "application/vnd.ms-excel.sheet.binary.macroenabled.12",
            "ods" => "application/vnd.oasis.opendocument.spreadsheet",
            "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "doc" => "application/msword",
            "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            "ppt" => "application/vnd.ms-powerpoint",
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "svg" => "image/svg+xml",
            "mp3" => "audio/mpeg",
            "wav" => "audio/wav",
            "aiff" | "aif" => "audio/aiff",
            "aac" => "audio/aac",
            "flac" => "audio/flac",
            "ogg" | "opus" => "audio/ogg",
            "m4a" => "audio/mp4",
            "mp4" | "m4v" => "video/mp4",
            "webm" => "video/webm",
            "mov" => "video/quicktime",
            "mkv" => "video/x-matroska",
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn variants_are_tagged_by_kind_on_the_wire() {
        let d = ToolDisplay::text("hello");
        assert_eq!(
            serde_json::to_value(&d).unwrap(),
            json!({ "kind": "text", "text": "hello" })
        );
    }

    #[test]
    fn diff_round_trips_with_stats_and_change_type() {
        let d = ToolDisplay::Diff {
            path: "src/main.rs".into(),
            stat: DiffStat {
                added: 1,
                removed: 1,
            },
            change: Some(FileChange::Modified),
            object_id: None,
        };
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v["kind"], "diff");
        assert_eq!(v["stat"]["added"], 1);
        assert_eq!(v["change"], "modified");
        assert_eq!(serde_json::from_value::<ToolDisplay>(v).unwrap(), d);
    }

    /// A diff card carries a reference, never the text — at any size.
    #[test]
    fn a_diff_card_points_at_an_object_and_never_carries_the_text() {
        let id: ObjectId =
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                .parse()
                .unwrap();
        let d = ToolDisplay::Diff {
            path: "big.lock".into(),
            stat: DiffStat {
                added: 9000,
                removed: 8000,
            },
            change: Some(FileChange::Modified),
            object_id: Some(id),
        };
        let v = serde_json::to_value(&d).unwrap();
        assert!(
            v.get("unified").is_none(),
            "the field does not exist at all"
        );
        assert!(v["object_id"].is_string());
    }

    #[test]
    fn file_display_carries_what_a_renderer_needs() {
        let d = ToolDisplay::File {
            path: "a.rs".into(),
            mime: ToolDisplay::guess_mime("a.rs").map(str::to_string),
            bytes: 1234,
            total_lines: Some(42),
            object_id: None,
        };
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v["mime"], "text/rust");
        assert_eq!(v["total_lines"], 42);
        assert_eq!(serde_json::from_value::<ToolDisplay>(v).unwrap(), d);
    }

    /// Guessing wrong is worse than not guessing — an unknown extension gets no MIME type.
    #[test]
    fn mime_guessing_declines_when_unsure() {
        assert_eq!(ToolDisplay::guess_mime("x.rs"), Some("text/rust"));
        assert_eq!(
            ToolDisplay::guess_mime("x.PNG"),
            Some("image/png"),
            "case-insensitive"
        );
        assert_eq!(ToolDisplay::guess_mime("mystery.xyzzy"), None);
        assert_eq!(
            ToolDisplay::guess_mime("Makefile"),
            None,
            "no extension, no guess"
        );
    }

    #[test]
    fn mime_guessing_covers_audio_and_video() {
        assert_eq!(ToolDisplay::guess_mime("voice.mp3"), Some("audio/mpeg"));
        assert_eq!(ToolDisplay::guess_mime("clip.MP4"), Some("video/mp4"));
        assert_eq!(ToolDisplay::guess_mime("track.m4a"), Some("audio/mp4"));
        assert_eq!(ToolDisplay::guess_mime("film.webm"), Some("video/webm"));
        assert_eq!(ToolDisplay::guess_mime("film.xyz"), None);
    }

    #[test]
    fn only_the_cards_that_point_at_an_object_report_one() {
        let id: ObjectId =
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                .parse()
                .unwrap();
        assert_eq!(
            ToolDisplay::Output {
                object_id: id.clone(),
                total_chars: 1,
                truncated: false
            }
            .object_id(),
            Some(&id)
        );
        assert_eq!(
            ToolDisplay::File {
                path: "a".into(),
                mime: None,
                bytes: 0,
                total_lines: None,
                object_id: Some(id.clone()),
            }
            .object_id(),
            Some(&id)
        );
        assert_eq!(ToolDisplay::text("x").object_id(), None);
        assert_eq!(
            ToolDisplay::Agent {
                agent: "a".into(),
                session_id: "s".into()
            }
            .object_id(),
            None
        );
    }

    #[test]
    fn the_agent_card_carries_the_child_session() {
        let d = ToolDisplay::Agent {
            agent: "researcher".into(),
            session_id: "abc".into(),
        };
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v["kind"], "agent");
        assert_eq!(v["session_id"], "abc");
        assert_eq!(serde_json::from_value::<ToolDisplay>(v).unwrap(), d);
    }

    #[test]
    fn output_display_round_trips() {
        let id: ObjectId =
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                .parse()
                .unwrap();
        let d = ToolDisplay::Output {
            object_id: id,
            total_chars: 50_000,
            truncated: true,
        };
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v["kind"], "output");
        assert_eq!(serde_json::from_value::<ToolDisplay>(v).unwrap(), d);
    }
}
