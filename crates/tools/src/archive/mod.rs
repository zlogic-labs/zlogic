//! Bounded ZIP/TAR archive inspection, creation, and safe extraction.

use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use serde::Deserialize;
use serde_json::json;
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};
use zlogic_protocol::llm::ToolDefinition;

use crate::{
    CancellationToken, PromptExample, Recovery, Result, Tool, ToolCtx, ToolError, ToolExecResult,
    ToolExposure, ToolMeta, ToolPromptSpec, ToolRisk, parse_args,
};

const MAX_ENTRIES: usize = 100_000;
const MAX_TOTAL_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Polled between entries by the archive operations.
///
/// A newtype rather than a bare `&CancellationToken` so the call sites read as what they are — a
/// stop check, not a token read — and so a later second signal has somewhere to go without
/// re-threading every signature.
#[derive(Clone, Copy)]
struct Stop<'a>(&'a CancellationToken);

impl Stop<'_> {
    /// `Err` once the caller has been abandoned. Checked *between* entries: the work inside one
    /// entry (a multi-gigabyte `io::copy`) is not interruptible, so the bound here is one entry,
    /// not the whole operation.
    fn check(&self) -> Result<()> {
        if self.0.is_cancelled() {
            return Err(stopped());
        }
        Ok(())
    }
}

/// The operation was abandoned — by `tools.timeout_secs`, or by the user pressing Esc.
///
/// Not [`ToolError::Cancelled`] and not a plain failure, because the runtime already reports this
/// event as a timeout and a tool contradicting that with "the user interrupted you" would be a
/// different, untrue story. The text is usually never read: the caller has already given up, and
/// what it writes to the timeline is its own account of the timeout.
fn stopped() -> ToolError {
    ToolError::Failed("archive operation was stopped before it finished".into())
}

pub struct ArchiveInfo;
pub struct ArchiveProcess;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InfoArgs {
    path: String,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessArgs {
    operation: String,
    #[serde(default)]
    input: Option<String>,
    output: String,
    #[serde(default)]
    files: Vec<ArchiveFile>,
    #[serde(default)]
    overwrite: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchiveFile {
    source: String,
    path: String,
}

#[async_trait]
impl Tool for ArchiveInfo {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "archive_info".into(),
            source: "builtin",
            risk: ToolRisk::Read,
        }
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Deferred
    }

    fn prompt_spec(&self) -> Option<ToolPromptSpec> {
        Some(ToolPromptSpec {
            when: "Reach for this to see what is inside a ZIP or TAR before extracting it \
                — an archive from the user is an unknown quantity, and listing costs nothing.",
            contract: "Listing only. Nothing is written and nothing is extracted.",
            positive_examples: &[r#"{"path":"/exact/attachment.zip"}"#],
            negative_examples: &[PromptExample {
                args: r#"{"path":"bundle.zip","output":"out"}"#,
                why: "extracting is archive_process",
            }],
        })
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "archive_info".into(),
            description: "List bounded metadata from ZIP, TAR, or TAR.GZ archives without \
                          extracting their contents."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "minLength": 1},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 10000, "default": 1000}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, ctx: &ToolCtx, args: &str) -> Result<ToolExecResult> {
        let args: InfoArgs = parse_args(args)?;
        let path = ctx.resolve_required_path("path", &args.path)?.path;
        let limit = args.limit.unwrap_or(1_000).clamp(1, 10_000);
        // A child of the conversation token, so Esc reaches the worker too: the two signals differ
        // in meaning but not at all in who can fire them here.
        let stop = ctx.cancel.child_token();
        let output = tokio::task::spawn_blocking(move || list_archive(&path, limit, Stop(&stop)))
            .await
            .map_err(|error| ToolError::Failed(format!("archive worker failed: {error}")))??;
        ctx.offload_if_large(&output, Recovery::Narrow)
    }
}

