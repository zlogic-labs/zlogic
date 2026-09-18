//! `read_file`.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use zlogic_protocol::llm::ToolDefinition;

use crate::file::convert;
use crate::file::parser;
use crate::{
    ObjectRef, ObjectRole, PromptExample, Result, Tool, ToolContent, ToolCtx, ToolDisplay,
    ToolExecResult, ToolExecStatus, ToolMeta, ToolPromptSpec, ToolRisk,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    path: String,
    /// `"start:end"` for text/docx, `"Sheet!A1:F50"` for spreadsheets. Omitted means "no range":
    /// the whole file when it fits, otherwise a structural summary to pick a range from.
    ranges: Option<Vec<String>>,
    /// Whether the read opens with the one-line header (path, shape, size). Defaults to true.
    include_metadata: Option<bool>,
}

pub struct ReadFile;

#[async_trait]
impl Tool for ReadFile {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "read_file".into(),
            source: "builtin",
            risk: ToolRisk::Read,
        }
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read_file".into(),
            description: "Read one file and return its contents as text. \
The read opens with one line naming the file and its shape — lines, or paragraphs, or sheets, \
plus the size — and then the content. \
Ranges are strings resolved by the file's own parser: text/code and DOCX take lines like \
\"1:100\" (1-based inclusive), spreadsheets take a sheet and cell rectangle like \
\"Sheet1!A1:F50\" or a bare row range \"Sheet1!1:10\". \
With no ranges, a text file is read whole when it fits the budget, a spreadsheet answers with \
its sheets (name, rows, columns) so you can pick a range, a document gives its paragraphs, and a \
code file too large to read whole gives its definitions instead — one \"kind qualified \
start:end\" per line, languages TS/TSX/JS, Python, Rust, Java, Go, C/C++, Ruby, PHP, C#, Zig, Lua \
— so the next read can name exact line ranges. \
Anything the tool has to say about the content rather than being the content (a conversion, a \
cut and the exact range that continues it) is a bracketed note. \
Images (PNG, JPEG, GIF, WebP) and audio/video are attached for viewing/playback instead of read \
as text."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "minLength": 1, "description": "Absolute, or relative to the working directory" },
                    "ranges": {
                        "type": "array",
                        "description": "Per-kind range strings (see description), e.g. [\"1:40\",\"200:260\"]. Omit or pass an empty array to read the whole file / structural summary.",
                        "items": { "type": "string" }
                    },
                    "include_metadata": {
                        "type": "boolean",
                        "description": "Whether the read opens with the one-line header (path, shape, size). Defaults to true."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        }
    }

    fn prompt_spec(&self) -> Option<ToolPromptSpec> {
        Some(ToolPromptSpec {
            when: "Read file contents whenever the answer depends on what a file actually says \
                (code to reason about, data to inspect, a document to summarise).",
            contract: "One call reads one file. Several files you already know you need are read in \
                the same round — one call each, all in one message; leaving them to later rounds \
                wastes rounds. \
                Only request the parts you need: give `ranges` for large files or when you know the \
                relevant lines (\"1:120\", \"1200:1400\"). Omit `ranges` only when the file is small \
                or when you first need its structure (a spreadsheet then answers with its sheets, \
                a big code file with its definitions, so the next read can pick a range). \
                Range syntax is per file kind: text/code/DOCX take \"start:end\" line ranges \
                (1-based, inclusive); spreadsheets take \"Sheet!A1:F50\" cell rectangles or \
                \"Sheet!1:10\" row ranges. \
                Do not re-read a file or range whose content is already in this conversation. A \
                cut read ends with a bracketed note naming the exact range that continues it — \
                continue from exactly that range.",
            positive_examples: &[
                r#"{"path":"src/lib.rs","ranges":["1:80","200:260"]}"#,
                r#"{"path":"data/sales.xlsx","ranges":["Summary!A1:H20"]}"#,
            ],
            negative_examples: &[
                PromptExample {
                    args: r#"{"path":"src/lib.rs"}"#,
                    why: "whole-file read of a large file floods the context; give a range instead",
                },
                PromptExample {
                    args: r#"{"files":[{"path":"a.rs"}]}"#,
                    why: "one call reads one file: pass \"path\" (and \"ranges\") directly; for several files make several calls in the same message",
                },
                PromptExample {
                    args: r#"{"path":"book.xlsx","ranges":["A1:F50"]}"#,
                    why: "a multi-sheet workbook needs the sheet name, e.g. \"Sheet1!A1:F50\" (omit it only when the workbook has exactly one sheet)",
                },
            ],
        })
    }

    async fn execute(&self, ctx: &ToolCtx, args: &str) -> Result<ToolExecResult> {
        let a: Args = crate::parse_args_with_prompt(args, self.prompt_spec())?;
        let mut out = read_one_file(ctx, &a)?;

        let body: String = out
            .content
            .iter()
            .filter_map(|c| match c {
                ToolContent::Text(t) => Some(t.as_str()),
                ToolContent::File(_) => None,
            })
            .collect::<Vec<_>>()
            .join("");
        if !body.is_empty()
            && let Ok(id) = ctx.objects.put(body.as_bytes())
        {
            out.display.push(ToolDisplay::Output {
                object_id: id.clone(),
                total_chars: body.chars().count() as u64,
                truncated: false,
            });
            out.objects.push(ObjectRef::new(id, ObjectRole::Output));
        }
        Ok(out)
    }
}

