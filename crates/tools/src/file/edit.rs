//! `edit` — exact-string replacement in one file.
//! # Why matching is not just `str::find`
//! The `old_string` a model supplies was almost always reconstructed from something it read, and
//! text can change on its way back: a leading BOM may be omitted or copied into the request, line
//! endings may be normalised, and a model that saw numbered output may zlogic the numbers. A literal
//! comparison rejects all of those, and the model's only recourse is to guess again — which it
//! does badly, because the text it is looking at genuinely does appear in the file.
//! So a small set of [`candidates`] is tried in order: the request verbatim first, then the same
//! request with its line endings adapted to the file's, then with `read_file`-style line-number
//! prefixes stripped. Exact matches always win. Only if none exists do we retry while ignoring
//! whitespace. That fallback still resolves to concrete byte ranges and still requires a unique
//! match, so it cannot silently choose one of several similar blocks.
//! # A failure explains its cause
//! "not found" alone makes a model re-read the same file and try the same string again.
//! [`diagnose`] separates the two cases that account for nearly every miss — right text, wrong
//! whitespace; and right anchor line, drifted surroundings — and names them, so the next attempt
//! fixes the actual problem.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use std::ops::Range;
use zlogic_protocol::llm::ToolDefinition;

use crate::display::FileChange;
use crate::file::{UTF8_BOM, line_delta, split_bom, unified_diff};
use crate::{
    ObjectRole, PromptExample, Recovery, Result, Tool, ToolCtx, ToolDisplay, ToolExecResult,
    ToolMeta, ToolPromptSpec, ToolRisk, parse_args_with_prompt,
};

#[derive(Debug, Deserialize)]
struct Args {
    path: String,
    old_string: String,
    new_string: String,
    /// Without this, a match count other than 1 is a failure.
    #[serde(default)]
    allow_multiple: bool,
}

pub struct Edit;

