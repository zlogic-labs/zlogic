//! File tools.
//! Grouped by domain rather than by "built in": what matters when reading this is that these
//! tools touch the filesystem, and therefore share the same path-resolution and
//! large-content handling.
//! Each tool does the operation and reports exactly what it did; none of them promises undo.

pub mod convert;
pub mod edit;
pub mod parser;
pub mod read_file;
pub mod write_file;

use std::sync::Arc;

pub use edit::Edit;
pub use read_file::ReadFile;
pub use write_file::WriteFile;

use crate::Tool;
use crate::display::DiffStat;

pub fn all() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(ReadFile), Arc::new(WriteFile), Arc::new(Edit)]
}

/// The UTF-8 byte-order mark.
/// Stripped before matching and re-added on write. A caller may omit the BOM or copy it from
/// `read_file`; accepting either form while keeping the mark outside the replaceable content
/// prevents the first edit of a BOM-prefixed file from duplicating or silently dropping it.
pub(crate) const UTF8_BOM: &str = "\u{feff}";

/// Splits a leading BOM off, returning `(had_bom, rest)`.
pub(crate) fn split_bom(text: &str) -> (bool, &str) {
    match text.strip_prefix(UTF8_BOM) {
        Some(rest) => (true, rest),
        None => (false, text),
    }
}

/// Exact added/removed line counts.
/// Deliberately not a diff algorithm: this is a **count**, computed from the common prefix and
/// suffix of the two line sequences. It is exact for the shapes that matter (append, prepend,
/// replace a block) and never claims to describe *which* lines changed.
/// Shared by every writing tool so a diff card's numbers mean the same thing regardless of
/// which tool produced it.
pub(crate) fn line_delta(before: Option<&str>, after: &str) -> DiffStat {
    let before: Vec<&str> = before.map(|s| s.lines().collect()).unwrap_or_default();
    let after: Vec<&str> = after.lines().collect();

    let prefix = before
        .iter()
        .zip(&after)
        .take_while(|(a, b)| a == b)
        .count();
    let max_suffix = (before.len() - prefix).min(after.len() - prefix);
    let suffix = (0..max_suffix)
        .take_while(|i| before[before.len() - 1 - i] == after[after.len() - 1 - i])
        .count();

    DiffStat {
        added: (after.len() - prefix - suffix) as u32,
        removed: (before.len() - prefix - suffix) as u32,
    }
}

/// A real unified diff, with three lines of context.
/// This is the only diff the project shows: every write path goes through it, so the card, the
/// object store and the model see one shape of diff no matter which tool produced
/// the change (see the ruling in `file/write_file.rs` — a diff that merely *looks* like one is
/// worse than none, which is why whole-file rewrites carry no diff at all).
pub(crate) fn unified_diff(before: &str, after: &str, path: &str) -> String {
    similar::TextDiff::from_lines(before, after)
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_delta_is_exact_for_the_common_shapes() {
        // Pure append.
        assert_eq!(
            line_delta(Some("a\nb\n"), "a\nb\nc\n"),
            DiffStat {
                added: 1,
                removed: 0
            }
        );
        // Pure prepend.
        assert_eq!(
            line_delta(Some("b\n"), "a\nb\n"),
            DiffStat {
                added: 1,
                removed: 0
            }
        );
        // Replace a block in the middle.
        assert_eq!(
            line_delta(Some("a\nx\ny\nd\n"), "a\n1\nd\n"),
            DiffStat {
                added: 1,
                removed: 2
            }
        );
        // New file.
        assert_eq!(
            line_delta(None, "a\nb\n"),
            DiffStat {
                added: 2,
                removed: 0
            }
        );
        // No change at all.
        assert_eq!(
            line_delta(Some("a\n"), "a\n"),
            DiffStat {
                added: 0,
                removed: 0
            }
        );
        // Everything replaced.
        assert_eq!(
            line_delta(Some("a\nb\n"), "x\ny\n"),
            DiffStat {
                added: 2,
                removed: 2
            }
        );
    }

    #[test]
    fn a_bom_is_detected_and_separable() {
        assert_eq!(split_bom("\u{feff}fn main() {}"), (true, "fn main() {}"));
        assert_eq!(split_bom("fn main() {}"), (false, "fn main() {}"));
    }
}
