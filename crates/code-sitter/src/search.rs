//! Stage 1: ripgrep-style search using the grep crate family + ignore walk.
//! Mirrors the backend the TS grep tool's Rust-port note prescribes
//! (grep-regex + grep-searcher + ignore::WalkBuilder), so this doubles as a
//! reference for the eventual port.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{
    BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkContextKind, SinkMatch,
};
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;

use crate::Options;

#[derive(Debug)]
pub enum SearchError {
    BadRegex(String),
    Walk(String),
    Cancelled,
}

impl std::fmt::Display for SearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SearchError::BadRegex(m) => write!(f, "invalid regex: {m}"),
            SearchError::Walk(m) => write!(f, "walk failed: {m}"),
            SearchError::Cancelled => write!(f, "search cancelled"),
        }
    }
}
impl std::error::Error for SearchError {}

#[derive(Debug, Clone)]
pub struct RawHit {
    pub path: PathBuf,
    /// 1-based line number.
    pub line: u64,
    /// 0-based BYTE column of the match start within the line (tree-sitter
    /// `Point.column` is a byte offset). Needed to land on the right node when
    /// the hit shares a line with an outer construct (e.g. an arrow-fn decl).
    pub col: usize,
    pub text: String,
    /// Context lines before/after the match (line number, text). Populated
    /// only when `Options.context_lines > 0`.
    pub before: Vec<(u64, String)>,
    pub after: Vec<(u64, String)>,
}

/// Raw search output plus honest bookkeeping for the caller.
pub struct RunOutput {
    pub hits: Vec<RawHit>,
    /// Files that errored mid-read and were skipped (potential false negatives).
    pub skipped_files: usize,
    /// True when `max_total` cut the results short.
    pub truncated: bool,
}

fn line_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .trim_end_matches(['\n', '\r'])
        .to_string()
}

/// Custom sink: collects match lines (with byte column) AND context lines.
/// The convenience `sinks::UTF8` drops context, so we implement `Sink` directly.
struct HitSink<'a> {
    matcher: &'a RegexMatcher,
    path: &'a Path,
    /// Per-file hit budget; `matched` returns false once reached.
    budget: usize,
    hits: Vec<RawHit>,
    /// Before-context lines seen since the last match — they belong to the
    /// NEXT match in this file.
    pending_before: Vec<(u64, String)>,
    cancel: Option<&'a AtomicBool>,
}

impl Sink for HitSink<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        if self
            .cancel
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(false)
        {
            return Ok(false);
        }
        // Recover the match's byte column within the line so the tree-sitter
        // lookup lands on the matched token, not column 0.
        let col = self
            .matcher
            .find(mat.bytes())
            .ok()
            .flatten()
            .map(|m| m.start())
            .unwrap_or(0);
        self.hits.push(RawHit {
            path: self.path.to_path_buf(),
            line: mat.line_number().unwrap_or(0),
            col,
            text: line_text(mat.bytes()),
            before: std::mem::take(&mut self.pending_before),
            after: Vec::new(),
        });
        Ok(self.hits.len() < self.budget)
    }

    fn context(
        &mut self,
        _searcher: &Searcher,
        ctx: &SinkContext<'_>,
    ) -> Result<bool, Self::Error> {
        let entry = (ctx.line_number().unwrap_or(0), line_text(ctx.bytes()));
        match ctx.kind() {
            SinkContextKind::Before => self.pending_before.push(entry),
            SinkContextKind::After => {
                if let Some(last) = self.hits.last_mut() {
                    last.after.push(entry);
                }
            }
            _ => {}
        }
        Ok(true)
    }
}

pub fn run(root: &Path, pattern: &str, opts: &Options) -> Result<RunOutput, SearchError> {
    // Fail loudly if the root itself is missing/unreadable, instead of walking
    // an empty iterator and reporting "no matches" (a false negative).
    std::fs::metadata(root).map_err(|e| SearchError::Walk(format!("{}: {e}", root.display())))?;

    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(!opts.case_sensitive)
        .build(pattern)
        .map_err(|e| SearchError::BadRegex(e.to_string()))?;

    let mut walk = WalkBuilder::new(root);
    walk.hidden(!opts.include_hidden) // default: skip dotfiles (ripgrep-compatible)
        .git_ignore(!opts.include_ignored)
        .git_global(!opts.include_ignored)
        .git_exclude(!opts.include_ignored)
        .parents(!opts.include_ignored);
    // Apply adapter-provided exclusions in the walk, before hits consume `max_total`. Filtering
    // results afterwards can turn "the first N hits were ignored" into a false "no matches".
    let exclude_dirs = opts.exclude_dirs.clone();
    let exclude_files = opts.exclude_files.clone();
    walk.filter_entry(move |entry| {
        let name = entry.file_name().to_string_lossy();
        if entry.file_type().is_some_and(|ty| ty.is_dir()) {
            name != ".git" && !exclude_dirs.iter().any(|excluded| excluded == &name)
        } else {
            !exclude_files.iter().any(|excluded| excluded == &name)
        }
    });

    if let Some(inc) = &opts.include {
        let mut ob = OverrideBuilder::new(root);
        // gitignore glob semantics: a bare "*.ts" matches by basename anywhere.
        ob.add(inc).map_err(|e| SearchError::Walk(e.to_string()))?;
        let ov = ob.build().map_err(|e| SearchError::Walk(e.to_string()))?;
        walk.overrides(ov);
    }

    let context = if opts.files_only {
        0
    } else {
        opts.context_lines
    };
    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        // Treat NUL as binary and bail — otherwise .png/.wasm/build output
        // produce garbage hits (ripgrep's own default behavior).
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .before_context(context)
        .after_context(context)
        .build();

    let cancel = opts.cancel.as_deref();
    let mut out: Vec<RawHit> = Vec::new();
    let mut skipped = 0usize;
    let mut truncated = false;

    for dent in walk.build() {
        if cancel.map(|c| c.load(Ordering::Relaxed)).unwrap_or(false) {
            return Err(SearchError::Cancelled);
        }
        let dent = match dent {
            Ok(d) => d,
            Err(_) => continue, // skip unreadable entries, keep going
        };
        if !dent.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = dent.path();

        // files_only: one hit per file is enough (its first match).
        let budget = if opts.files_only {
            1
        } else {
            opts.max_total - out.len()
        };
        let mut sink = HitSink {
            matcher: &matcher,
            path,
            budget,
            hits: Vec::new(),
            pending_before: Vec::new(),
            cancel,
        };
        // A per-file failure (IO, mid-read) is non-fatal — like ripgrep, skip
        // the file and keep going — but COUNT it so it isn't a silent false
        // negative (surfaced via RunOutput.skipped_files).
        if searcher.search_path(&matcher, path, &mut sink).is_err() {
            skipped += 1;
        }
        out.append(&mut sink.hits);
        if out.len() >= opts.max_total {
            truncated = true;
            break; // explicit early-exit: don't pull one more walker entry
        }
    }

    if out.len() > opts.max_total {
        out.truncate(opts.max_total);
    }
    Ok(RunOutput {
        hits: out,
        skipped_files: skipped,
        truncated,
    })
}
