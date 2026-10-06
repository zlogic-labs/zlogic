//! The gate every log line passes through on its way to a file or to stderr.
//!
//! It lives here rather than at the call sites because a convention only lasts as long as every
//! author remembers it, and there are hundreds of `tracing::` lines. Two rules:
//! 1. **Content and credentials are dropped whole.** No prefix of a prompt is safe, and a
//!    truncated key is still a key.
//! 2. **What gets through is made harmless.** The home directory becomes `~`, and every value has
//!    a length cap, so no single event can fill a log day.
//!
//! ## What this cannot catch
//! The gate sees **named fields**. `tracing::info!(text = %user_text)` is dropped;
//! `tracing::info!("user said {}", user_text)` is not — that text arrives as the `message` field,
//! which is exempt because it is also how every legitimate sentence in the codebase is written.
//! The length cap bounds the second case; it does not prevent it. **Content belongs in a named
//! field** so the gate can see it. Span fields also bypass the gate, because the fmt layer renders
//! them itself: keep them to identifiers.
//!
//! The deliberate exception is a provider's error response, which arrives inside `LlmError`'s own
//! message. It is the most useful line in the file when filing a bug, and the most likely to quote
//! user input — which is why `zlogic-llm` redacts it and caps it before it ever gets here.

use std::fmt;
use std::path::PathBuf;
use std::sync::OnceLock;

use tracing::field::{Field, Visit};
use tracing_subscriber::field::RecordFields;
use tracing_subscriber::fmt::format::{FormatFields, Writer};

/// How long any one value may be. Applied to the message field too: an event embedding an 8000
/// character provider error is still one event, and `zlogic-llm` already shortens those.
const MAX_VALUE_CHARS: usize = 1000;

/// Field names whose value is user content or a credential.
///
/// The `message` field is deliberately absent — it is the human sentence, not user data, and
/// excluding it is what makes the gate usable at all.
const DROPPED: &[&str] = &[
    "api_key",
    "apikey",
    "args",
    "arguments",
    "auth",
    "authorization",
    "cookie",
    "delta",
    "email",
    "input",
    "params",
    "password",
    "prompt",
    "reasoning",
    "secret",
    "text",
    "thinking",
    "token",
    "tool_args",
    "tool_input",
    "tool_output",
    "tool_result",
];

/// Formats an event's fields, applying [`DROPPED`], the home-directory scrub and the length cap.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SafeFields;

impl<'writer> FormatFields<'writer> for SafeFields {
    fn format_fields<R: RecordFields>(
        &self,
        mut writer: Writer<'writer>,
        fields: R,
    ) -> fmt::Result {
        let mut line = String::new();
        fields.record(&mut SafeVisitor { line: &mut line });
        write!(writer, "{line}")
    }
}

/// Builds the `name=value` tail, one field at a time, deciding per field whether it survives.
struct SafeVisitor<'a> {
    line: &'a mut String,
}

impl SafeVisitor<'_> {
    fn write(&mut self, field: &Field, value: String) {
        let name = field.name();
        if DROPPED.contains(&name) || value.is_empty() {
            return;
        }
        let mut value = value;
        scrub_home(&mut value);
        if !self.line.is_empty() {
            self.line.push(' ');
        }
        self.line.push_str(name);
        self.line.push('=');
        push_truncated(self.line, &value);
    }
}

impl Visit for SafeVisitor<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.write(field, value.to_owned());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.write(field, format!("{value:?}"));
    }
}

fn push_truncated(out: &mut String, text: &str) {
    let mut chars = text.chars();
    let kept: String = chars.by_ref().take(MAX_VALUE_CHARS).collect();
    if chars.next().is_some() {
        out.push_str(&kept);
        out.push_str(&format!(
            "…(+{} chars)",
            text.chars().count() - MAX_VALUE_CHARS
        ));
    } else {
        out.push_str(&kept);
    }
}

fn home() -> &'static Option<PathBuf> {
    static HOME: OnceLock<Option<PathBuf>> = OnceLock::new();
    HOME.get_or_init(dirs::home_dir)
}

/// Replaces the user's home directory with `~`, so a log line about
/// `C:\Users\someone\.local\state\zlogic\...` still says which file failed without saying whose
/// machine it was. Two forms because `?path` on a `PathBuf` reaches the writer already escaped
/// (`C:\\Users\\someone`), which would sail straight past the raw prefix.
fn scrub_home(text: &mut String) {
    let Some(home) = home().as_ref() else {
        return;
    };
    let escaped = format!("{home:?}");
    for needle in [home.to_string_lossy().into_owned(), escaped] {
        let needle = needle.trim_matches('"');
        if needle.is_empty() {
            continue;
        }
        if let Some(at) = text.find(needle) {
            text.replace_range(at..at + needle.len(), "~");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_value_is_cut_with_a_count_of_what_was_cut() {
        let mut out = String::new();
        push_truncated(&mut out, &"x".repeat(MAX_VALUE_CHARS + 40));
        assert!(out.ends_with("…(+40 chars)"), "{}", &out[out.len() - 20..]);
        assert_eq!(
            out.chars().count(),
            MAX_VALUE_CHARS + "…(+40 chars)".chars().count()
        );
    }

    #[test]
    fn a_short_value_is_left_alone() {
        let mut out = String::new();
        push_truncated(&mut out, "policy denied");
        assert_eq!(out, "policy denied");
    }

    #[test]
    fn the_home_directory_becomes_a_tilde() {
        let Some(home) = home().as_deref() else {
            return; // no home in this environment; nothing to scrub
        };
        let raw = home.join("state").to_string_lossy().into_owned();
        let mut text = format!("could not read {raw}");
        scrub_home(&mut text);
        assert!(text.starts_with("could not read ~"), "{text}");
        assert!(
            !text.contains(&home.to_string_lossy().into_owned()),
            "{text}"
        );
    }

    #[test]
    fn an_escaped_path_is_scrubbed_too() {
        let Some(home) = home().as_deref() else {
            return;
        };
        // What `?path` on a PathBuf actually hands the formatter.
        let mut text = format!("{:?}", home.join("state"));
        scrub_home(&mut text);
        assert!(text.starts_with("\"~"), "{text}");
    }

    #[test]
    fn a_value_with_no_home_in_it_is_untouched() {
        let mut text = String::from("POST /v1/messages 200 in 812ms");
        scrub_home(&mut text);
        assert_eq!(text, "POST /v1/messages 200 in 812ms");
    }

    /// The field names in [`DROPPED`] must never collide with a diagnostic field the codebase
    /// actually uses — silently dropping `session_id` would be worse than leaking.
    #[test]
    fn diagnostics_fields_are_not_dropped() {
        for name in [
            "session_id",
            "turn_id",
            "round_id",
            "request_id",
            "model",
            "kind",
            "status",
            "path",
            "phase",
            "tool",
            "port",
            "device",
            "serial",
        ] {
            assert!(!DROPPED.contains(&name), "{name} must survive to the log");
        }
    }
}
