//! `glob` — find paths by filename pattern, newest first.
//! # Why newest first
//! Modification time is the closest thing to a relevance signal a filename search has. Someone
//! asking for `**/*.test.ts` in a repository with four hundred of them is nearly always looking at
//! whatever is being worked on now, and alphabetical order buries that under `__fixtures__`. When
//! the list is capped, mtime order means the cap drops the least likely candidates rather than
//! everything after the letter `d`.

use std::path::PathBuf;
use std::time::SystemTime;

use async_trait::async_trait;
use globset::GlobBuilder;
use serde::Deserialize;
use serde_json::json;
use zlogic_protocol::llm::ToolDefinition;

use crate::search::CaseMode;
use crate::search::ignores::Ignores;
use crate::{Recovery, Result, Tool, ToolCtx, ToolExecResult, ToolMeta, ToolRisk, parse_args};

const DEFAULT_MAX_RESULTS: usize = 100;
const MAX_RESULTS_CEILING: usize = 1_000;

/// Where the walk stops even if nothing matched.
/// A pattern rooted at `/` or `~` would otherwise walk the whole disk before reporting nothing.
/// The limit is on *entries visited*, not matches, because that is the cost that runs away.
const MAX_VISITED: usize = 200_000;

#[derive(Debug, Deserialize)]
struct Args {
    pattern: String,
    /// Defaults to the working directory.
    path: Option<String>,
    #[serde(default)]
    case_mode: CaseMode,
    /// Also search `.gitignore`d paths and generated/dependency trees.
    #[serde(default)]
    include_ignored: bool,
    max_results: Option<usize>,
}

pub struct Glob;