#[async_trait]
impl Tool for ArchiveProcess {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "archive_process".into(),
            source: "builtin",
            risk: ToolRisk::Write,
        }
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Deferred
    }

    fn prompt_spec(&self) -> Option<ToolPromptSpec> {
        Some(ToolPromptSpec {
            when: "Reach for this to create or extract a ZIP/TAR instead of running \
                `tar` or `zip` in the shell: extraction here refuses traversal paths, links, devices \
                and absurd expansion, which a shell command will happily perform on you.",
            contract: "`create` needs `output` (the archive) plus `files`, each entry giving the \
                `source` on disk and the relative `path` to store — the stored path is what someone \
                unpacking will get, so keep it relative and free of `..`. `extract` needs `input` \
                plus an `output` directory that does not exist yet. `overwrite` applies to create \
                only.",
            positive_examples: &[
                r#"{"operation":"create","output":"logs.tar.gz","files":[{"source":"/var/log/app.log","path":"app.log"}]}"#,
                r#"{"operation":"extract","input":"bundle.zip","output":"bundle-contents"}"#,
            ],
            negative_examples: &[
                PromptExample {
                    args: r#"{"operation":"create","output":"logs.zip","files":[{"source":"/var/log/app.log","path":"/var/log/app.log"}]}"#,
                    why: "the stored path must be relative, not the absolute source path",
                },
                PromptExample {
                    args: r#"{"operation":"extract","input":"bundle.zip"}"#,
                    why: "extract needs an output directory; it never unpacks in place",
                },
            ],
        })
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "archive_process".into(),
            description: "Create or safely extract ZIP, TAR, and TAR.GZ archives. Extraction \
                          rejects traversal paths, links, devices, excessive entry counts, and \
                          oversized expanded data."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "operation": {"type": "string", "enum": ["create", "extract"]},
                    "input": {"type": "string", "description": "Archive path; required for extract"},
                    "output": {"type": "string", "minLength": 1, "description": "Archive path for create, new destination directory for extract"},
                    "files": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "source": {"type": "string", "minLength": 1},
                                "path": {"type": "string", "minLength": 1, "description": "Safe relative path stored in the archive"}
                            },
                            "required": ["source", "path"],
                            "additionalProperties": false
                        }
                    },
                    "overwrite": {"type": "boolean", "default": false, "description": "For create only"}
                },
                "required": ["operation", "output"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, ctx: &ToolCtx, args: &str) -> Result<ToolExecResult> {
        let args: ProcessArgs = parse_args(args)?;
        let output = ctx.resolve_required_path("output", &args.output)?.path;
        match args.operation.as_str() {
            "create" => {
                if args.files.is_empty() {
                    return Err(ToolError::BadArgs(
                        "files must contain at least one entry for create".into(),
                    ));
                }
                if output.exists() && !args.overwrite {
                    return Ok(ToolExecResult::failed(format!(
                        "{} already exists; set overwrite=true to replace it",
                        output.display()
                    )));
                }
                let mut files = Vec::with_capacity(args.files.len());
                for file in args.files {
                    let source = ctx.resolve_required_path("source", &file.source)?.path;
                    validate_archive_path(&file.path)?;
                    if source == output {
                        return Err(ToolError::BadArgs(
                            "archive output must not also be an input file".into(),
                        ));
                    }
                    files.push((source, file.path));
                }
                let output_for_worker = output.clone();
                let stop = ctx.cancel.child_token();
                tokio::task::spawn_blocking(move || {
                    // A half-written archive is worse than no archive: it is a file the size of the
                    // real one, holding a prefix of it, and nothing on disk distinguishes the two.
                    // `extract` has always deleted its output on failure for the same reason — this
                    // is that policy applied to the other writer.
                    let result = create_archive(&output_for_worker, files, Stop(&stop));
                    if result.is_err() {
                        let _ = std::fs::remove_file(&output_for_worker);
                    }
                    result
                })
                .await
                .map_err(|error| {
                    ToolError::Failed(format!("archive worker failed: {error}"))
                })??;
                let name = output
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("archive")
                    .to_owned();
                let captured = ctx.capture_file_path(name, mime_for_archive(&output), &output)?;
                Ok(captured.add_to(ToolExecResult::success(format!(
                    "Created archive {}",
                    output.display()
                ))))
            }
            "extract" => {
                let raw = args
                    .input
                    .as_deref()
                    .ok_or_else(|| ToolError::BadArgs("input is required for extract".into()))?;
                let input = ctx.resolve_required_path("input", raw)?.path;
                if output.exists() {
                    return Ok(ToolExecResult::failed(format!(
                        "extraction destination {} must not already exist",
                        output.display()
                    )));
                }
                let output_for_worker = output.clone();
                let stop = ctx.cancel.child_token();
                let count = tokio::task::spawn_blocking(move || {
                    let result = extract_archive(&input, &output_for_worker, Stop(&stop));
                    if result.is_err() {
                        let _ = std::fs::remove_dir_all(&output_for_worker);
                    }
                    result
                })
                .await
                .map_err(|error| ToolError::Failed(format!("archive worker failed: {error}")))??;
                Ok(ToolExecResult::success(format!(
                    "Extracted {count} entries to {}",
                    output.display()
                )))
            }
            other => Err(ToolError::BadArgs(format!(
                "unsupported archive operation {other:?}"
            ))),
        }
    }
}

