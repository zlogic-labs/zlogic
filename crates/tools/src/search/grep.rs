//! `grep` — regular-expression search across file contents.
//! # There is no `rg` subprocess here
//! ripgrep *is* Rust. The search runs in-process through `code-sitter`, which drives the same crates
//! ripgrep itself does (`grep-regex`, `grep-searcher`, `ignore`). That removes the entire
//! machinery the TypeScript tool needed: no probing the `PATH` for `rg` and falling back to
//! system `grep` and then to a hand-written scan, no parsing `path:line:text` back out of a pipe,
//! and no killing a child process to enforce a total match cap — the cap is a counter in the sink.
//! # Each hit says what it *is*
//! `code-sitter` parses only the files that had hits with tree-sitter and tags each one with its
//! enclosing symbol and a kind — definition, call, comment, string:
//! ```text
//! 2 matches in 2 files for /apply_coupon/ under .:
//! src/discount.rs:
//! 3: [def] (in fn apply_coupon) pub fn apply_coupon(subtotal: u64) -> u64 {
//! src/total.rs:
//! 15: [call] (in method Checkout::compute) subtotal = apply_coupon(subtotal);
//! ```
//! # Grouped by file, because the path is what repeats
//! A hit's path is the largest repeated thing in a grep result: a 60-character path on ten hits
//! from one file is 600 characters carrying no new information, and with `context_lines` it
//! repeats on every context line too. So the path is written once per file, as the group heading,
//! and each hit under it carries only its line number. Measured on real searches in this
//! repository: −27% characters for a spread-out search (39 files, 300 hits), −37% for few files
//! with many hits, −53% with `context_lines: 1`. Fewer characters is not cosmetic — it is the
//! difference between a result that fits the budget and one that comes back as head-and-tail.
//! # …unless the result is about to be trimmed
//! A grouped hit has one dependency: `12:` means nothing without the heading above it. The engine
//! trims a long result by keeping the head and the tail and dropping whole lines in between, so a
//! tail can begin in the middle of a group. When the grouped rendition would not fit
//! `max_result_chars` — and so is about to be trimmed — the list falls back to the flat form
//! instead, where every line carries its own `path:line:` and is readable on its own, quotable on
//! its own, and can be handed straight back to `read_file`.
//! This is the difference between "27 files mention `apply_coupon`" and "here is where it is
//! defined and here are the three places that call it". It costs a parse of the handful of hit
//! files, needs no index, and cannot go stale. Files in a language it does not handle come back
//! unannotated — plain grep output, which is exactly the old behaviour — and the summary line
//! reports how many hits those were, so an absent `[def]` cannot read as "defined elsewhere" when
//! the file it lives in simply cannot be parsed for symbols.
//! # The search is synchronous, so it does not run on the executor
//! It is CPU- and IO-bound and can take seconds over a large tree, so it goes to
//! `spawn_blocking`. Cancellation is bridged onto `code-sitter`'s cooperative flag rather than
//! dropping the future: a dropped blocking task keeps running to completion with nobody waiting
//! for it, whereas the flag makes the search return at its next checkpoint.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use code_sitter::{HitKind, Options};
use serde::Deserialize;
use serde_json::json;
use zlogic_protocol::llm::ToolDefinition;

use crate::search::CaseMode;
use crate::search::ignores::Ignores;
use crate::{Recovery, Result, Tool, ToolCtx, ToolExecResult, ToolMeta, ToolRisk, parse_args};

const DEFAULT_MAX_MATCHES: usize = 100;
const MAX_MATCHES_CEILING: usize = 2_000;
const MAX_CONTEXT_LINES: usize = 20;

#[derive(Debug, Deserialize)]
struct Args {
    pattern: String,
    /// Defaults to the working directory.
    path: Option<String>,
    /// A file glob, e.g. `*.rs` or `src/**/*.{js,ts}`.
    include: Option<String>,
    #[serde(default)]
    case_mode: CaseMode,
    /// Lines shown either side of a match, like `grep -C`.
    #[serde(default)]
    context_lines: usize,
    /// Report the matching file paths only.
    #[serde(default)]
    files_only: bool,
    /// Also search what `.gitignore` excludes.
    #[serde(default)]
    include_ignored: bool,
    max_matches: Option<usize>,
}