#[async_trait]
impl Tool for Edit {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "edit".into(),
            source: "builtin",
            risk: ToolRisk::Write,
        }
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "edit".into(),
            description: "Replace a piece of text in one file. Exact matching is tried first; \
                          if that fails, differences in whitespace, line endings, and a leading \
                          UTF-8 BOM are ignored. old_string must carry enough surrounding \
                          context (roughly three lines either side) to identify one place \
                          uniquely. It must appear exactly once unless allow_multiple is set. \
                          To create a file or rewrite it wholesale, use write_file."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "minLength": 1, "description": "Absolute, or relative to the working directory" },
                    "old_string": { "type": "string", "minLength": 1, "description": "The text to replace. Exact matching is preferred; whitespace and line-ending differences are tolerated as a unique-match fallback" },
                    "new_string": { "type": "string", "description": "What to replace it with, verbatim" },
                    "allow_multiple": { "type": "boolean", "description": "Replace every occurrence; default false" }
                },
                "required": ["path", "old_string", "new_string"],
                "additionalProperties": false
            }),
        }
    }

    fn prompt_spec(&self) -> Option<ToolPromptSpec> {
        Some(ToolPromptSpec {
            when: "This is the default way to change an existing file: one small, localised \
                   change whose old text you can quote exactly — a renamed symbol, a constant, a \
                   line or a block.",
            contract: "old_string must be the file's *current* text, quoted exactly, with enough \
                surrounding context (roughly three lines either side) to identify one place; it \
                must occur exactly once unless allow_multiple is set. Copy it from what you read \
                rather than from memory. If the edit is refused, re-read the file and copy the \
                current text again — widening the context fixes an ambiguous match, and only \
                re-reading fixes a stale one.",
            positive_examples: &[
                r#"{"path":"src/lib.rs","old_string":"fn a() {}\n\nfn b() {}","new_string":"fn a() {}\n\nfn c() {}"}"#,
                r#"{"path":"src/lib.rs","old_string":"MAX_RETRIES","new_string":"RETRY_LIMIT","allow_multiple":true}"#,
            ],
            negative_examples: &[
                PromptExample {
                    args: r#"{"path":"src/lib.rs","old_string":"    x();","new_string":"    y();"}"#,
                    why: "a fragment this short usually matches in several places — quote the \
                          lines around it",
                },
                PromptExample {
                    args: r#"{"path":"src/lib.rs","old_string":"fn a() {}\n// …everything else…\nfn z() {}","new_string":"<the rewritten file>"}"#,
                    why: "old_string is the real text to locate, not an outline of it; to replace \
                          a file wholesale use write_file",
                },
            ],
        })
    }

    async fn execute(&self, ctx: &ToolCtx, args: &str) -> Result<ToolExecResult> {
        let a: Args = parse_args_with_prompt(args, self.prompt_spec())?;
        let resolved = ctx.resolve_required_path("path", &a.path)?;

        if a.old_string == a.new_string {
            return Ok(ToolExecResult::failed(
                "old_string and new_string are identical, so there is nothing to change",
            ));
        }
        if a.old_string.is_empty() {
            // An empty needle "occurs" at every position; there is no such thing as replacing it.
            return Ok(ToolExecResult::failed(
                "old_string is empty; give the text to replace, or use write_file to create a file",
            ));
        }

        let raw = match std::fs::read(&resolved.path) {
            Ok(b) => b,
            Err(e) => {
                return Ok(ToolExecResult::failed(format!(
                    "cannot read {}: {e}",
                    resolved.path.display()
                )));
            }
        };
        let Ok(raw) = String::from_utf8(raw) else {
            return Ok(ToolExecResult::failed(format!(
                "{} is not UTF-8 text; edit cannot change it",
                resolved.path.display()
            )));
        };
        // Match against the content without the BOM and put it back before writing. Requests both
        // with and without the mark are accepted, but an edit never duplicates or strips it.
        let (had_bom, before) = split_bom(&raw);

        let Some(plan) = plan(before, &a.old_string, &a.new_string, a.allow_multiple) else {
            // Distinguish "nowhere in the file" from "here, but not exactly once": the second is
            // fixed by widening old_string, the first by re-reading. Both are the model's to fix.
            let occurrences = max_occurrences(before, &a.old_string, &a.new_string);
            return Ok(ToolExecResult::failed(if occurrences > 1 {
                format!(
                    "old_string occurs {occurrences} times in {}, and exactly one is required. \
                     Add surrounding context to identify a single place, or set \
                     allow_multiple to replace all {occurrences}.",
                    resolved.path.display()
                )
            } else {
                format!(
                    "old_string does not occur in {}.{} Re-read the file and copy the current \
                     text exactly.",
                    resolved.path.display(),
                    diagnose(before, &a.old_string),
                )
            }));
        };

        let after = apply_plan(before, &plan, a.allow_multiple);
        if after == before {
            return Ok(ToolExecResult::failed(
                "the matched text already has the requested replacement, so there is nothing to change",
            ));
        }

        let to_write = if had_bom {
            format!("{UTF8_BOM}{after}")
        } else {
            after.clone()
        };
        if let Err(e) = std::fs::write(&resolved.path, to_write.as_bytes()) {
            return Ok(ToolExecResult::failed(format!(
                "cannot write {}: {e}",
                resolved.path.display()
            )));
        }

        let path = resolved.path.to_string_lossy().into_owned();
        let unified = unified_diff(before, &after, &path);
        let stat = line_delta(Some(before), &after);
        let plural = if plan.occurrences == 1 { "" } else { "s" };

        let summary = format!(
            "edited {path} ({} occurrence{plural} replaced, +{} -{})",
            plan.occurrences, stat.added, stat.removed,
        );

        // **Always** to the object store, whatever the size. There is no threshold, and dropping
        // the one that used to be here is the point: a threshold means two code paths — inline and
        // referenced — in the producer, in the card, in the projection and in the UI, and each of
        // them has to keep getting the boundary right. One path cannot get it wrong.
        // The two audiences then bound themselves independently. `capture` gives the model the diff
        // trimmed to `max_result_chars` on line boundaries, and the card gets only the reference —
        // so the database row stays the same small size whether the edit touched one line or ten
        // thousand.
        let cap = ctx.capture(
            ObjectRole::Diff,
            Some(&path),
            &unified,
            Recovery::ReadFileRange,
        )?;
        let object_id = cap.object_id().clone();
        let out = ToolExecResult::success(format!("{summary}\n\n{}", cap.model_text))
            .with_object(cap.object)
            .with_display(ToolDisplay::Diff {
                path,
                stat,
                change: Some(FileChange::Modified),
                object_id: Some(object_id),
            });
        Ok(out)
    }
}

