//! `write_file`.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use zlogic_protocol::llm::ToolDefinition;

use crate::display::FileChange;
use crate::file::line_delta;
use crate::{Result, Tool, ToolCtx, ToolDisplay, ToolExecResult, ToolMeta, ToolRisk, parse_args};

#[derive(Debug, Deserialize)]
struct Args {
    path: String,
    content: String,
    /// Overwrites by default.
    #[serde(default)]
    append: bool,
}

pub struct WriteFile;

#[async_trait]
impl Tool for WriteFile {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "write_file".into(),
            source: "builtin",
            risk: ToolRisk::Write,
        }
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write_file".into(),
            description: "Write a file, overwriting by default. Parent directories are created."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "minLength": 1, "description": "Absolute, or relative to the working directory" },
                    "content": { "type": "string" },
                    "append": { "type": "boolean", "description": "Append instead of overwriting; default false" }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, ctx: &ToolCtx, args: &str) -> Result<ToolExecResult> {
        use std::io::Write;

        let a: Args = parse_args(args)?;
        let resolved = ctx.resolve_required_path("path", &a.path)?;

        if resolved.path.is_dir() {
            return Ok(ToolExecResult::failed(format!(
                "{} is a directory",
                resolved.path.display()
            )));
        }

        let existed = resolved.path.exists();
        let previous_bytes = if existed {
            match std::fs::read(&resolved.path) {
                Ok(bytes) => Some(bytes),
                Err(e) => {
                    return Ok(ToolExecResult::failed(format!(
                        "cannot read {} before writing: {e}",
                        resolved.path.display()
                    )));
                }
            }
        } else {
            None
        };
        if a.append
            && previous_bytes
                .as_ref()
                .is_some_and(|bytes| std::str::from_utf8(bytes).is_err())
        {
            return Ok(ToolExecResult::failed(format!(
                "{} is not UTF-8 text; write_file will not append text to binary content",
                resolved.path.display()
            )));
        }
        if !a.append
            && previous_bytes
                .as_deref()
                .is_some_and(|bytes| bytes == a.content.as_bytes())
        {
            return Ok(ToolExecResult::success(format!(
                "unchanged {} (content already matches)",
                resolved.path.display()
            ))
            .with_display(ToolDisplay::Diff {
                path: resolved.path.to_string_lossy().into_owned(),
                stat: crate::display::DiffStat {
                    added: 0,
                    removed: 0,
                },
                change: Some(FileChange::Modified),
                object_id: None,
            }));
        }
        let previous = previous_bytes
            .as_ref()
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .map(str::to_string);

        if let Some(parent) = resolved.path.parent()
            && !parent.as_os_str().is_empty()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            return Ok(ToolExecResult::failed(format!(
                "cannot create {}: {e}",
                parent.display()
            )));
        }

        let write = || -> std::io::Result<()> {
            if a.append {
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&resolved.path)?;
                f.write_all(a.content.as_bytes())?;
            } else {
                std::fs::write(&resolved.path, a.content.as_bytes())?;
            }
            Ok(())
        };
        if let Err(e) = write() {
            return Ok(ToolExecResult::failed(format!(
                "cannot write {}: {e}",
                resolved.path.display()
            )));
        }

        let change = if !existed {
            FileChange::Created
        } else {
            FileChange::Modified
        };
        let after = if a.append {
            format!("{}{}", previous.clone().unwrap_or_default(), a.content)
        } else {
            a.content.clone()
        };
        let stat = line_delta(previous.as_deref(), &after);

        let verb = match (existed, a.append) {
            (false, _) => "created",
            (true, true) => "appended to",
            (true, false) => "overwrote",
        };
        Ok(ToolExecResult::success(format!(
            "{verb} {} ({} bytes, +{} -{})",
            resolved.path.display(),
            a.content.len(),
            stat.added,
            stat.removed
        ))
        .with_display(ToolDisplay::Diff {
            path: resolved.path.to_string_lossy().into_owned(),
            // No unified diff yet: producing one needs a real diff implementation, and a
            // wrong-looking diff is worse than none. The line counts are exact.
            stat,
            change: Some(change),
            object_id: None,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::DiffStat;
    use crate::{ToolExecStatus, test_ctx};

    fn setup() -> (tempfile::TempDir, ToolCtx) {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        (dir, ctx)
    }

    #[tokio::test]
    async fn creates_a_file_and_reports_it_as_created() {
        let (d, ctx) = setup();
        let out = WriteFile
            .execute(&ctx, r#"{"path":"a.txt","content":"hi\n"}"#)
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Success);
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.txt")).unwrap(),
            "hi\n"
        );
        match &out.display[0] {
            ToolDisplay::Diff { change, stat, .. } => {
                assert_eq!(*change, Some(FileChange::Created));
                assert_eq!(
                    *stat,
                    DiffStat {
                        added: 1,
                        removed: 0
                    }
                );
            }
            other => panic!("expected a diff card, got {other:?}"),
        }
        // No object here — the diff is inline. Still checked, so adding one later cannot quietly
        // leave the card pointing at nothing.
        assert!(out.dangling_display_objects().is_empty());
    }

    #[tokio::test]
    async fn overwrites_by_default_and_reports_the_line_delta() {
        let (d, ctx) = setup();
        WriteFile
            .execute(&ctx, r#"{"path":"a.txt","content":"a\nb\nc\n"}"#)
            .await
            .unwrap();
        let out = WriteFile
            .execute(&ctx, r#"{"path":"a.txt","content":"a\nCHANGED\nc\n"}"#)
            .await
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(d.path().join("a.txt")).unwrap(),
            "a\nCHANGED\nc\n"
        );
        match &out.display[0] {
            ToolDisplay::Diff { change, stat, .. } => {
                assert_eq!(*change, Some(FileChange::Modified));
                // One line replaced: the unchanged prefix and suffix are excluded.
                assert_eq!(
                    *stat,
                    DiffStat {
                        added: 1,
                        removed: 1
                    }
                );
            }
            other => panic!("expected a diff card, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn append_adds_instead_of_replacing() {
        let (d, ctx) = setup();
        WriteFile
            .execute(&ctx, r#"{"path":"a.txt","content":"one\n"}"#)
            .await
            .unwrap();
        let out = WriteFile
            .execute(&ctx, r#"{"path":"a.txt","content":"two\n","append":true}"#)
            .await
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(d.path().join("a.txt")).unwrap(),
            "one\ntwo\n"
        );
        match &out.display[0] {
            ToolDisplay::Diff { stat, .. } => {
                assert_eq!(
                    *stat,
                    DiffStat {
                        added: 1,
                        removed: 0
                    },
                    "append only adds"
                );
            }
            other => panic!("expected a diff card, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn identical_overwrite_is_a_no_op() {
        let (d, ctx) = setup();
        std::fs::write(d.path().join("a.txt"), "same\n").unwrap();
        let before = std::fs::metadata(d.path().join("a.txt"))
            .unwrap()
            .modified()
            .unwrap();

        let out = WriteFile
            .execute(&ctx, r#"{"path":"a.txt","content":"same\n"}"#)
            .await
            .unwrap();

        assert_eq!(out.status, ToolExecStatus::Success);
        assert!(out.model_text().contains("unchanged"));
        assert_eq!(
            std::fs::metadata(d.path().join("a.txt"))
                .unwrap()
                .modified()
                .unwrap(),
            before
        );
    }

    #[tokio::test]
    async fn append_to_binary_content_is_refused() {
        let (d, ctx) = setup();
        std::fs::write(d.path().join("a.bin"), [0xff, 0x00]).unwrap();
        let out = WriteFile
            .execute(&ctx, r#"{"path":"a.bin","content":"text","append":true}"#)
            .await
            .unwrap();

        assert_eq!(out.status, ToolExecStatus::Failed);
        assert_eq!(std::fs::read(d.path().join("a.bin")).unwrap(), [0xff, 0x00]);
    }

    #[tokio::test]
    async fn creates_missing_parent_directories() {
        let (d, ctx) = setup();
        WriteFile
            .execute(&ctx, r#"{"path":"deep/nested/a.txt","content":"x"}"#)
            .await
            .unwrap();
        assert!(d.path().join("deep/nested/a.txt").exists());
    }

    #[tokio::test]
    async fn refuses_to_write_over_a_directory() {
        let (d, ctx) = setup();
        std::fs::create_dir(d.path().join("adir")).unwrap();
        let out = WriteFile
            .execute(&ctx, r#"{"path":"adir","content":"x"}"#)
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Failed);
    }

    #[tokio::test]
    async fn missing_required_args_are_rejected() {
        let (_d, ctx) = setup();
        assert!(
            WriteFile
                .execute(&ctx, r#"{"path":"a.txt"}"#)
                .await
                .is_err()
        );
        assert!(WriteFile.execute(&ctx, r#"{"content":"x"}"#).await.is_err());
    }

    /// The counts are exact but the diff itself is absent.
    /// A whole-file rewrite's unified diff *is* the whole file, twice — it would cost the UI a
    /// large payload to say what the line counts already say. `edit`, whose change is by
    /// construction small and local, does carry one.
    #[tokio::test]
    async fn no_unified_diff_is_claimed() {
        let (_d, ctx) = setup();
        let out = WriteFile
            .execute(&ctx, r#"{"path":"a.txt","content":"x"}"#)
            .await
            .unwrap();
        match &out.display[0] {
            ToolDisplay::Diff {
                object_id, stat, ..
            } => {
                assert!(object_id.is_none(), "no diff object for a whole rewrite");
                assert_eq!(stat.added, 1, "but the counts are exact");
            }
            other => panic!("expected a diff card, got {other:?}"),
        }
    }
}
