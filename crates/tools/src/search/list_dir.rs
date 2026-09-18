//! `list_dir` — what is in a directory, flat or as a tree.
//! # Two modes because they answer two questions
//! Flat is "what is in here" and carries sizes. Tree is "how is this laid out", and its value is
//! the shape, not the detail — which is why it is depth- and entry-limited by default. An
//! unbounded tree of a real repository is tens of thousands of lines, and a model that receives
//! one has spent its context on `node_modules` instead of on the code.
//! # A truncated listing always says so
//! Both limits are reported when they bite. A silently cut listing is worse than a short one: the
//! model concludes a file does not exist and stops looking for it.

use std::path::Path;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use zlogic_protocol::llm::ToolDefinition;

use crate::search::ignores::Ignores;
use crate::{Recovery, Result, Tool, ToolCtx, ToolExecResult, ToolMeta, ToolRisk, parse_args};

const DEFAULT_MAX_DEPTH: usize = 3;
const MAX_DEPTH_CEILING: usize = 10;
const DEFAULT_MAX_ENTRIES: usize = 200;
const MAX_ENTRIES_CEILING: usize = 2_000;

#[derive(Debug, Deserialize)]
struct Args {
    /// Defaults to the working directory.
    path: Option<String>,
    /// A tree instead of the direct children.
    #[serde(default)]
    recursive: bool,
    max_depth: Option<usize>,
    max_entries: Option<usize>,
    /// Also list `.gitignore`d paths and generated/dependency trees.
    #[serde(default)]
    include_ignored: bool,
}

pub struct ListDir;

#[async_trait]
impl Tool for ListDir {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "list_dir".into(),
            source: "builtin",
            risk: ToolRisk::Read,
        }
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "list_dir".into(),
            description: "List a directory. By default its direct children with sizes; with \
                          recursive, a depth-limited tree. Dependencies, build output and \
                          .gitignored paths are left out. Use glob for path patterns and grep \
                          for file contents."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Directory, absolute or relative to the working directory; defaults to the working directory" },
                    "recursive": { "type": "boolean", "description": "Print a tree instead of the direct children; default false" },
                    "max_depth": {
                        "type": "integer", "minimum": 1, "maximum": MAX_DEPTH_CEILING,
                        "description": format!("Tree depth, children of the root being depth 1; default {DEFAULT_MAX_DEPTH}")
                    },
                    "max_entries": {
                        "type": "integer", "minimum": 1, "maximum": MAX_ENTRIES_CEILING,
                        "description": format!("Cap on entries printed; default {DEFAULT_MAX_ENTRIES}")
                    },
                    "include_ignored": { "type": "boolean", "description": "Do not set unless strictly necessary: the default false already respects .gitignore and skips target/, node_modules/ and build output, while enabling it lists those ignored trees, which is slow. Only set true when the paths you need are genuinely inside an ignored directory, and then narrow the scope with path/max_depth/max_entries" }
                },
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, ctx: &ToolCtx, args: &str) -> Result<ToolExecResult> {
        let a: Args = parse_args(args)?;
        let root = match &a.path {
            Some(path) if !path.trim().is_empty() => ctx.resolve_path(path).path,
            _ => ctx.exec_cwd.clone(),
        };
        let max_depth = a
            .max_depth
            .unwrap_or(DEFAULT_MAX_DEPTH)
            .clamp(1, MAX_DEPTH_CEILING);
        let max_entries = a
            .max_entries
            .unwrap_or(DEFAULT_MAX_ENTRIES)
            .clamp(1, MAX_ENTRIES_CEILING);
        let ignores = Ignores::resolve(&root, !a.include_ignored);

        if !root.is_dir() {
            return Ok(ToolExecResult::failed(if root.exists() {
                format!("{} is not a directory", root.display())
            } else {
                format!("{} does not exist", root.display())
            }));
        }

        let mut printed = 0usize;
        let mut body = String::new();
        if a.recursive {
            body.push_str(&format!("{} (depth {max_depth}):\n", root.display()));
            if let Err(e) = tree(
                &root,
                &ignores,
                max_depth,
                max_entries,
                1,
                "",
                &mut printed,
                &mut body,
            ) {
                return Ok(ToolExecResult::failed(format!(
                    "cannot read {}: {e}",
                    root.display()
                )));
            }
        } else {
            body.push_str(&format!("{}:\n", root.display()));
            let rows = match read_children(&root, &ignores) {
                Ok(r) => r,
                Err(e) => {
                    return Ok(ToolExecResult::failed(format!(
                        "cannot read {}: {e}",
                        root.display()
                    )));
                }
            };
            for row in rows.iter().take(max_entries) {
                body.push_str(&format!("{}\n", row.render("")));
                printed += 1;
            }
            if rows.len() > printed {
                body.push_str(&format!(
                    "… {} more entries, past the {max_entries} limit\n",
                    rows.len() - printed
                ));
            }
        }

        if printed == 0 {
            return Ok(ToolExecResult::success(format!(
                "{} is empty",
                root.display()
            )));
        }
        if a.recursive && printed >= max_entries {
            body.push_str(&format!(
                "… stopped at the {max_entries} entry limit; raise max_entries, or list a \
                 subdirectory\n"
            ));
        }
        ctx.offload_if_large(body.trim_end(), Recovery::Narrow)
    }
}