/// Reads the one requested file, and returns the call's whole result.
fn read_one_file(ctx: &ToolCtx, a: &Args) -> Result<ToolExecResult> {
    let resolved = match ctx.resolve_required_path("path", &a.path) {
        Ok(r) => r,
        Err(e) => return Ok(error_block(format!("cannot read {:?}: {e}", a.path))),
    };
    let display = display_for_reuse(&resolved.path, &ctx.exec_cwd);
    let path_str = resolved.path.to_string_lossy().into_owned();
    // A directory is not a file: fail with the right next step instead of fs::read's
    // platform-y "Is a directory" error.
    if resolved.path.is_dir() {
        return Ok(error_block(format!(
            "{display} is a directory — use list_dir to browse it"
        )));
    }
    let raw = match std::fs::read(&resolved.path) {
        Ok(b) => b,
        Err(e) => {
            return Ok(error_block(read_error_message(
                &a.path,
                &resolved.path,
                &e,
                ctx,
            )));
        }
    };
    let bytes = raw.len() as u64;
    let budget = ctx.max_result_chars;
    let show_metadata = a.include_metadata.unwrap_or(true);

    let mut result = ToolExecResult::new(ToolExecStatus::Success);

    match convert::classify(&resolved.path, &raw) {
        // Text, spreadsheets, documents: the parser renders the whole model-facing text.
        convert::Kind::Text | convert::Kind::Table | convert::Kind::Docx => {
            let rendered = match parser::render(
                &resolved.path,
                &display,
                &raw,
                a.ranges.as_deref(),
                budget,
                show_metadata,
            ) {
                Ok(r) => r,
                Err(e) => {
                    // A parser failure (unreadable workbook, zip bomb, bad range) is a file-level
                    // error, not a call failure.
                    result.status = ToolExecStatus::Failed;
                    result
                        .content
                        .push(ToolContent::text(format!("{display}: {e}\n")));
                    return Ok(result);
                }
            };
            result.content.push(ToolContent::text(rendered.content));
            attach_file_card(&mut result, ctx, &path_str, &a.path, bytes, &raw)?;
            Ok(result)
        }

        // Images and media are attached as structured files; the model gets one line describing
        // the attachment, never the bytes as text.
        convert::Kind::Image(mime) | convert::Kind::Media(mime) => {
            let ranges_requested = a.ranges.as_deref().is_some_and(|r| !r.is_empty());
            if ranges_requested {
                result.status = ToolExecStatus::Failed;
            }
            let is_image = matches!(
                convert::classify(&resolved.path, &raw),
                convert::Kind::Image(_)
            );
            let name = file_basename(&resolved.path);
            let captured = ctx.capture_file(name, mime, &raw)?;
            result = captured.add_to(result);
            if let Some(ToolDisplay::File { path, .. }) = result.display.first_mut() {
                *path = path_str.clone();
            }
            let note = if ranges_requested {
                format!(
                    "line ranges do not apply here; read it whole instead. {mime}, {bytes} bytes"
                )
            } else if is_image {
                format!("{mime}, {bytes} bytes — the image is attached for you to view")
            } else {
                format!("{mime}, {bytes} bytes — the file is captured for you to preview")
            };
            result
                .content
                .insert(0, ToolContent::text(format!("{display}: {note}\n")));
            Ok(result)
        }

        convert::Kind::Binary => {
            let name = file_basename(&resolved.path);
            let object_id = ctx.objects.put(&raw)?;
            let mime = ToolDisplay::guess_mime(&a.path).map(str::to_string);
            result.status = ToolExecStatus::Failed;
            result.display.push(ToolDisplay::File {
                path: path_str.clone(),
                mime: mime.clone(),
                bytes,
                total_lines: None,
                object_id: Some(object_id.clone()),
            });
            result
                .objects
                .push(ObjectRef::keyed(object_id, ObjectRole::Output, name));
            result.content.push(ToolContent::text(format!(
                "{display}: looks like a binary file ({bytes} bytes); not read\n"
            )));
            Ok(result)
        }
    }
}