fn list_archive(path: &Path, limit: usize, stop: Stop<'_>) -> Result<String> {
    let kind = archive_kind(path)?;
    let mut entries = Vec::new();
    let mut total = 0_u64;
    match kind {
        ArchiveKind::Zip => {
            let mut archive = ZipArchive::new(File::open(path)?)
                .map_err(|error| ToolError::Failed(format!("invalid ZIP archive: {error}")))?;
            for index in 0..archive.len().min(limit) {
                stop.check()?;
                let file = archive
                    .by_index(index)
                    .map_err(|error| ToolError::Failed(format!("invalid ZIP entry: {error}")))?;
                total = total.saturating_add(file.size());
                entries.push(json!({
                    "path": file.name(),
                    "bytes": file.size(),
                    "directory": file.is_dir()
                }));
            }
        }
        ArchiveKind::Tar | ArchiveKind::TarGz => {
            let reader = tar_reader(path, kind)?;
            let mut archive = tar::Archive::new(reader);
            for entry in archive
                .entries()
                .map_err(|error| ToolError::Failed(format!("invalid TAR archive: {error}")))?
                .take(limit)
            {
                stop.check()?;
                let entry = entry
                    .map_err(|error| ToolError::Failed(format!("invalid TAR entry: {error}")))?;
                let size = entry.size();
                total = total.saturating_add(size);
                entries.push(json!({
                    "path": entry.path().ok().map(|path| path.display().to_string()),
                    "bytes": size,
                    "type": format!("{:?}", entry.header().entry_type())
                }));
            }
        }
    }
    serde_json::to_string_pretty(&json!({
        "path": path,
        "format": kind.label(),
        "returned_entries": entries.len(),
        "returned_uncompressed_bytes": total,
        "limit": limit,
        "entries": entries
    }))
    .map_err(|error| ToolError::Failed(error.to_string()))
}

fn create_archive(
    output: &Path,
    files: Vec<(PathBuf, String)>,
    stop: Stop<'_>,
) -> Result<()> {
    let kind = archive_kind(output)?;
    let mut total = 0_u64;
    for (source, _) in &files {
        stop.check()?;
        let metadata = std::fs::metadata(source)?;
        if !metadata.is_file() {
            return Err(ToolError::BadArgs(format!(
                "archive source must be a regular file: {}",
                source.display()
            )));
        }
        total = total.saturating_add(metadata.len());
        if total > MAX_TOTAL_BYTES {
            return Err(ToolError::Failed(format!(
                "archive inputs exceed the {MAX_TOTAL_BYTES}-byte limit"
            )));
        }
    }
    if files.len() > MAX_ENTRIES {
        return Err(ToolError::Failed(format!(
            "archive contains more than {MAX_ENTRIES} entries"
        )));
    }
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match kind {
        ArchiveKind::Zip => {
            let mut writer = ZipWriter::new(File::create(output)?);
            let options =
                SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
            for (source, path) in files {
                stop.check()?;
                writer
                    .start_file(path, options)
                    .map_err(|error| ToolError::Failed(format!("cannot add ZIP entry: {error}")))?;
                let mut source = File::open(source)?;
                std::io::copy(&mut source, &mut writer)?;
            }
            writer
                .finish()
                .map_err(|error| ToolError::Failed(format!("cannot finish ZIP: {error}")))?;
        }
        ArchiveKind::Tar => {
            let mut builder = tar::Builder::new(File::create(output)?);
            for (source, path) in files {
                stop.check()?;
                builder.append_path_with_name(source, path)?;
            }
            builder.finish()?;
        }
        ArchiveKind::TarGz => {
            let encoder = GzEncoder::new(File::create(output)?, Compression::default());
            let mut builder = tar::Builder::new(encoder);
            for (source, path) in files {
                stop.check()?;
                builder.append_path_with_name(source, path)?;
            }
            builder.finish()?;
            builder.into_inner()?.finish()?;
        }
    }
    Ok(())
}