pub struct Grep;

#[async_trait]
impl Tool for Grep {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "grep".into(),
            source: "builtin",
            risk: ToolRisk::Read,
        }
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "grep".into(),
            description: "Search file contents with a regular expression. Hits come grouped \
                          under their file's path, one row per hit as `line: [kind] (in symbol) \
                          text` — the tag says whether the hit is a definition, a call, a comment \
                          or a string, so one search distinguishes where something is defined \
                          from where it is used. Cite a hit as path:line (its file heading plus \
                          its line number). A result too large to fit comes back flat instead, \
                          one self-contained path:line:text per row. Use this when you know a \
                          name, an error string or a config key but not the file. For filename \
                          patterns use glob; to read an implementation after a hit, use \
                          read_file."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "minLength": 1, "description": "Regular expression" },
                    "path": { "type": "string", "description": "File or directory to search; defaults to the working directory" },
                    "include": { "type": "string", "description": "File glob filter, e.g. \"*.rs\" or \"src/**/*.{js,ts}\"" },
                    "case_mode": { "type": "string", "enum": ["smart", "sensitive", "insensitive"], "description": "Case handling; smart (default) is sensitive only when the pattern contains uppercase letters" },
                    "context_lines": {
                        "type": "integer", "minimum": 0, "maximum": MAX_CONTEXT_LINES,
                        "description": "Lines shown before and after each match, like grep -C; default 0"
                    },
                    "files_only": { "type": "boolean", "description": "Report only the paths of files that matched" },
                    "include_ignored": { "type": "boolean", "description": "Do not set unless strictly necessary: the default false already respects .gitignore and skips target/, node_modules/ and build output, while enabling it makes the search walk those ignored trees, which is slow. Only set true when the content you need is genuinely inside an ignored path, and then narrow the scope with path/include/max_matches" },
                    "max_matches": { "type": "integer", "minimum": 1, "maximum": MAX_MATCHES_CEILING, "description": format!("Cap on reported matches; default {DEFAULT_MAX_MATCHES}, maximum {MAX_MATCHES_CEILING}") }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, ctx: &ToolCtx, args: &str) -> Result<ToolExecResult> {
        let a: Args = parse_args(args)?;
        if a.pattern.is_empty() {
            return Ok(ToolExecResult::failed("pattern is required"));
        }
        let root = match &a.path {
            Some(p) => ctx.resolve_path(p).path,
            None => ctx.exec_cwd.clone(),
        };
        if !root.is_dir() && !root.is_file() {
            return Ok(ToolExecResult::failed(format!(
                "{} is not a file or directory",
                root.display()
            )));
        }
        let display_base = if root.is_file() {
            root.parent().unwrap_or(&root)
        } else {
            &root
        };

        let max_matches = a
            .max_matches
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_MAX_MATCHES)
            .min(MAX_MATCHES_CEILING);
        // Context and file-only are mutually exclusive by construction — a path has no context —
        // so the request is normalised rather than refused.
        let context_lines = if a.files_only {
            0
        } else {
            a.context_lines.min(MAX_CONTEXT_LINES)
        };

        let ignores = Ignores::resolve(&root, !a.include_ignored);
        let opts = Options {
            include: a.include.clone(),
            case_sensitive: a.case_mode.is_sensitive(&a.pattern),
            max_total: max_matches,
            include_ignored: a.include_ignored,
            include_hidden: false,
            exclude_dirs: ignores.excluded_dir_names(),
            exclude_files: ignores.excluded_file_names(),
            // The annotation pass is the reason to use this backend at all. It is skipped
            // automatically for `files_only`, where there is nothing to annotate.
            annotate: !a.files_only,
            context_lines,
            files_only: a.files_only,
            cancel: None,
        };

        let (result, cancelled) = run_blocking(ctx, root.clone(), a.pattern.clone(), opts).await?;
        if cancelled {
            return Ok(ToolExecResult::cancelled("search interrupted"));
        }
        let outcome = match result {
            Ok(r) => r,
            // A bad regex is the model's to fix, so it goes back as a result.
            Err(code_sitter::SearchError::BadRegex(m)) => {
                return Ok(ToolExecResult::failed(format!("invalid regex: {m}")));
            }
            Err(code_sitter::SearchError::Cancelled) => {
                return Ok(ToolExecResult::cancelled("search interrupted"));
            }
            Err(e) => return Ok(ToolExecResult::failed(format!("search failed: {e}"))),
        };

        let hits = outcome.hits;

        if hits.is_empty() {
            return Ok(ToolExecResult::success(format!(
                "no matches for /{}/ under {}{}",
                a.pattern,
                root.display(),
                match &a.include {
                    Some(g) => format!(" (include: {g})"),
                    None => String::new(),
                },
            )));
        }

        let mut body = String::new();
        let mut file_count = 0usize;
        let mut last_path: Option<std::path::PathBuf> = None;
        let mut unannotated = 0usize;
        for hit in &hits {
            if last_path.as_ref() != Some(&hit.path) {
                file_count += 1;
                last_path = Some(hit.path.clone());
            }
            if hit.hit_kind.is_none() {
                unannotated += 1;
            }
        }
        if a.files_only {
            body.push_str(&format!(
                "{} file{} for /{}/ under {}",
                hits.len(),
                if hits.len() == 1 { "" } else { "s" },
                a.pattern,
                root.display()
            ));
        } else {
            body.push_str(&format!(
                "{} match{} in {} file{} for /{}/ under {}",
                hits.len(),
                if hits.len() == 1 { "" } else { "es" },
                file_count,
                if file_count == 1 { "" } else { "s" },
                a.pattern,
                root.display()
            ));
        }
        if let Some(g) = &a.include {
            body.push_str(&format!(" (include: {g})"));
        }
        if outcome.truncated {
            body.push_str(&format!(
                " — capped at {max_matches}, narrow the search to see more"
            ));
        }
        if outcome.skipped_files > 0 {
            // Honest bookkeeping: unreadable files mean the absence of a match proves nothing.
            body.push_str(&format!(
                " — {} file(s) could not be read and were skipped",
                outcome.skipped_files
            ));
        }
        if !a.files_only && unannotated > 0 {
            body.push_str(&format!(
                " — {unannotated} hit(s) not annotated (language not parsed), so a [def] may be \
                 missing"
            ));
        }
        body.push_str(":\n\n");

        let hits_text = if a.files_only {
            render_paths(&hits, display_base)
        } else {
            let grouped = render_grouped(&hits, display_base);
            if body.chars().count() + grouped.chars().count() <= ctx.max_result_chars {
                grouped
            } else {
                render_flat(&hits, display_base)
            }
        };
        body.push_str(&hits_text);

        ctx.result_with_full_output(body.trim_end(), Recovery::Narrow)
    }
}