/// Attaches the standard `read_file` file card. Every file the tool read is captured into the
/// object store so the UI can offer "preview" uniformly — the object is content-addressed, so
/// re-reading the same file does not duplicate storage. Converted files (spreadsheets, documents)
/// and truncated reads store the original bytes; plain text stores the same bytes the model saw.
fn attach_file_card(
    result: &mut ToolExecResult,
    ctx: &ToolCtx,
    path_str: &str,
    path_arg: &str,
    bytes: u64,
    raw: &[u8],
) -> Result<()> {
    let mime = ToolDisplay::guess_mime(path_arg).map(str::to_string);
    let total_lines = None;

    let object_id = match ctx.objects.put(raw) {
        Ok(id) => {
            result
                .objects
                .push(ObjectRef::new(id.clone(), ObjectRole::Output));
            Some(id)
        }
        Err(_) => None,
    };

    result.display.push(ToolDisplay::File {
        path: path_str.to_string(),
        mime,
        bytes,
        total_lines,
        object_id,
    });
    Ok(())
}

/// A read that could not happen: the message is the whole answer, so it is the whole text. No
/// display card either — there is nothing to preview.
fn error_block(message: String) -> ToolExecResult {
    let mut result = ToolExecResult::new(ToolExecStatus::Failed);
    result
        .content
        .push(ToolContent::text(format!("{message}\n")));
    result
}

/// Turns an `fs::read` failure into a model-facing message: which absolute path failed, why, and —
/// for a missing file — copy-pasteable lookalikes found nearby.
fn read_error_message(raw: &str, resolved: &Path, error: &std::io::Error, ctx: &ToolCtx) -> String {
    let display = display_for_reuse(resolved, &ctx.exec_cwd);
    match error.kind() {
        std::io::ErrorKind::NotFound => {
            let mut message = format!("cannot read {display}: no such file");
            let lookalikes = suggest_similar_paths(raw, ctx);
            if lookalikes.is_empty() {
                message.push_str(
                    " — check the spelling, or use list_dir on a parent directory to see \
                     what is actually there",
                );
            } else if lookalikes.len() == 1 {
                message.push_str(&format!(" — did you mean {:?}?", lookalikes[0]));
            } else {
                message.push_str(" — did you mean ");
                for (i, candidate) in lookalikes.iter().enumerate() {
                    if i > 0 {
                        message.push_str(", ");
                    }
                    message.push_str(&format!("{candidate:?}"));
                }
                message.push('?');
            }
            message
        }
        std::io::ErrorKind::PermissionDenied => format!(
            "cannot read {display}: permission denied — the file exists but cannot be opened; \
             check its access rights"
        ),
        _ => format!("cannot read {display}: {error}"),
    }
}