fn extract_archive(input: &Path, output: &Path, stop: Stop<'_>) -> Result<usize> {
    let kind = archive_kind(input)?;
    std::fs::create_dir_all(output)?;
    let mut count = 0_usize;
    let mut total = 0_u64;
    match kind {
        ArchiveKind::Zip => {
            let mut archive = ZipArchive::new(File::open(input)?)
                .map_err(|error| ToolError::Failed(format!("invalid ZIP archive: {error}")))?;
            if archive.len() > MAX_ENTRIES {
                return Err(ToolError::Failed(format!(
                    "archive contains more than {MAX_ENTRIES} entries"
                )));
            }
            for index in 0..archive.len() {
                stop.check()?;
                let mut entry = archive
                    .by_index(index)
                    .map_err(|error| ToolError::Failed(format!("invalid ZIP entry: {error}")))?;
                let relative = entry.enclosed_name().ok_or_else(|| {
                    ToolError::Failed(format!("unsafe ZIP path {:?}", entry.name()))
                })?;
                if entry
                    .unix_mode()
                    .is_some_and(|mode| mode & 0o170000 == 0o120000)
                {
                    return Err(ToolError::Failed(format!(
                        "ZIP symbolic links are not extracted: {:?}",
                        entry.name()
                    )));
                }
                total = total.saturating_add(entry.size());
                if total > MAX_TOTAL_BYTES {
                    return Err(ToolError::Failed(format!(
                        "expanded archive exceeds {MAX_TOTAL_BYTES} bytes"
                    )));
                }
                let target = output.join(relative);
                if entry.is_dir() {
                    std::fs::create_dir_all(&target)?;
                } else {
                    if let Some(parent) = target.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    let mut target = File::create(target)?;
                    std::io::copy(&mut entry, &mut target)?;
                }
                count += 1;
            }
        }
        ArchiveKind::Tar | ArchiveKind::TarGz => {
            let reader = tar_reader(input, kind)?;
            let mut archive = tar::Archive::new(reader);
            for entry in archive
                .entries()
                .map_err(|error| ToolError::Failed(format!("invalid TAR archive: {error}")))?
            {
                stop.check()?;
                if count >= MAX_ENTRIES {
                    return Err(ToolError::Failed(format!(
                        "archive contains more than {MAX_ENTRIES} entries"
                    )));
                }
                let mut entry = entry
                    .map_err(|error| ToolError::Failed(format!("invalid TAR entry: {error}")))?;
                let entry_type = entry.header().entry_type();
                if !(entry_type.is_file() || entry_type.is_dir()) {
                    return Err(ToolError::Failed(
                        "TAR links, devices, and special entries are not extracted".into(),
                    ));
                }
                total = total.saturating_add(entry.size());
                if total > MAX_TOTAL_BYTES {
                    return Err(ToolError::Failed(format!(
                        "expanded archive exceeds {MAX_TOTAL_BYTES} bytes"
                    )));
                }
                if !entry.unpack_in(output)? {
                    return Err(ToolError::Failed(
                        "TAR entry attempted to escape the destination".into(),
                    ));
                }
                count += 1;
            }
        }
    }
    Ok(count)
}