#[async_trait]
impl Tool for Glob {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "glob".into(),
            source: "builtin",
            risk: ToolRisk::Read,
        }
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "glob".into(),
            description: "Find files and directories by path pattern, e.g. \"**/*.rs\" or \
                          \"docs/*.md\". Returns paths sorted by modification time, newest \
                          first, at most 100 of them. Use this when you know roughly where \
                          something lives but not its exact path. To search inside file \
                          contents use grep."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "minLength": 1, "description": "Glob pattern, e.g. \"**/*.rs\" or \"src/**/*.{js,ts}\"" },
                    "path": { "type": "string", "description": "Directory to search from; defaults to the working directory" },
                    "case_mode": { "type": "string", "enum": ["smart", "sensitive", "insensitive"], "description": "Case handling; smart (default) is sensitive only when the pattern contains uppercase letters" },
                    "include_ignored": { "type": "boolean", "description": "Do not set unless strictly necessary: the default false already respects .gitignore and skips target/, node_modules/ and build output, while enabling it walks those ignored trees, which is slow. Only set true when the paths you need are genuinely inside an ignored directory, and then narrow the scope with path/max_results" },
                    "max_results": { "type": "integer", "minimum": 1, "maximum": MAX_RESULTS_CEILING, "description": format!("Maximum paths returned; default {DEFAULT_MAX_RESULTS}, maximum {MAX_RESULTS_CEILING}") }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, ctx: &ToolCtx, args: &str) -> Result<ToolExecResult> {
        let a: Args = parse_args(args)?;
        if a.pattern.trim().is_empty() {
            return Ok(ToolExecResult::failed("pattern is required"));
        }
        let root = match &a.path {
            Some(p) => ctx.resolve_path(p).path,
            None => ctx.exec_cwd.clone(),
        };
        if !root.is_dir() {
            return Ok(ToolExecResult::failed(format!(
                "{} is not a directory",
                root.display()
            )));
        }

        // `literal_separator` is what makes `*` stop at a directory boundary and `**` the only
        // way to cross one. Without it `src/*.rs` would match `src/a/b.rs`, which is not what the
        // pattern says anywhere else the user has met globs.
        let glob = match GlobBuilder::new(&a.pattern)
            .case_insensitive(!a.case_mode.is_sensitive(&a.pattern))
            .literal_separator(true)
            .build()
        {
            Ok(g) => g.compile_matcher(),
            Err(e) => return Ok(ToolExecResult::failed(format!("invalid pattern: {e}"))),
        };

        let ignores = Ignores::resolve(&root, !a.include_ignored);
        let max_results = a
            .max_results
            .unwrap_or(DEFAULT_MAX_RESULTS)
            .clamp(1, MAX_RESULTS_CEILING);
        let mut matches: Vec<(SystemTime, PathBuf)> = Vec::new();
        let mut visited = 0usize;
        let mut capped_walk = false;

        for entry in ignores.walker(&root).build() {
            if ctx.is_cancelled() {
                return Ok(ToolExecResult::cancelled("search interrupted"));
            }
            // An unreadable directory is skipped silently: a permission error deep in a tree is
            // not what the caller asked about, and one such entry must not fail the whole search.
            let Ok(entry) = entry else { continue };
            visited += 1;
            if visited > MAX_VISITED {
                capped_walk = true;
                break;
            }
            let Ok(rel) = entry.path().strip_prefix(&root) else {
                continue;
            };
            if rel.as_os_str().is_empty() || !glob.is_match(rel) {
                continue;
            }
            // A file whose mtime cannot be read still belongs in the results — it sorts last
            // rather than disappearing.
            let mtime = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            matches.push((mtime, entry.path().to_path_buf()));
        }

        if matches.is_empty() {
            return Ok(ToolExecResult::success(format!(
                "no paths match \"{}\" under {}",
                a.pattern,
                root.display()
            )));
        }

        // Newest first, then by path so equal timestamps do not reorder between calls — an
        // unstable list looks like the tree changed when it did not.
        matches.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let total = matches.len();
        let shown = total.min(max_results);

        let mut body = format!(
            "{total} path(s) matching \"{}\" under {}",
            a.pattern,
            root.display()
        );
        if total > shown {
            body.push_str(&format!(" — showing the {shown} most recently modified"));
        }
        if capped_walk {
            body.push_str(&format!(
                " — the walk stopped after {MAX_VISITED} entries, so there may be more"
            ));
        }
        body.push_str(":\n");
        for (_, path) in matches.iter().take(shown) {
            let shown_path = path.strip_prefix(&root).unwrap_or(path);
            body.push_str(&format!("{}\n", shown_path.display()));
        }

        ctx.result_with_full_output(body.trim_end(), Recovery::Narrow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ToolExecStatus, test_ctx};

    fn setup() -> (tempfile::TempDir, ToolCtx) {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("src/deep")).unwrap();
        std::fs::write(d.path().join("src/a.rs"), "a").unwrap();
        std::fs::write(d.path().join("src/deep/b.rs"), "b").unwrap();
        std::fs::write(d.path().join("notes.md"), "n").unwrap();
        let mut ctx = test_ctx(d.path());
        ctx.max_result_chars = 100_000;
        (d, ctx)
    }

    fn slash(s: &str) -> String {
        s.replace('\\', "/")
    }

    #[tokio::test]
    async fn finds_files_at_any_depth() {
        let (_d, ctx) = setup();
        let out = Glob
            .execute(&ctx, r#"{"pattern":"**/*.rs"}"#)
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Success);
        let text = slash(&out.model_text());
        assert!(text.contains("src/a.rs"), "{text}");
        assert!(text.contains("src/deep/b.rs"), "{text}");
        assert!(!text.contains("notes.md"), "{text}");
    }

    /// `*` must not cross a directory boundary; only `**` does.
    #[tokio::test]
    async fn a_single_star_stays_within_one_directory() {
        let (_d, ctx) = setup();
        let out = Glob
            .execute(&ctx, r#"{"pattern":"src/*.rs"}"#)
            .await
            .unwrap();
        let text = slash(&out.model_text());
        assert!(text.contains("src/a.rs"), "{text}");
        assert!(
            !text.contains("deep/b.rs"),
            "src/*.rs is not src/**/*.rs: {text}"
        );
    }

    #[tokio::test]
    async fn newest_comes_first() {
        let (d, ctx) = setup();
        // Rewriting b.rs makes it the most recently modified.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(d.path().join("src/deep/b.rs"), "touched").unwrap();

        let out = Glob
            .execute(&ctx, r#"{"pattern":"**/*.rs"}"#)
            .await
            .unwrap();
        let text = out.model_text();
        let first = text.lines().nth(1).unwrap();
        assert!(
            first.contains("b.rs"),
            "the most recently touched file leads: {text}"
        );
    }

    #[tokio::test]
    async fn smart_case_and_explicit_case_modes_are_honoured() {
        let (_d, ctx) = setup();
        let loose = Glob
            .execute(&ctx, r#"{"pattern":"**/*.RS","case_mode":"insensitive"}"#)
            .await
            .unwrap();
        assert!(
            loose.model_text().contains("a.rs"),
            "{}",
            loose.model_text()
        );

        let strict = Glob
            .execute(&ctx, r#"{"pattern":"**/*.RS"}"#)
            .await
            .unwrap();
        assert!(
            strict.model_text().contains("no paths match"),
            "{}",
            strict.model_text()
        );
    }

    #[tokio::test]
    async fn directories_match_too() {
        let (_d, ctx) = setup();
        let out = Glob
            .execute(&ctx, r#"{"pattern":"src/deep"}"#)
            .await
            .unwrap();
        assert!(
            slash(&out.model_text()).contains("src/deep"),
            "{}",
            out.model_text()
        );
    }

    #[tokio::test]
    async fn dependencies_are_skipped_by_default_and_reachable_on_request() {
        let (d, ctx) = setup();
        std::fs::create_dir_all(d.path().join("node_modules/pkg")).unwrap();
        std::fs::write(d.path().join("node_modules/pkg/i.rs"), "x").unwrap();

        let default = Glob
            .execute(&ctx, r#"{"pattern":"**/*.rs"}"#)
            .await
            .unwrap();
        assert!(
            !default.model_text().contains("node_modules"),
            "{}",
            default.model_text()
        );

        let all = Glob
            .execute(&ctx, r#"{"pattern":"**/*.rs","include_ignored":true}"#)
            .await
            .unwrap();
        assert!(
            all.model_text().contains("node_modules"),
            "{}",
            all.model_text()
        );
    }

    /// A `.gitignore` is honoured, and overridable.
    #[tokio::test]
    async fn gitignored_paths_are_skipped_by_default() {
        let (d, ctx) = setup();
        std::fs::write(d.path().join(".gitignore"), "generated/\n").unwrap();
        std::fs::create_dir(d.path().join("generated")).unwrap();
        std::fs::write(d.path().join("generated/g.rs"), "g").unwrap();

        let default = Glob
            .execute(&ctx, r#"{"pattern":"**/*.rs"}"#)
            .await
            .unwrap();
        assert!(
            !default.model_text().contains("generated"),
            "{}",
            default.model_text()
        );

        let all = Glob
            .execute(&ctx, r#"{"pattern":"**/*.rs","include_ignored":true}"#)
            .await
            .unwrap();
        assert!(
            all.model_text().contains("generated"),
            "{}",
            all.model_text()
        );
    }

    /// With a `.gitignore` present, `build/` is the project's call — and it did not exclude it.
    #[tokio::test]
    async fn an_ambiguous_build_directory_survives_when_a_gitignore_exists() {
        let (d, ctx) = setup();
        std::fs::write(d.path().join(".gitignore"), "coverage/\n").unwrap();
        std::fs::create_dir(d.path().join("build")).unwrap();
        std::fs::write(d.path().join("build/script.rs"), "s").unwrap();

        let out = Glob
            .execute(&ctx, r#"{"pattern":"**/*.rs"}"#)
            .await
            .unwrap();
        assert!(
            slash(&out.model_text()).contains("build/script.rs"),
            "{}",
            out.model_text()
        );
    }

    #[tokio::test]
    async fn no_match_is_a_successful_empty_answer() {
        let (_d, ctx) = setup();
        let out = Glob
            .execute(&ctx, r#"{"pattern":"**/*.zig"}"#)
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Success);
        assert!(out.model_text().contains("no paths match"));
    }

    /// The cap must announce itself: silence reads as "that is all there is".
    #[tokio::test]
    async fn the_result_cap_is_reported() {
        let (d, ctx) = setup();
        for i in 0..DEFAULT_MAX_RESULTS + 10 {
            std::fs::write(d.path().join(format!("f{i}.txt")), "x").unwrap();
        }
        let out = Glob.execute(&ctx, r#"{"pattern":"*.txt"}"#).await.unwrap();
        let text = out.model_text();
        assert!(
            text.contains(&format!("showing the {DEFAULT_MAX_RESULTS}")),
            "{text}"
        );
        assert_eq!(
            text.lines().count(),
            DEFAULT_MAX_RESULTS + 1,
            "header plus the capped list"
        );
    }

    #[tokio::test]
    async fn an_invalid_pattern_is_a_failed_result_not_an_err() {
        let (_d, ctx) = setup();
        let out = Glob
            .execute(&ctx, r#"{"pattern":"[unclosed"}"#)
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Failed);
        assert!(out.model_text().contains("invalid pattern"));
    }

    #[tokio::test]
    async fn searching_a_path_that_is_not_a_directory_is_reported() {
        let (_d, ctx) = setup();
        let out = Glob
            .execute(&ctx, r#"{"pattern":"*","path":"notes.md"}"#)
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Failed);
        assert!(out.model_text().contains("not a directory"));
    }

    #[tokio::test]
    async fn a_cancelled_turn_stops_the_walk() {
        let (_d, ctx) = setup();
        ctx.cancel.cancel();
        let out = Glob.execute(&ctx, r#"{"pattern":"**/*"}"#).await.unwrap();
        assert_eq!(out.status, ToolExecStatus::Cancelled);
    }

    #[tokio::test]
    async fn missing_required_args_are_rejected() {
        let (_d, ctx) = setup();
        assert!(Glob.execute(&ctx, "{}").await.is_err());
        assert!(Glob.execute(&ctx, "not json").await.is_err());
    }
}