/// Best-effort lookalikes for a path that does not exist, as copy-pasteable paths (relative to
/// the working directory when inside it, `/`-separated).
/// Runs on the error path of a failed read, so it must stay cheap even in a huge repo: a bounded
/// breadth-first walk (pruning vendored/build trees) that collects files whose name equals the
/// requested one, case-insensitively first, then contains it. Only the top three come back.
fn suggest_similar_paths(raw: &str, ctx: &ToolCtx) -> Vec<String> {
    let Some(wanted) = Path::new(raw).file_name().and_then(|n| n.to_str()) else {
        return Vec::new();
    };

    let mut matches = find_lookalikes(&ctx.exec_cwd, wanted, 8_000);
    if matches.is_empty() && ctx.root != ctx.exec_cwd {
        matches = find_lookalikes(&ctx.root, wanted, 8_000);
    }

    matches
        .into_iter()
        .map(|path| display_for_reuse(&path, &ctx.exec_cwd))
        .collect()
}

/// Paths we never search inside when hunting lookalikes (vendored/build noise dwarfs anything
/// the agent meant to type).
fn is_prunable_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| {
            matches!(
                name,
                ".git"
                    | ".hg"
                    | ".svn"
                    | "node_modules"
                    | "target"
                    | "dist"
                    | "build"
                    | "vendor"
                    | ".cargo"
                    | ".wrangler"
                    | ".zlogic"
            )
        })
}