fn relative<'a>(path: &'a std::path::Path, base: &std::path::Path) -> &'a std::path::Path {
    path.strip_prefix(base).unwrap_or(path)
}

fn render_paths(hits: &[code_sitter::Hit], base: &std::path::Path) -> String {
    let mut out = String::new();
    for hit in hits {
        out.push_str(&format!("{}\n", relative(&hit.path, base).display()));
    }
    out
}

fn render_grouped(hits: &[code_sitter::Hit], base: &std::path::Path) -> String {
    let mut out = String::new();
    let mut current: Option<std::path::PathBuf> = None;
    for hit in hits {
        let path = relative(&hit.path, base);
        if current.as_deref() != Some(path) {
            if current.is_some() {
                out.push('\n');
            }
            out.push_str(&format!("{}:\n", path.display()));
            current = Some(path.to_path_buf());
        }
        for c in &hit.context_before {
            out.push_str(&format!("{}-{}\n", c.line, c.text));
        }
        out.push_str(&format_hit_body(hit));
        for c in &hit.context_after {
            out.push_str(&format!("{}-{}\n", c.line, c.text));
        }
    }
    out
}

fn render_flat(hits: &[code_sitter::Hit], base: &std::path::Path) -> String {
    let mut out = String::new();
    for hit in hits {
        let path = relative(&hit.path, base).display().to_string();
        for c in &hit.context_before {
            out.push_str(&format!("{path}-{}-{}\n", c.line, c.text));
        }
        out.push_str(&format_hit(&path, hit));
        for c in &hit.context_after {
            out.push_str(&format!("{path}-{}-{}\n", c.line, c.text));
        }
    }
    out
}