/// One candidate substitution, and how many times it occurs.
struct Plan {
    new: String,
    matches: Vec<Range<usize>>,
    occurrences: usize,
}

/// The first candidate that occurs an acceptable number of times.
/// All exact candidates are considered before the whitespace-insensitive fallback. This preserves
/// the unsurprising rule that text copied byte-for-byte always selects that exact text.
fn plan(content: &str, old: &str, new: &str, allow_multiple: bool) -> Option<Plan> {
    let candidates = candidates(content, old, new);
    for ignore_whitespace in [false, true] {
        for c in &candidates {
            let matches = if ignore_whitespace {
                whitespace_insensitive_matches(content, &c.old)
            } else {
                exact_matches(content, &c.old)
            };
            let occurrences = matches.len();
            if occurrences == 0 || (occurrences > 1 && !allow_multiple) {
                continue;
            }
            return Some(Plan {
                new: c.new.clone(),
                matches,
                occurrences,
            });
        }
    }
    None
}

fn apply_plan(content: &str, plan: &Plan, allow_multiple: bool) -> String {
    let matches = if allow_multiple {
        plan.matches.as_slice()
    } else {
        &plan.matches[..1]
    };
    let replaced_bytes: usize = matches.iter().map(|range| range.len()).sum();
    let mut out = String::with_capacity(
        content.len() - replaced_bytes + plan.new.len().saturating_mul(matches.len()),
    );
    let mut cursor = 0;
    for range in matches {
        out.push_str(&content[cursor..range.start]);
        out.push_str(&plan.new);
        cursor = range.end;
    }
    out.push_str(&content[cursor..]);
    out
}

struct Candidate {
    old: String,
    new: String,
}

/// The substitutions to try, most literal first.
/// Order is the whole point: the request as written must win whenever it matches, so a
/// transformation can only ever *add* a way to succeed, never change which bytes an exact
/// request lands on.
fn candidates(content: &str, old: &str, new: &str) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    let mut push = |old: String, new: String| {
        if old.is_empty() || old == new || out.iter().any(|c| c.old == old && c.new == new) {
            return;
        }
        out.push(Candidate { old, new });
    };

    let mut with_endings = |old: &str, new: &str| {
        // Adapt both arguments independently. In particular, replacing a single-line needle with
        // a multi-line value in a CRLF file must adapt `new` even though `old` has no newline.
        let file_crlf = content.contains("\r\n");
        let adapt = |text: &str| {
            let lf = text.replace("\r\n", "\n");
            if file_crlf {
                lf.replace('\n', "\r\n")
            } else {
                lf
            }
        };
        let adapted_old = adapt(old);
        let adapted_new = adapt(new);
        if adapted_old == old && adapted_new != new {
            // Both candidates select the same bytes. Prefer the replacement that preserves the
            // file's established newline convention.
            push(adapted_old, adapted_new);
            push(old.to_string(), new.to_string());
            return;
        }
        push(old.to_string(), new.to_string());
        if adapted_old != old {
            push(adapted_old, adapted_new);
        }
    };

    // A BOM belongs to the file, not to the replaceable content. Accept it if a caller copied it
    // into either argument, but never duplicate or remove the file's own leading BOM.
    let old = old.strip_prefix(UTF8_BOM).unwrap_or(old);
    let new = new.strip_prefix(UTF8_BOM).unwrap_or(new);
    with_endings(old, new);
    // A model that read numbered output tends to hand the numbers back. Strip them from both
    // sides — `new_string` usually carries the same prefixes, and stripping only the needle
    // would write the numbers into the file.
    if let Some(stripped_old) = strip_line_numbers(old) {
        let stripped_new = strip_line_numbers(new).unwrap_or_else(|| new.to_string());
        with_endings(&stripped_old, &stripped_new);
    }
    out
}