/// Bounded BFS from `root`: files whose name matches `wanted` case-insensitively first; if that
/// finds nothing, files that merely contain it. `scanned_cap` stops the walk no matter what.
fn find_lookalikes(root: &Path, wanted: &str, scanned_cap: usize) -> Vec<PathBuf> {
    let wanted_lower = wanted.to_lowercase();

    let mut exact: Vec<PathBuf> = Vec::new();
    let mut fuzzy: Vec<PathBuf> = Vec::new();
    let mut scanned: usize = 0;
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    // Depth guard: a `crates/x/src/routes/foo.rs` style layout is ~4 levels; anything deeper is
    // beyond what a typo-correction pass should chase.
    while let Some(dir) = stack.pop() {
        if dir.components().count() - root.components().count() > 6 {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            scanned += 1;
            if scanned > scanned_cap {
                return Vec::new(); // gave up mid-walk — report nothing rather than half results
            }
            let path = entry.path();
            let Ok(ty) = entry.file_type() else { continue };
            if ty.is_dir() {
                if !is_prunable_dir(&path) {
                    stack.push(path);
                }
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let lower = name.to_lowercase();
            if lower == wanted_lower {
                exact.push(path);
            } else if lower.contains(&wanted_lower) {
                fuzzy.push(path);
            }
        }
        if exact.len() >= 3 {
            break;
        }
    }

    exact.sort();
    exact.truncate(3);
    if exact.is_empty() {
        fuzzy.sort();
        fuzzy.truncate(3);
    }
    if exact.is_empty() { fuzzy } else { exact }
}

/// Paths are shown the way the agent writes them back into tool args: `/`-separated, relative to
/// the working directory when inside it.
fn display_for_reuse(path: &Path, exec_cwd: &Path) -> String {
    let norm = |p: &Path| p.to_string_lossy().replace('\\', "/");
    if let Ok(rel) = path.strip_prefix(exec_cwd) {
        return norm(rel);
    }
    norm(path)
}

/// The file's own name — the display card's label and the object-store key.
fn file_basename(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file")
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_ctx;

    fn slash(s: &str) -> String {
        s.replace('\\', "/")
    }

    fn project() -> (tempfile::TempDir, ToolCtx) {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("src")).unwrap();
        std::fs::write(d.path().join("src/a.rs"), "l1\nl2\nl3\n").unwrap();
        let mut ctx = test_ctx(d.path());
        ctx.max_result_chars = 10_000;
        (d, ctx)
    }

    #[tokio::test]
    async fn a_small_file_is_read_whole_under_a_one_line_header() {
        let (_d, ctx) = project();
        let out = ReadFile
            .execute(&ctx, r#"{"path":"src/a.rs"}"#)
            .await
            .unwrap();

        assert_eq!(out.status, ToolExecStatus::Success);
        let text = slash(&out.model_text());
        let (head, body) = text.split_once("\n\n").unwrap();
        assert!(head.starts_with("src/a.rs (3 lines, "), "{text}");
        assert_eq!(body, "l1\nl2\nl3\n", "{text}");
    }

    #[tokio::test]
    async fn without_metadata_the_output_is_the_file_itself() {
        let (_d, ctx) = project();
        let out = ReadFile
            .execute(&ctx, r#"{"path":"src/a.rs","include_metadata":false}"#)
            .await
            .unwrap();
        assert_eq!(out.model_text(), "l1\nl2\nl3\n");
    }

    #[tokio::test]
    async fn ranges_are_labelled_and_the_gap_between_them_is_dropped() {
        let (d, ctx) = project();
        std::fs::write(d.path().join("src/a.rs"), "l1\nl2\nl3\nl4\nl5\n").unwrap();
        let out = ReadFile
            .execute(&ctx, r#"{"path":"src/a.rs","ranges":["1:2","4:5"]}"#)
            .await
            .unwrap();

        let text = slash(&out.model_text());
        assert!(text.contains("--- lines 1-2 ---\nl1\nl2\n"), "{text}");
        assert!(text.contains("--- lines 4-5 ---\nl4\nl5\n"), "{text}");
        assert!(!text.contains("l3"), "{text}");
    }

    #[tokio::test]
    async fn a_cut_read_names_the_exact_range_that_continues_it() {
        let (d, mut ctx) = project();
        ctx.max_result_chars = 80;
        std::fs::write(d.path().join("src/a.rs"), "line\n".repeat(60)).unwrap();

        let out = ReadFile
            .execute(&ctx, r#"{"path":"src/a.rs"}"#)
            .await
            .unwrap();
        let text = slash(&out.model_text());

        assert!(text.contains("cut at the read limit"), "{text}");
        let note = text.lines().find(|l| l.starts_with("[cut")).unwrap();
        assert!(note.contains("Continue with {\"ranges\":[\""), "{note}");
        assert!(
            note.contains("of 60 lines") || note.contains("60 lines"),
            "{note}"
        );
        let start: usize = note
            .split("[\"")
            .nth(1)
            .and_then(|s| s.split(':').next())
            .and_then(|s| s.parse().ok())
            .unwrap();
        assert!(start > 1, "{note}");
    }

    #[tokio::test]
    async fn a_file_outside_the_workspace_is_named_absolutely() {
        let (d, ctx) = project();
        let outside = tempfile::tempdir().unwrap();
        let path = outside.path().join("b.txt");
        std::fs::write(&path, "x\n").unwrap();

        let out = ReadFile
            .execute(&ctx, &format!(r#"{{"path":{}}}"#, json_path(&path)))
            .await
            .unwrap();
        let text = slash(&out.model_text());
        assert!(
            text.starts_with(&slash(&path.to_string_lossy())),
            "absolute for a file outside the workspace: {text}"
        );
        assert!(
            !text.contains(d.path().to_string_lossy().as_ref()),
            "{text}"
        );
    }

    #[tokio::test]
    async fn a_missing_file_fails_with_its_path_and_no_markup() {
        let (_d, ctx) = project();
        let out = ReadFile
            .execute(&ctx, r#"{"path":"src/nope.rs"}"#)
            .await
            .unwrap();

        assert_eq!(out.status, ToolExecStatus::Failed);
        let text = slash(&out.model_text());
        assert!(text.contains("cannot read src/nope.rs"), "{text}");
        assert!(!text.contains('<'), "no markup in the answer: {text}");
    }

    #[tokio::test]
    async fn an_invalid_range_is_a_failed_result_not_a_panic() {
        let (_d, ctx) = project();
        let out = ReadFile
            .execute(&ctx, r#"{"path":"src/a.rs","ranges":["abc"]}"#)
            .await
            .unwrap();

        assert_eq!(out.status, ToolExecStatus::Failed);
        assert!(
            out.model_text().contains("must look like 1:20"),
            "{}",
            out.model_text()
        );
    }

    fn json_path(path: &Path) -> String {
        serde_json::to_string(&path.to_string_lossy().replace('\\', "/")).unwrap()
    }
}