/// One listed entry, already classified.
struct Row {
    name: String,
    /// `None` for anything that could not be stated — reported as unknown rather than as a file.
    kind: Option<Kind>,
    size: u64,
}

#[derive(PartialEq)]
enum Kind {
    Dir,
    File,
    Link,
    Other,
}

impl Row {
    fn is_dir(&self) -> bool {
        self.kind.as_ref() == Some(&Kind::Dir)
    }

    fn render(&self, connector: &str) -> String {
        match &self.kind {
            Some(Kind::Dir) => format!("{connector}{}/", self.name),
            Some(Kind::File) => format!("{connector}{} ({} bytes)", self.name, self.size),
            Some(Kind::Link) => format!("{connector}{}@", self.name),
            Some(Kind::Other) => format!("{connector}{}", self.name),
            None => format!("{connector}{} (unreadable)", self.name),
        }
    }
}

/// Direct children, directories first then alphabetical.
/// Directories first because it is the order every file manager uses, and because a model reading
/// a listing to decide where to look next wants the navigable entries together.
fn read_children(dir: &Path, ignores: &Ignores) -> std::io::Result<Vec<Row>> {
    let mut rows = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        // Hidden entries are listed — unlike glob and grep, which search. Someone asking what is
        // in a directory means `.env` and `.github` too; that is the difference between listing
        // and searching.
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if (is_dir && ignores.skips_dir(&name)) || (!is_dir && ignores.skips_file(&name)) {
            continue;
        }
        // `symlink_metadata` so a link is reported as a link rather than as whatever it points
        // at — and so a broken link is still listed instead of vanishing.
        let (kind, size) = match entry.metadata() {
            Ok(m) if m.is_dir() => (Some(Kind::Dir), 0),
            Ok(m) if m.is_symlink() => (Some(Kind::Link), 0),
            Ok(m) if m.is_file() => (Some(Kind::File), m.len()),
            Ok(_) => (Some(Kind::Other), 0),
            Err(_) => (None, 0),
        };
        rows.push(Row { name, kind, size });
    }
    rows.sort_by(|a, b| {
        b.is_dir()
            .cmp(&a.is_dir())
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(rows)
}

/// Renders the tree beneath `dir`, appending to `body`.
/// Stops the moment `printed` reaches `max_entries`; the caller reports the stop. Reading a
/// subdirectory that fails is skipped rather than aborting the whole tree — one unreadable
/// directory should not hide its siblings.
#[allow(clippy::too_many_arguments)]
fn tree(
    dir: &Path,
    ignores: &Ignores,
    max_depth: usize,
    max_entries: usize,
    depth: usize,
    prefix: &str,
    printed: &mut usize,
    body: &mut String,
) -> std::io::Result<()> {
    let rows = read_children(dir, ignores)?;
    let last_index = rows.len().saturating_sub(1);
    for (i, row) in rows.iter().enumerate() {
        if *printed >= max_entries {
            return Ok(());
        }
        let last = i == last_index;
        body.push_str(&row.render(if last { "└── " } else { "├── " }));
        body.push('\n');
        *printed += 1;

        if row.is_dir() && depth < max_depth {
            let child_prefix = format!("{prefix}{}", if last { "    " } else { "│   " });
            // The connector characters are written by the child call, so the prefix has to be
            // threaded rather than reconstructed.
            let mut nested = String::new();
            let _ = tree(
                &dir.join(&row.name),
                ignores,
                max_depth,
                max_entries,
                depth + 1,
                &child_prefix,
                printed,
                &mut nested,
            );
            for line in nested.lines() {
                body.push_str(&format!("{child_prefix}{line}\n"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ToolExecStatus, test_ctx};

    fn setup() -> (tempfile::TempDir, ToolCtx) {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("src/deep")).unwrap();
        std::fs::write(d.path().join("src/a.rs"), "aaaa").unwrap();
        std::fs::write(d.path().join("src/deep/b.rs"), "bb").unwrap();
        std::fs::write(d.path().join("README.md"), "hello").unwrap();
        let mut ctx = test_ctx(d.path());
        ctx.max_result_chars = 100_000;
        (d, ctx)
    }

    #[tokio::test]
    async fn lists_direct_children_with_sizes_directories_first() {
        let (_d, ctx) = setup();
        let out = ListDir.execute(&ctx, r#"{"path":"."}"#).await.unwrap();
        assert_eq!(out.status, ToolExecStatus::Success);

        let text = out.model_text();
        let lines: Vec<&str> = text.lines().skip(1).collect();
        assert_eq!(lines[0], "src/", "directories lead");
        assert_eq!(lines[1], "README.md (5 bytes)");
        assert_eq!(lines.len(), 2, "direct children only: {lines:?}");
    }

    #[tokio::test]
    async fn recursive_prints_a_tree() {
        let (_d, ctx) = setup();
        let out = ListDir
            .execute(&ctx, r#"{"path":".","recursive":true}"#)
            .await
            .unwrap();
        let text = out.model_text();
        assert!(
            text.contains("├── src/") || text.contains("└── src/"),
            "{text}"
        );
        assert!(text.contains("deep/"), "{text}");
        assert!(text.contains("b.rs"), "nested files appear: {text}");
    }

    #[tokio::test]
    async fn the_tree_stops_at_max_depth() {
        let (_d, ctx) = setup();
        let out = ListDir
            .execute(&ctx, r#"{"path":".","recursive":true,"max_depth":1}"#)
            .await
            .unwrap();
        let text = out.model_text();
        assert!(text.contains("src/"), "{text}");
        assert!(
            !text.contains("a.rs"),
            "depth 1 is the root's children only: {text}"
        );
    }

    /// A cut listing that does not say so makes the model conclude a file is absent.
    #[tokio::test]
    async fn a_truncated_flat_listing_says_how_much_was_left_out() {
        let (d, ctx) = setup();
        for i in 0..20 {
            std::fs::write(d.path().join(format!("f{i}")), "x").unwrap();
        }
        let out = ListDir
            .execute(&ctx, r#"{"path":".","max_entries":5}"#)
            .await
            .unwrap();
        assert!(
            out.model_text().contains("more entries"),
            "{}",
            out.model_text()
        );
    }

    #[tokio::test]
    async fn a_truncated_tree_says_it_stopped() {
        let (d, ctx) = setup();
        for i in 0..20 {
            std::fs::write(d.path().join(format!("f{i}")), "x").unwrap();
        }
        let out = ListDir
            .execute(&ctx, r#"{"path":".","recursive":true,"max_entries":4}"#)
            .await
            .unwrap();
        assert!(
            out.model_text().contains("entry limit"),
            "{}",
            out.model_text()
        );
    }

    /// Listing is not searching: someone asking what is in a directory means `.env` too.
    #[tokio::test]
    async fn hidden_entries_are_listed() {
        let (d, ctx) = setup();
        std::fs::write(d.path().join(".env"), "K=V").unwrap();
        let out = ListDir.execute(&ctx, r#"{"path":"."}"#).await.unwrap();
        assert!(out.model_text().contains(".env"), "{}", out.model_text());
    }

    /// …but the object database and dependency trees still are not.
    #[tokio::test]
    async fn dot_git_and_dependencies_are_left_out() {
        let (d, ctx) = setup();
        std::fs::create_dir(d.path().join(".git")).unwrap();
        std::fs::create_dir(d.path().join("node_modules")).unwrap();

        let out = ListDir.execute(&ctx, r#"{"path":"."}"#).await.unwrap();
        let text = out.model_text();
        assert!(!text.contains(".git"), "{text}");
        assert!(!text.contains("node_modules"), "{text}");
    }

    #[tokio::test]
    async fn opting_out_shows_the_dependencies_again() {
        let (d, ctx) = setup();
        std::fs::create_dir(d.path().join("node_modules")).unwrap();
        let out = ListDir
            .execute(&ctx, r#"{"path":".","include_ignored":true}"#)
            .await
            .unwrap();
        assert!(
            out.model_text().contains("node_modules"),
            "{}",
            out.model_text()
        );
    }

    #[tokio::test]
    async fn an_empty_directory_says_so() {
        let (d, ctx) = setup();
        std::fs::create_dir(d.path().join("empty")).unwrap();
        let out = ListDir.execute(&ctx, r#"{"path":"empty"}"#).await.unwrap();
        assert_eq!(out.status, ToolExecStatus::Success);
        assert!(out.model_text().contains("is empty"));
    }

    /// Missing and not-a-directory are different mistakes and get different messages.
    #[tokio::test]
    async fn a_missing_path_and_a_file_path_are_reported_differently() {
        let (_d, ctx) = setup();
        let missing = ListDir.execute(&ctx, r#"{"path":"nope"}"#).await.unwrap();
        assert_eq!(missing.status, ToolExecStatus::Failed);
        assert!(missing.model_text().contains("does not exist"));

        let file = ListDir
            .execute(&ctx, r#"{"path":"README.md"}"#)
            .await
            .unwrap();
        assert_eq!(file.status, ToolExecStatus::Failed);
        assert!(file.model_text().contains("not a directory"));
    }

    /// A broken symlink is still an entry, and is shown as a link rather than as a file.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_is_reported_as_a_link() {
        let (d, ctx) = setup();
        std::os::unix::fs::symlink(d.path().join("nowhere"), d.path().join("dangling")).unwrap();
        let out = ListDir.execute(&ctx, r#"{"path":"."}"#).await.unwrap();
        assert!(
            out.model_text().contains("dangling"),
            "{}",
            out.model_text()
        );
    }

    #[tokio::test]
    async fn a_missing_path_defaults_to_the_working_directory() {
        let (_d, ctx) = setup();
        let out = ListDir.execute(&ctx, "{}").await.unwrap();
        assert_eq!(out.status, ToolExecStatus::Success);
        assert!(out.model_text().contains("README.md"));
        assert!(ListDir.execute(&ctx, "not json").await.is_err());
    }
}