fn exact_matches(haystack: &str, needle: &str) -> Vec<Range<usize>> {
    haystack
        .match_indices(needle)
        .map(|(start, matched)| start..start + matched.len())
        .collect()
}

/// Finds the needle after removing Unicode whitespace from both sides, then maps each match back
/// to the exact byte range in `content`.
/// Leading and trailing whitespace in the needle is deliberately not consumed. Only whitespace
/// *between* the first and last non-whitespace character belongs to the replacement; this prevents
/// an indented snippet from eating the previous line's newline or the next line's indentation.
fn whitespace_insensitive_matches(content: &str, needle: &str) -> Vec<Range<usize>> {
    let compact_needle: String = needle.chars().filter(|c| !c.is_whitespace()).collect();
    if compact_needle.is_empty() {
        return Vec::new();
    }

    let mut compact_content = String::with_capacity(content.len());
    let mut source_chars = Vec::new();
    for (source_start, ch) in content.char_indices() {
        if ch.is_whitespace() {
            continue;
        }
        let compact_start = compact_content.len();
        compact_content.push(ch);
        source_chars.push((
            compact_start,
            compact_content.len(),
            source_start,
            source_start + ch.len_utf8(),
        ));
    }

    compact_content
        .match_indices(&compact_needle)
        .filter_map(|(compact_start, matched)| {
            let compact_end = compact_start + matched.len();
            let first = source_chars
                .binary_search_by_key(&compact_start, |entry| entry.0)
                .ok()?;
            let last = source_chars
                .binary_search_by_key(&compact_end, |entry| entry.1)
                .ok()?;
            Some(source_chars[first].2..source_chars[last].3)
        })
        .collect()
}

fn max_occurrences(content: &str, old: &str, new: &str) -> usize {
    candidates(content, old, new)
        .iter()
        .flat_map(|c| {
            [
                exact_matches(content, &c.old).len(),
                whitespace_insensitive_matches(content, &c.old).len(),
            ]
        })
        .max()
        .unwrap_or(0)
}

/// Strips `   12\t` prefixes, but only if **every** line has one.
/// All-or-nothing on purpose: a partial strip would silently mangle a genuine tab-indented
/// snippet that happens to start with digits.
fn strip_line_numbers(text: &str) -> Option<String> {
    let normalised = text.replace("\r\n", "\n");
    let lines: Vec<&str> = normalised.split('\n').collect();
    let mut out = Vec::with_capacity(lines.len());
    let mut stripped = 0;

    for (i, line) in lines.iter().enumerate() {
        // A trailing empty line is the split artefact of a final newline, not a numbered line.
        if line.is_empty() && i == lines.len() - 1 {
            out.push(String::new());
            continue;
        }
        let rest = line.trim_start_matches(' ');
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        let body = rest[digits.len()..].strip_prefix('\t')?;
        if digits.is_empty() {
            return None;
        }
        out.push(body.to_string());
        stripped += 1;
    }

    if stripped > 0 {
        Some(out.join("\n"))
    } else {
        None
    }
}