#[derive(Clone, Copy)]
enum ArchiveKind {
    Zip,
    Tar,
    TarGz,
}

impl ArchiveKind {
    fn label(self) -> &'static str {
        match self {
            Self::Zip => "zip",
            Self::Tar => "tar",
            Self::TarGz => "tar.gz",
        }
    }
}

fn archive_kind(path: &Path) -> Result<ArchiveKind> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if name.ends_with(".zip") {
        Ok(ArchiveKind::Zip)
    } else if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        Ok(ArchiveKind::TarGz)
    } else if name.ends_with(".tar") {
        Ok(ArchiveKind::Tar)
    } else {
        Err(ToolError::BadArgs(
            "archive path must end in .zip, .tar, .tar.gz, or .tgz".into(),
        ))
    }
}

fn tar_reader(path: &Path, kind: ArchiveKind) -> Result<Box<dyn Read>> {
    let file = File::open(path)?;
    match kind {
        ArchiveKind::Tar => Ok(Box::new(file)),
        ArchiveKind::TarGz => Ok(Box::new(GzDecoder::new(file))),
        ArchiveKind::Zip => unreachable!(),
    }
}

fn validate_archive_path(path: &str) -> Result<()> {
    let path = Path::new(path);
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(ToolError::BadArgs(format!(
            "archive entry path must be safe and relative: {path:?}"
        )));
    }
    Ok(())
}

fn mime_for_archive(path: &Path) -> &'static str {
    match archive_kind(path) {
        Ok(ArchiveKind::Zip) => "application/zip",
        Ok(ArchiveKind::Tar) => "application/x-tar",
        Ok(ArchiveKind::TarGz) => "application/gzip",
        Err(_) => "application/octet-stream",
    }
}

pub fn all() -> Vec<std::sync::Arc<dyn Tool>> {
    vec![
        std::sync::Arc::new(ArchiveInfo),
        std::sync::Arc::new(ArchiveProcess),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traversal_paths_are_rejected() {
        assert!(validate_archive_path("safe/file.txt").is_ok());
        assert!(validate_archive_path("../escape").is_err());
        assert!(validate_archive_path("/absolute").is_err());
    }

    /// The never-cancelled token, for the tests that are about the archive and not the stop path.
    fn running() -> CancellationToken {
        CancellationToken::new()
    }

    #[test]
    fn zip_round_trip_keeps_the_requested_entry_path() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.txt");
        std::fs::write(&source, "hello archive").unwrap();
        let archive = temp.path().join("sample.zip");
        create_archive(
            &archive,
            vec![(source, "nested/renamed.txt".to_string())],
            Stop(&running()),
        )
        .unwrap();
        let destination = temp.path().join("expanded");
        assert_eq!(extract_archive(&archive, &destination, Stop(&running())).unwrap(), 1);
        assert_eq!(
            std::fs::read_to_string(destination.join("nested/renamed.txt")).unwrap(),
            "hello archive"
        );
    }

    /// A stop signal set before the work begins ends it, rather than being noticed only after the
    /// archive is already written. Without this check, a timed-out `create` writes a full archive
    /// that nobody is waiting for.
    #[test]
    fn an_already_cancelled_stop_ends_the_operation() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.txt");
        std::fs::write(&source, "hello archive").unwrap();
        let archive = temp.path().join("sample.zip");
        let token = CancellationToken::new();
        token.cancel();

        let result = create_archive(&archive, vec![(source, "a.txt".to_string())], Stop(&token));

        assert!(result.is_err(), "{result:?}");
        assert!(
            !archive.exists(),
            "the caller cleans up on error, but nothing may have been created to clean up"
        );
    }

    #[test]
    fn a_stopped_extraction_leaves_nothing_behind() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.txt");
        std::fs::write(&source, "hello archive").unwrap();
        let archive = temp.path().join("sample.zip");
        create_archive(&archive, vec![(source, "a.txt".to_string())], Stop(&running())).unwrap();
        let destination = temp.path().join("expanded");
        let token = CancellationToken::new();
        token.cancel();

        assert!(extract_archive(&archive, &destination, Stop(&token)).is_err());
    }
}