/// `path:line: [kind] (in symbol) text` — the flat form of one hit.
fn format_hit(path: &str, hit: &code_sitter::Hit) -> String {
    format!("{path}:{}", format_hit_body(hit))
}

fn format_hit_body(hit: &code_sitter::Hit) -> String {
    let mut line = format!("{}:", hit.line);
    if let Some(kind) = hit.hit_kind.and_then(HitKind::label) {
        line.push_str(&format!(" [{kind}]"));
    }
    if let Some(sym) = &hit.symbol {
        line.push_str(&format!(" (in {} {})", sym.kind, sym.qualified));
    }
    line.push_str(&format!(" {}\n", hit.text));
    line
}

/// Runs the search off the executor, with the conversation's cancellation bridged onto
/// `code-sitter`'s flag.
/// Returns `(result, cancelled_before_it_started)`. The second value exists because a token that
/// was already cancelled must not start a search at all — there would be nobody left to read it.
#[allow(clippy::type_complexity)]
async fn run_blocking(
    ctx: &ToolCtx,
    root: std::path::PathBuf,
    pattern: String,
    mut opts: Options,
) -> Result<(
    std::result::Result<code_sitter::SearchResult, code_sitter::SearchError>,
    bool,
)> {
    if ctx.is_cancelled() {
        return Ok((Err(code_sitter::SearchError::Cancelled), true));
    }
    let flag = Arc::new(AtomicBool::new(false));
    opts.cancel = Some(flag.clone());

    // A watcher rather than `select!` on the blocking join: the point is to make the *search*
    // return, not to stop waiting for it. Aborted as soon as the search is done so the task does
    // not outlive the call.
    let watcher = {
        let (flag, token) = (flag.clone(), ctx.cancel.clone());
        tokio::spawn(async move {
            token.cancelled().await;
            flag.store(true, Ordering::Relaxed);
        })
    };
    let joined =
        tokio::task::spawn_blocking(move || code_sitter::search(&root, &pattern, &opts)).await;
    watcher.abort();

    match joined {
        Ok(r) => Ok((r, false)),
        // The blocking pool panicked. That is not something the model can act on.
        Err(e) => Err(crate::ToolError::Failed(format!("search task failed: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ToolExecStatus, test_ctx};

    /// A tiny TypeScript project: `code-sitter` annotates TS and Python, so the symbol assertions
    /// need a language it handles.
    fn project() -> (tempfile::TempDir, ToolCtx) {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("src")).unwrap();
        std::fs::write(
            d.path().join("src/discount.ts"),
            "export function applyCoupon(subtotal: number) {\n  return subtotal - 1;\n}\n",
        )
        .unwrap();
        std::fs::write(
            d.path().join("src/total.ts"),
            "import { applyCoupon } from './discount';\n\
             export class Checkout {\n\
             \x20 computeTotal(subtotal: number) {\n\
             \x20   // applyCoupon trims it\n\
             \x20   return applyCoupon(subtotal);\n\
             \x20 }\n\
             }\n",
        )
        .unwrap();
        let mut ctx = test_ctx(d.path());
        // The default test budget is 100 chars, which would offload every result here.
        ctx.max_result_chars = 100_000;
        (d, ctx)
    }

    fn slash(s: &str) -> String {
        s.replace('\\', "/")
    }

    #[tokio::test]
    async fn finds_matches_and_reports_paths_relative_to_the_search_root() {
        let (_d, ctx) = project();
        let out = Grep
            .execute(&ctx, r#"{"pattern":"applyCoupon"}"#)
            .await
            .unwrap();

        assert_eq!(out.status, ToolExecStatus::Success);
        let text = slash(&out.model_text());
        assert!(text.contains("src/discount.ts:\n1:"), "{text}");
        assert!(text.contains("src/total.ts:\n"), "{text}");
        // Only the header names the root; every hit is relative to it, which keeps a deep-tree
        // result readable and costs far fewer tokens.
        let root = ctx.root.display().to_string();
        for line in text.lines().skip(1) {
            assert!(!line.contains(&root), "hits must be relative: {line}");
        }
    }

    #[tokio::test]
    async fn each_file_is_named_once_and_its_hits_carry_only_line_numbers() {
        let (_d, ctx) = project();
        let out = Grep
            .execute(&ctx, r#"{"pattern":"applyCoupon"}"#)
            .await
            .unwrap();
        let text = slash(&out.model_text());

        assert_eq!(
            text.matches("src/total.ts").count(),
            1,
            "the path appears once, as the group heading: {text}"
        );
        // Which file's group comes first is the filesystem's business, so the rows are checked as a
        // whole: the summary, the two headings, and the blank lines between groups are not rows.
        for line in text.lines().skip(1) {
            if line.is_empty() || line == "src/total.ts:" || line == "src/discount.ts:" {
                continue;
            }
            assert!(
                line.starts_with(|c: char| c.is_ascii_digit()),
                "under a heading every row starts with its line number: {line:?} in\n{text}"
            );
        }
    }

    /// The reason this backend exists: the definition and the call are distinguishable.
    #[tokio::test]
    async fn hits_carry_their_kind_and_enclosing_symbol() {
        let (_d, ctx) = project();
        let out = Grep
            .execute(&ctx, r#"{"pattern":"applyCoupon"}"#)
            .await
            .unwrap();
        let text = out.model_text();

        assert!(text.contains("[def]"), "the definition is labelled: {text}");
        assert!(text.contains("[call]"), "the call site is labelled: {text}");
        assert!(
            text.contains("[comment]"),
            "the mention in a comment is labelled: {text}"
        );
        assert!(text.contains("applyCoupon"), "{text}");
        // The enclosing symbol is qualified, so a call inside a method is attributable.
        assert!(text.contains("computeTotal"), "{text}");
    }

    #[tokio::test]
    async fn the_summary_is_separated_from_the_hits() {
        let (_d, ctx) = project();
        let out = Grep
            .execute(&ctx, r#"{"pattern":"applyCoupon"}"#)
            .await
            .unwrap();
        let text = out.model_text();
        let mut lines = text.lines();
        assert!(lines.next().unwrap().contains("matches in"), "{text}");
        assert_eq!(
            lines.next(),
            Some(""),
            "a blank line, then the hits: {text}"
        );
        let heading = lines.next().unwrap();
        assert!(
            heading.ends_with(':'),
            "then the first file heading: {text}"
        );
        assert!(
            lines.next().unwrap().starts_with('1'),
            "then that file's own rows, by line number: {text}"
        );
    }

    #[tokio::test]
    async fn hits_that_could_not_be_annotated_are_admitted() {
        let (d, ctx) = project();
        std::fs::write(
            d.path().join("notes.md"),
            "applyCoupon is documented here\n",
        )
        .unwrap();

        let out = Grep
            .execute(&ctx, r#"{"pattern":"applyCoupon"}"#)
            .await
            .unwrap();
        assert!(
            out.model_text().contains("1 hit(s) not annotated"),
            "{}",
            out.model_text()
        );

        let ts_only = Grep
            .execute(&ctx, r#"{"pattern":"applyCoupon","include":"*.ts"}"#)
            .await
            .unwrap();
        assert!(
            !ts_only.model_text().contains("not annotated"),
            "{}",
            ts_only.model_text()
        );
    }

    #[tokio::test]
    async fn no_match_is_a_successful_empty_answer_not_a_failure() {
        let (_d, ctx) = project();
        let out = Grep
            .execute(&ctx, r#"{"pattern":"nothingLikeThis"}"#)
            .await
            .unwrap();
        assert_eq!(
            out.status,
            ToolExecStatus::Success,
            "finding nothing is an answer"
        );
        assert!(out.model_text().contains("no matches"));
    }

    #[tokio::test]
    async fn files_only_reports_paths_without_line_text() {
        let (_d, ctx) = project();
        let out = Grep
            .execute(&ctx, r#"{"pattern":"applyCoupon","files_only":true}"#)
            .await
            .unwrap();
        let text = slash(&out.model_text());
        assert!(text.contains("src/discount.ts"), "{text}");
        assert!(
            !text.contains("return subtotal"),
            "no line text in files_only mode: {text}"
        );
    }

    #[tokio::test]
    async fn an_include_glob_narrows_the_search() {
        let (d, ctx) = project();
        std::fs::write(
            d.path().join("notes.md"),
            "applyCoupon is documented here\n",
        )
        .unwrap();

        let out = Grep
            .execute(&ctx, r#"{"pattern":"applyCoupon","include":"*.md"}"#)
            .await
            .unwrap();
        let text = out.model_text();
        assert!(text.contains("notes.md"), "{text}");
        assert!(!text.contains("discount.ts"), "{text}");
    }

    #[tokio::test]
    async fn smart_case_and_explicit_case_modes_are_honoured() {
        let (_d, ctx) = project();
        let loose = Grep
            .execute(
                &ctx,
                r#"{"pattern":"APPLYCOUPON","case_mode":"insensitive"}"#,
            )
            .await
            .unwrap();
        assert!(
            !loose.model_text().contains("no matches"),
            "{}",
            loose.model_text()
        );

        let strict = Grep
            .execute(&ctx, r#"{"pattern":"APPLYCOUPON"}"#)
            .await
            .unwrap();
        assert!(
            strict.model_text().contains("no matches"),
            "{}",
            strict.model_text()
        );
    }

    #[tokio::test]
    async fn context_lines_surround_each_match() {
        let (_d, ctx) = project();
        let out = Grep
            .execute(
                &ctx,
                r#"{"pattern":"return applyCoupon","context_lines":1}"#,
            )
            .await
            .unwrap();
        let text = slash(&out.model_text());
        assert!(text.contains("total.ts:\n4-"), "{text}");
        assert!(
            text.contains("\n5: (in method Checkout.computeTotal)"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn a_result_that_will_be_trimmed_falls_back_to_self_contained_lines() {
        let (d, mut ctx) = project();
        ctx.max_result_chars = 300;
        std::fs::write(d.path().join("many.ts"), "applyCoupon\n".repeat(40)).unwrap();

        let out = Grep
            .execute(&ctx, r#"{"pattern":"applyCoupon"}"#)
            .await
            .unwrap();
        let text = slash(&out.model_text());

        assert!(
            text.contains("omitted here"),
            "the engine trimmed this one: {text}"
        );
        for line in text.lines().skip(1) {
            assert!(
                !line.starts_with(|c: char| c.is_ascii_digit()),
                "no orphan line-number row may survive a trim: {line}"
            );
        }
    }

    /// Dependency trees are skipped even when the project never ignored them.
    #[tokio::test]
    async fn dependencies_are_skipped_without_a_gitignore() {
        let (d, ctx) = project();
        std::fs::create_dir_all(d.path().join("node_modules/dep")).unwrap();
        std::fs::write(d.path().join("node_modules/dep/index.ts"), "applyCoupon\n").unwrap();

        let out = Grep
            .execute(&ctx, r#"{"pattern":"applyCoupon"}"#)
            .await
            .unwrap();
        assert!(
            !out.model_text().contains("node_modules"),
            "{}",
            out.model_text()
        );
    }

    #[tokio::test]
    async fn ignored_hits_do_not_consume_the_match_cap() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("node_modules/pkg")).unwrap();
        std::fs::create_dir(d.path().join("src")).unwrap();
        std::fs::write(
            d.path().join("node_modules/pkg/a.ts"),
            "uniqueNeedle\nuniqueNeedle\n",
        )
        .unwrap();
        std::fs::write(d.path().join("src/z.ts"), "uniqueNeedle\n").unwrap();
        let ctx = test_ctx(d.path());

        let out = Grep
            .execute(&ctx, r#"{"pattern":"uniqueNeedle","max_matches":1}"#)
            .await
            .unwrap();

        assert_eq!(out.status, ToolExecStatus::Success);
        assert!(
            slash(&out.model_text()).contains("src/z.ts"),
            "{}",
            out.model_text()
        );
        assert!(!out.model_text().contains("node_modules"));
    }

    /// …and searchable on request, which is the whole point of the flag.
    #[tokio::test]
    async fn include_ignored_reaches_into_dependencies() {
        let (d, ctx) = project();
        std::fs::create_dir_all(d.path().join("node_modules/dep")).unwrap();
        std::fs::write(d.path().join("node_modules/dep/index.ts"), "applyCoupon\n").unwrap();

        let out = Grep
            .execute(&ctx, r#"{"pattern":"applyCoupon","include_ignored":true}"#)
            .await
            .unwrap();
        assert!(
            out.model_text().contains("node_modules"),
            "{}",
            out.model_text()
        );
    }

    #[tokio::test]
    async fn the_match_cap_is_reported_rather_than_hidden() {
        let (d, ctx) = project();
        std::fs::write(d.path().join("many.ts"), "applyCoupon\n".repeat(50)).unwrap();

        let out = Grep
            .execute(&ctx, r#"{"pattern":"applyCoupon","max_matches":5}"#)
            .await
            .unwrap();
        let text = out.model_text();
        assert!(
            text.contains("capped at 5"),
            "a silent cap reads as 'that is all there is': {text}"
        );
    }

    /// A regex the model got wrong is the model's to fix.
    #[tokio::test]
    async fn an_invalid_regex_is_a_failed_result_not_an_err() {
        let (_d, ctx) = project();
        let out = Grep.execute(&ctx, r#"{"pattern":"("}"#).await.unwrap();
        assert_eq!(out.status, ToolExecStatus::Failed);
        assert!(out.model_text().contains("invalid regex"));
    }

    #[tokio::test]
    async fn searching_one_file_is_supported() {
        let (_d, ctx) = project();
        let out = Grep
            .execute(&ctx, r#"{"pattern":"applyCoupon","path":"src/total.ts"}"#)
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Success);
        assert!(
            out.model_text().contains("total.ts"),
            "{}",
            out.model_text()
        );
    }

    /// An already-cancelled turn must not start a search nobody will read.
    #[tokio::test]
    async fn a_cancelled_turn_does_not_start_the_search() {
        let (_d, ctx) = project();
        ctx.cancel.cancel();
        let out = Grep
            .execute(&ctx, r#"{"pattern":"applyCoupon"}"#)
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Cancelled);
    }

    #[tokio::test]
    async fn a_large_result_set_is_offloaded_to_the_object_store() {
        let (d, mut ctx) = project();
        ctx.max_result_chars = 200;
        std::fs::write(d.path().join("many.ts"), "applyCoupon\n".repeat(80)).unwrap();

        let out = Grep
            .execute(&ctx, r#"{"pattern":"applyCoupon"}"#)
            .await
            .unwrap();
        assert!(
            !out.objects.is_empty(),
            "a wall of matches must not enter the context whole"
        );
        assert!(out.dangling_display_objects().is_empty());
    }

    #[tokio::test]
    async fn missing_required_args_are_rejected() {
        let (_d, ctx) = project();
        assert!(Grep.execute(&ctx, "{}").await.is_err());
        assert!(Grep.execute(&ctx, "not json").await.is_err());
    }
}