/// Names the likely cause of a miss, as a sentence to append to the failure.
/// Returns `""` when nothing is recognisable — genuinely absent text, where the honest answer is
/// no explanation rather than a guessed one.
fn diagnose(content: &str, old: &str) -> String {
    // Compare with all indentation and blank lines removed. If the text is there under that
    // comparison, whitespace is the only thing wrong — by far the most common miss.
    let collapse = |s: &str| {
        s.replace("\r\n", "\n")
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let needle = collapse(old);
    if !needle.is_empty() && collapse(content).contains(&needle) {
        return " The text is present but its whitespace differs — copy the exact spaces, tabs \
                and line breaks."
            .into();
    }

    // Otherwise, if the snippet's first substantial line exists somewhere, the anchor is right
    // and the surrounding context has drifted.
    let anchor = old.lines().map(str::trim).find(|l| l.len() > 4);
    if let Some(anchor) = anchor
        && let Some(idx) = content.lines().position(|l| l.trim() == anchor)
    {
        return format!(
            " A line matching the start of old_string is at line {}, but the text around it \
             differs — re-read there and widen old_string.",
            idx + 1
        );
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::DiffStat;
    use crate::{ToolExecStatus, test_ctx};

    fn setup(content: &str) -> (tempfile::TempDir, ToolCtx) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), content).unwrap();
        let ctx = test_ctx(dir.path());
        (dir, ctx)
    }

    fn args(old: &str, new: &str, allow_multiple: bool) -> String {
        json!({ "path": "a.rs", "old_string": old, "new_string": new, "allow_multiple": allow_multiple })
            .to_string()
    }

    #[tokio::test]
    async fn replaces_a_unique_occurrence() {
        let (d, ctx) = setup("fn a() {}\nfn b() {}\n");
        let out = Edit
            .execute(&ctx, &args("fn b() {}", "fn c() {}", false))
            .await
            .unwrap();

        assert_eq!(out.status, ToolExecStatus::Success);
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.rs")).unwrap(),
            "fn a() {}\nfn c() {}\n"
        );
    }

    /// The diff is real, so the model can confirm it changed the place it meant to.
    /// The card carries only what a collapsed card needs plus the reference; the diff text lives in
    /// the object store, whatever its size.
    #[tokio::test]
    async fn the_result_carries_a_true_unified_diff() {
        let (_d, mut ctx) = setup("one\ntwo\nthree\n");
        ctx.max_result_chars = 30_000;
        let out = Edit
            .execute(&ctx, &args("two", "TWO", false))
            .await
            .unwrap();

        match &out.display[0] {
            ToolDisplay::Diff {
                stat,
                change,
                object_id,
                ..
            } => {
                assert_eq!(
                    *stat,
                    DiffStat {
                        added: 1,
                        removed: 1
                    },
                    "the collapsed card renders from this"
                );
                assert_eq!(*change, Some(FileChange::Modified));
                let stored =
                    String::from_utf8(ctx.objects.get(object_id.as_ref().unwrap()).unwrap())
                        .unwrap();
                assert!(stored.contains("-two"), "{stored}");
                assert!(stored.contains("+TWO"), "{stored}");
                assert!(
                    stored.contains("@@"),
                    "a unified diff has hunk headers: {stored}"
                );
            }
            other => panic!("expected a diff card, got {other:?}"),
        }
        // The model gets the diff too — that is what lets it verify without another read.
        assert!(out.model_text().contains("+TWO"));
        assert!(out.dangling_display_objects().is_empty());
    }

    /// One path, no threshold: a three-line edit and a four-thousand-line edit produce the same
    /// shape of card, so nothing downstream needs a branch.
    #[tokio::test]
    async fn every_size_of_diff_produces_the_same_shape_of_card() {
        for content in ["one\ntwo\n", &"needle\n".repeat(4000)] {
            let (_d, mut ctx) = setup(content);
            ctx.max_result_chars = 30_000;
            let needle = if content.starts_with("one") {
                "two"
            } else {
                "needle"
            };

            let out = Edit
                .execute(&ctx, &args(needle, "REPLACED", true))
                .await
                .unwrap();
            match &out.display[0] {
                ToolDisplay::Diff { object_id, .. } => {
                    assert!(object_id.is_some(), "always a reference, never inline text");
                }
                other => panic!("expected a diff card, got {other:?}"),
            }
            assert!(out.dangling_display_objects().is_empty());
        }
    }

    /// More than one match without `allow_multiple` changes nothing at all.
    #[tokio::test]
    async fn an_ambiguous_match_is_refused_and_the_file_is_untouched() {
        let (d, ctx) = setup("x = 1\nx = 1\n");
        let out = Edit
            .execute(&ctx, &args("x = 1", "x = 2", false))
            .await
            .unwrap();

        assert_eq!(out.status, ToolExecStatus::Failed);
        assert!(out.model_text().contains("2 times"), "{}", out.model_text());
        assert!(
            out.model_text().contains("allow_multiple"),
            "it must say how to proceed"
        );
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.rs")).unwrap(),
            "x = 1\nx = 1\n"
        );
    }

    #[tokio::test]
    async fn allow_multiple_replaces_every_occurrence() {
        let (d, ctx) = setup("x = 1\ny = 2\nx = 1\n");
        let out = Edit
            .execute(&ctx, &args("x = 1", "x = 9", true))
            .await
            .unwrap();

        assert_eq!(out.status, ToolExecStatus::Success);
        assert!(out.model_text().contains("2 occurrences"));
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.rs")).unwrap(),
            "x = 9\ny = 2\nx = 9\n"
        );
    }

    /// The candidate that matters most in practice: a CRLF file and an LF request.
    #[tokio::test]
    async fn line_endings_are_adapted_to_the_file() {
        let (d, ctx) = setup("fn a() {\r\n    body();\r\n}\r\n");
        let out = Edit
            .execute(
                &ctx,
                &args("fn a() {\n    body();\n}", "fn a() {\n    new();\n}", false),
            )
            .await
            .unwrap();

        assert_eq!(out.status, ToolExecStatus::Success, "{}", out.model_text());
        let after = std::fs::read_to_string(d.path().join("a.rs")).unwrap();
        assert_eq!(
            after, "fn a() {\r\n    new();\r\n}\r\n",
            "the file keeps its own endings"
        );
    }

    #[tokio::test]
    async fn a_multiline_replacement_adopts_crlf_even_for_a_single_line_match() {
        let (d, ctx) = setup("before\r\nneedle\r\nafter\r\n");
        let out = Edit
            .execute(&ctx, &args("needle", "first\nsecond", false))
            .await
            .unwrap();

        assert_eq!(out.status, ToolExecStatus::Success, "{}", out.model_text());
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.rs")).unwrap(),
            "before\r\nfirst\r\nsecond\r\nafter\r\n"
        );
    }

    /// A model zlogicing numbered output must not have the numbers written into the file.
    #[tokio::test]
    async fn read_file_line_numbers_are_stripped_from_both_sides() {
        let (d, ctx) = setup("alpha\nbeta\n");
        let out = Edit
            .execute(
                &ctx,
                &args(
                    "     1\talpha\n     2\tbeta",
                    "     1\talpha\n     2\tGAMMA",
                    false,
                ),
            )
            .await
            .unwrap();

        assert_eq!(out.status, ToolExecStatus::Success, "{}", out.model_text());
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.rs")).unwrap(),
            "alpha\nGAMMA\n"
        );
    }

    /// A BOM the model never saw must survive the edit.
    #[tokio::test]
    async fn a_bom_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "\u{feff}one\ntwo\n").unwrap();
        let ctx = test_ctx(dir.path());

        let out = Edit
            .execute(&ctx, &args("one", "ONE", false))
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Success);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.rs")).unwrap(),
            "\u{feff}ONE\ntwo\n"
        );
    }

    /// Whitespace differences are tolerated after exact matching has failed.
    #[tokio::test]
    async fn a_whitespace_only_mismatch_is_replaced() {
        let (d, ctx) = setup("fn a() {\n        deeply(  1,\t2 );\n}\n");
        let out = Edit
            .execute(
                &ctx,
                &args(
                    "fn a() {\r\n  deeply(1, 2);\r\n}",
                    "fn a() {\n    shallow();\n}",
                    false,
                ),
            )
            .await
            .unwrap();

        assert_eq!(out.status, ToolExecStatus::Success, "{}", out.model_text());
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.rs")).unwrap(),
            "fn a() {\n    shallow();\n}\n"
        );
    }

    /// Ignoring whitespace must not weaken the unique-match guard.
    #[tokio::test]
    async fn ambiguous_whitespace_insensitive_matches_are_refused() {
        let (d, ctx) = setup("x = 1\nx=1\n");
        let out = Edit
            .execute(&ctx, &args("x\t=\t1", "x = 2", false))
            .await
            .unwrap();

        assert_eq!(out.status, ToolExecStatus::Failed);
        assert!(out.model_text().contains("2 times"), "{}", out.model_text());
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.rs")).unwrap(),
            "x = 1\nx=1\n"
        );
    }

    /// Byte-range mapping remains correct around multibyte characters, and surrounding layout is
    /// not swallowed when the request itself has leading/trailing whitespace.
    #[tokio::test]
    async fn whitespace_matching_maps_unicode_back_to_the_right_bytes() {
        let (d, ctx) = setup("before\n    你好（ 世界 ）\nafter\n");
        let out = Edit
            .execute(&ctx, &args("\n  你好（世界）\n", "再见", false))
            .await
            .unwrap();

        assert_eq!(out.status, ToolExecStatus::Success, "{}", out.model_text());
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.rs")).unwrap(),
            "before\n    再见\nafter\n",
            "the existing indentation and surrounding newlines survive"
        );
    }

    /// A BOM copied into the request is metadata too, not part of the replaceable body.
    #[tokio::test]
    async fn a_bom_in_the_request_is_ignored_without_being_duplicated() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "\u{feff}one\r\ntwo\r\n").unwrap();
        let ctx = test_ctx(dir.path());

        let out = Edit
            .execute(&ctx, &args("\u{feff}one\ntwo", "\u{feff}ONE\nTWO", false))
            .await
            .unwrap();

        assert_eq!(out.status, ToolExecStatus::Success, "{}", out.model_text());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.rs")).unwrap(),
            "\u{feff}ONE\r\nTWO\r\n"
        );
    }

    /// Right anchor, drifted context — the report points at the line.
    #[tokio::test]
    async fn a_drifted_context_points_at_the_anchor_line() {
        let (_d, ctx) = setup("aaa\nbbb\nthe_anchor_line\nccc\n");
        let out = Edit
            .execute(
                &ctx,
                &args("zzz\nthe_anchor_line\nyyy", "replacement", false),
            )
            .await
            .unwrap();

        assert_eq!(out.status, ToolExecStatus::Failed);
        assert!(out.model_text().contains("line 3"), "{}", out.model_text());
    }

    /// Genuinely absent text gets no invented explanation.
    #[tokio::test]
    async fn a_real_miss_gets_no_guessed_diagnosis() {
        let (_d, ctx) = setup("alpha\n");
        let out = Edit
            .execute(&ctx, &args("nothing_like_this_at_all", "x", false))
            .await
            .unwrap();

        assert_eq!(out.status, ToolExecStatus::Failed);
        assert!(!out.model_text().contains("whitespace"));
        assert!(!out.model_text().contains("line "));
    }

    #[tokio::test]
    async fn a_no_op_edit_is_refused() {
        let (_d, ctx) = setup("x\n");
        let out = Edit.execute(&ctx, &args("x", "x", false)).await.unwrap();
        assert_eq!(out.status, ToolExecStatus::Failed);
        assert!(out.model_text().contains("identical"));
    }

    /// An empty needle matches everywhere and nowhere; it must not be treated as a match.
    #[tokio::test]
    async fn an_empty_old_string_is_refused() {
        let (_d, ctx) = setup("x\n");
        let out = Edit.execute(&ctx, &args("", "y", false)).await.unwrap();
        assert_eq!(out.status, ToolExecStatus::Failed);
        assert!(out.model_text().contains("empty"));
    }

    #[tokio::test]
    async fn a_missing_file_is_a_failed_result_not_an_err() {
        let (_d, ctx) = setup("x");
        let out = Edit
            .execute(
                &ctx,
                &json!({ "path": "nope.rs", "old_string": "a", "new_string": "b" }).to_string(),
            )
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Failed);
        assert!(out.model_text().contains("nope.rs"));
    }

    #[tokio::test]
    async fn binary_content_is_refused_rather_than_mangled() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), [0xff, 0xfe, 0x00]).unwrap();
        let ctx = test_ctx(dir.path());
        let out = Edit
            .execute(
                &ctx,
                &json!({ "path": "a.rs", "old_string": "a", "new_string": "b" }).to_string(),
            )
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Failed);
        assert!(out.model_text().contains("not UTF-8"));
    }

    #[tokio::test]
    async fn missing_required_args_are_rejected() {
        let (_d, ctx) = setup("x");
        assert!(
            Edit.execute(&ctx, r#"{"path":"a.rs","old_string":"x"}"#)
                .await
                .is_err()
        );
        assert!(Edit.execute(&ctx, "not json").await.is_err());
    }

    /// A pathological diff becomes a reference the UI fetches, and the reference is kept alive.
    #[tokio::test]
    async fn a_huge_diff_is_offloaded_and_stays_referenced() {
        let (_d, ctx) = setup(&"needle\n".repeat(4000));
        let out = Edit
            .execute(&ctx, &args("needle", "replaced", true))
            .await
            .unwrap();

        assert_eq!(out.status, ToolExecStatus::Success);
        assert!(out.dangling_display_objects().is_empty());

        // The **whole** diff is in the store, not the trimmed version the model saw.
        let id = out.object_with_role(ObjectRole::Diff).unwrap();
        let stored = String::from_utf8(ctx.objects.get(id).unwrap()).unwrap();
        assert!(stored.contains("-needle") && stored.contains("+replaced"));
        assert_eq!(
            stored.lines().filter(|l| l.starts_with("-needle")).count(),
            4000,
            "every hunk"
        );
        assert!(
            stored.chars().count() > out.model_text().chars().count() * 10,
            "the store has the whole thing and the model has a fraction of it"
        );
    }

    /// The model's context is bounded by `max_result_chars`, and by nothing else — the card no
    /// longer has a size decision of its own to get wrong.
    #[tokio::test]
    async fn a_huge_diff_does_not_enter_the_model_context_whole() {
        let (_d, mut ctx) = setup(&"needle\n".repeat(400));
        ctx.max_result_chars = 500;

        let out = Edit
            .execute(&ctx, &args("needle", "replaced", true))
            .await
            .unwrap();
        let text = out.model_text();

        assert!(
            text.contains("400 occurrences replaced"),
            "the summary always survives: {text}"
        );
        assert!(
            text.contains("omitted"),
            "and the trim declares itself: {text}"
        );
        // Generously above the budget, since the summary and the note ride along with it.
        assert!(
            text.chars().count() < 1_500,
            "{} chars reached the model",
            text.chars().count()
        );
        // What it is told is how to get the rest — the file is on disk, so a line range.
        assert!(text.contains("read_file"), "{text}");
        let id = out.object_with_role(ObjectRole::Diff).unwrap();
        assert!(
            !text.contains(&id.to_string()),
            "an object id is useless to the model: {text}"
        );
    }

    /// Even a small diff is a reference. The model still gets the text — only the card does not.
    #[tokio::test]
    async fn a_small_diff_is_still_stored_and_still_reaches_the_model() {
        let (_d, mut ctx) = setup(
            "one
two
",
        );
        ctx.max_result_chars = 30_000;
        let out = Edit
            .execute(&ctx, &args("two", "TWO", false))
            .await
            .unwrap();

        assert_eq!(out.objects.len(), 1, "one object per edit, no threshold");
        assert!(
            out.model_text().contains("-two"),
            "and the model reads it in full"
        );
        assert!(
            !out.model_text().contains("omitted"),
            "nothing was trimmed: {}",
            out.model_text()
        );
    }

    /// The request as written must win, so an exact match is never re-interpreted.
    #[test]
    fn the_verbatim_request_is_the_first_candidate() {
        let c = candidates("     1\tx\n", "     1\tx", "     1\ty");
        assert_eq!(c[0].old, "     1\tx", "verbatim first");
        assert_eq!(c[1].old, "x", "the stripped form is only a fallback");
    }

    #[test]
    fn line_numbers_are_stripped_all_or_nothing() {
        assert_eq!(
            strip_line_numbers("   1\ta\n   2\tb").as_deref(),
            Some("a\nb")
        );
        // One unnumbered line and the whole thing is left alone — a genuine tab-indented
        // snippet must not be mangled.
        assert_eq!(strip_line_numbers("   1\ta\nplain"), None);
        assert_eq!(strip_line_numbers("no numbers here"), None);
        // A tab-indented body that merely starts with digits is not a numbered line.
        assert_eq!(strip_line_numbers("\t42 is the answer"), None);
    }
}
