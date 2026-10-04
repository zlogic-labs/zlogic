//! What a turn produced, found by looking at the filesystem once the turn is over.
//!
//! # This is the last resort, and only for shell
//!
//! Everything a turn produces is already in the entries, except what a **shell command** left
//! behind. `write_file` and `edit` write a diff into the timeline; an image or document generator
//! writes a file display; both are facts, parsed where they are rendered, and a walk of the
//! filesystem would only add a second, vaguer answer to them. A shell command is the one tool whose
//! output the entries cannot account for: `python render.py --name output/icon` writes
//! `output/icon-1024.png`, a path that appears in no tool call, no redirect and no file card — and
//! a file watcher is no help either, because it learns a path and a time and never which turn wrote
//! it. So the turn records that it ran a shell command, and only then is this called.
//!
//! Nothing here asks the model to name its output, and nothing parses what the model said: a turn
//! that writes four PNGs and answers "done" reports the same four files as one that itemises them.
//!
//! # The two signals
//!
//! **Where**: a file inside the turn's own cache folder is ours by convention. **When**: a file in
//! the workspace whose mtime falls inside the turn's window was written by this turn, almost always
//! — the window is the length of one turn.
//!
//! Both are needed because each is wrong alone. "New file, never mind where" would hand the user a
//! build artefact from a watcher that fired mid-turn; "in our folder" only works for a script that
//! was told to put it there.
//!
//! # Why extensions at all
//!
//! A turn that rewrites a source file has a diff card for it, and one that rewrites a hundred has a
//! hundred. Restricting this scan to things a person is *given* rather than things a person *edits*
//! keeps the two sources from describing the same event, and keeps a turn that runs a build from
//! reporting three thousand object files.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::SystemTime;

use zlogic_protocol::stream::TurnDeliverable;

/// Everything a turn said and named, kept as one searchable blob.
///
/// This is the answer to "whose file is this" for a workspace several sessions share. A timestamp
/// cannot answer it — two sessions running at once are inside each other's windows — but a command
/// line can: `--name out/icon` is how `out/icon-1024.png` came to exist, and the file's name minus
/// its size suffix is in that command. So a candidate this turn's own text mentions is certainly
/// its own, and one a *concurrent* turn's text mentions is certainly that turn's.
///
/// It is a blob rather than a set of paths because the mention is usually not a path at all: the
/// whole point is the file whose name the model assembled at runtime. Matching on the name is what
/// catches it, and a name is cheap to look for in the text that has to be searched anyway.
#[derive(Default)]
pub struct Footprint {
    text: std::sync::Mutex<String>,
    paths: std::sync::Mutex<Vec<String>>,
}

impl Footprint {
    /// Anything this turn wrote down: a command line, a reply, a file card's path.
    pub fn note(&self, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        let mut blob = self.text.lock().unwrap_or_else(|error| error.into_inner());
        // A turn's own text runs to tens of kilobytes at most; the cap is here so that a tool
        // echoing a large file back cannot turn the search below into a scan of megabytes.
        if blob.len() > 512 * 1024 {
            return;
        }
        blob.push_str(&text.replace('\\', "/").to_lowercase());
        blob.push('\n');
    }

    /// Whether this text has named the file at `path`, in any of the forms a path can take.
    pub fn names(&self, path: &Path) -> bool {
        let hay = self.text.lock().unwrap_or_else(|error| error.into_inner());
        if hay.is_empty() {
            return false;
        }
        names_in(&hay, path)
    }

    /// Whether the footprint has anything to search at all — worth asking before building the
    /// foreign one, which is the expensive half.
    pub fn is_empty(&self) -> bool {
        self.text.lock().unwrap_or_else(|error| error.into_inner()).is_empty()
    }

    /// A file this turn's own entries named outright — a diff or a file card's path.
    ///
    /// Kept apart from the text because these are the ones whose existence can be checked, and a
    /// file the turn wrote and then deleted has to be recognisable as gone rather than offered to
    /// the user as something they cannot open.
    pub fn note_path(&self, path: &Path) {
        if path.as_os_str().is_empty() {
            return;
        }
        let key = path.to_string_lossy().replace('\\', "/");
        let mut paths = self.paths.lock().unwrap_or_else(|error| error.into_inner());
        if paths.len() >= MAX_NOTED_PATHS || paths.iter().any(|seen| seen == &key) {
            return;
        }
        paths.push(key);
    }

    /// Every path this turn named outright, in the order it was first named.
    pub fn paths(&self) -> Vec<String> {
        self.paths.lock().unwrap_or_else(|error| error.into_inner()).clone()
    }
}

/// A turn that touches more files than this is editing a repository, not producing a handful of
/// outputs; the existence check is one `stat` each and stops here rather than growing with the turn.
const MAX_NOTED_PATHS: usize = 256;

/// The forms of a path worth looking for in a turn's text, loosest last.
///
/// The path itself, then the file name, then the stem, then the stem without a trailing size or
/// index suffix — `icon-1024` and `icon` for `icon-1024.png`, which is how a renderer that takes
/// `--name out/icon` names its output. A suffix is only stripped when there is a real stem left,
/// and the last form is dropped below three characters so a two-letter name cannot match half the
/// text.
fn names_in(hay: &str, path: &Path) -> bool {
    let full = path.to_string_lossy().replace('\\', "/").to_lowercase();
    if hay.contains(&full) {
        return true;
    }
    let Some(name) = path.file_name().map(|name| name.to_string_lossy().to_lowercase()) else {
        return false;
    };
    if hay.contains(&name) {
        return true;
    }
    let stem = name.rsplit_once('.').map_or(name.clone(), |(stem, _)| stem.to_owned());
    if hay.contains(&stem) {
        return true;
    }
    let base = match stem.rsplit_once(['-', '_']) {
        Some((base, tail)) if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) => base,
        _ => stem.as_str(),
    };
    base.len() >= 3 && hay.contains(base)
}

/// Enough for any real turn. A turn that produces more is running something that should be writing
/// into a folder of its own, and the cap is reported rather than silently applied.
pub const MAX_PRODUCED: usize = 64;

/// How many redirect names one turn may go looking for. A turn with more redirects than this is
/// piping something through, not producing a handful of files.
const MAX_REDIRECT_NAMES: usize = 32;

/// Entries the search for a name is allowed to read, and how deep it may go. The session cache is
/// the tree being searched, and a session that has run for days can hold a lot of iteration output.
const MAX_SEARCH_ENTRIES: u32 = 4_000;
const MAX_SEARCH_DEPTH: usize = 4;

/// The places a redirect's file can be, and the window it has to have landed in.
pub struct RedirectSources<'a> {
    /// What a relative path is relative to, and the tree a file name may be searched through.
    pub root: &'a Path,
    /// This turn's delivery folder, read by path because it lives under a dotfolder.
    pub deliverables: &'a Path,
    /// The session's cache folder, a few levels deep — where a model lands what it makes when it
    /// writes into the cache by hand instead of into the delivery folder.
    pub session_cache: &'a Path,
    /// When the turn began.
    pub since: SystemTime,
}

/// A redirect target, split into what the command line can say and what it cannot.
///
/// `> out.csv` is a path: the shell writes exactly that, and if the file is there the row can point
/// at it. `> "$OUT/v4_ws.png"` is only the shape of one — the variable held a folder and the file is
/// wherever that was. What survives of it is the file *name*, which is usually intact, and a name is
/// enough to look the real path up with.
struct RedirectTarget {
    literal: Option<PathBuf>,
    name: Option<String>,
}

/// Segments are split the way a shell reads them, so that a `cd` can be told apart from the command
/// it precedes.
fn segments(command: &str) -> Vec<&str> {
    let bytes = command.as_bytes();
    let mut out = Vec::new();
    let (mut start, mut index) = (0usize, 0usize);
    while index < bytes.len() {
        let two = command.get(index..index + 2);
        let one_byte = *bytes.get(index).unwrap();
        let width = if two == Some("&&") || two == Some("||") { 2 } else { 1 };
        let boundary = (two == Some("&&") || two == Some("||"))
            || one_byte == b';'
            || one_byte == b'|'
            || one_byte == b'\n';
        if boundary {
            out.push(&command[start..index]);
            index += width;
            start = index;
        } else {
            index += 1;
        }
    }
    out.push(&command[start..]);
    out.into_iter().map(str::trim).filter(|part| !part.is_empty()).collect()
}

fn has_variable(text: &str) -> bool {
    text.contains(['$', '`', '*', '?', '%'])
}

fn is_absolute(path: &str) -> bool {
    path.starts_with('/')
        || (path.len() > 2
            && path.as_bytes()[1] == b':'
            && matches!(path.as_bytes()[2], b'/' | b'\\'))
}

/// Where a segment moved the line to, when it is nothing but `cd <dir>`.
///
/// `cd -`, `cd ~` and `cd "$DIR"` say something no line here can resolve, and guessing would put the
/// file in a folder nobody chose — so those leave the base where it was.
fn cd_target(segment: &str) -> Option<&str> {
    let rest = segment
        .strip_prefix("cd")
        .filter(|_| segment.len() == 2 || segment[2..].starts_with([' ', '\t']))?
        .trim();
    let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'');
    let body = match quote {
        Some(quote) => rest[1..].split(quote).next()?,
        None => rest.split([' ', '\t', ';', '&', '|', '<', '>']).next()?,
    };
    let body = body.trim();
    if body.is_empty() || body == "-" || has_variable(body) {
        return None;
    }
    Some(body)
}

/// The redirect targets in one command, each resolved against the folder the line had reached by then.
///
/// Written out by hand rather than with a regex because the whole of it is "find a `>`, read the word
/// after it", and the dependency is not worth carrying for that.
fn redirect_targets(command: &str, root: &Path) -> Vec<RedirectTarget> {
    let mut out = Vec::new();
    let mut base = root.to_path_buf();
    for segment in segments(command) {
        if let Some(target) = cd_target(segment) {
            base = if is_absolute(target) {
                PathBuf::from(target)
            } else {
                base.join(target)
            };
            continue;
        }
        let bytes = segment.as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] != b'>' {
                index += 1;
                continue;
            }
            index += 1;
            while bytes.get(index).is_some_and(|byte| byte.is_ascii_whitespace()) {
                index += 1;
            }
            // `&> file`, `>&2`: a stream being redirected or duplicated, not a file being written.
            if bytes.get(index) == Some(&b'&') {
                index += 1;
                continue;
            }
            let (target, next) = read_target(segment, index);
            index = next;
            let target = target.trim();
            if target.is_empty() || target.ends_with('/') {
                continue;
            }
            if matches!(
                target.to_ascii_lowercase().as_str(),
                "/dev/null" | "/dev/stdout" | "/dev/stderr" | "nul" | "null" | "con"
            ) {
                continue;
            }
            out.push(if has_variable(target) {
                let name = file_name_of(Path::new(target)).filter(|name| !has_variable(name));
                RedirectTarget { literal: None, name }
            } else {
                let path = if is_absolute(target) {
                    PathBuf::from(target)
                } else {
                    base.join(target)
                };
                RedirectTarget { literal: Some(path), name: None }
            });
        }
    }
    out
}

/// The word after a `>`, quotes handled, and the index left after it.
fn read_target(segment: &str, from: usize) -> (&str, usize) {
    let bytes = segment.as_bytes();
    match bytes.get(from) {
        Some(&quote @ (b'"' | b'\'')) => {
            let quote = quote as char;
            let rest = &segment[from + 1..];
            match rest.find(quote) {
                Some(end) => (&rest[..end], from + 1 + end + 1),
                None => (rest, segment.len()),
            }
        }
        _ => {
            let rest = &segment[from..];
            let end = rest
                .find([' ', '\t', ';', '&', '|', '<', '>'])
                .unwrap_or(rest.len());
            (&rest[..end], from + end)
        }
    }
}

fn describe(path: &Path, bytes: u64) -> TurnDeliverable {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    TurnDeliverable {
        mime: zlogic_tools::file::sniff::detect_mime(&name, &head(path)).map(str::to_string),
        path: path.to_string_lossy().into_owned(),
        bytes,
    }
}

fn file_name_of(path: &Path) -> Option<String> {
    path.file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
        .filter(|name| !name.is_empty())
}

/// The files a turn's shell redirects wrote, on the paths they really landed.
///
/// # Why the path in the command line is not the answer
///
/// `python tile.py --name out/icon > "$LOG/icon.png"` writes `out/icon-1024.png`, and
/// `cd "<cache>/competitors" && … > logo.png` writes into `competitors` rather than into the
/// workspace: a relative target is relative to wherever the line moved itself to, which is not where
/// the tool started. Either way the command line points at nothing, so a literal target is used only
/// if the file is actually there, and otherwise the *name* is what gets looked up — through the
/// folders a model writes into, bounded, freshest first. Nothing found means nothing reported: a row
/// that opens nothing is worse than no row.
pub fn redirect_outputs(
    commands: &[String],
    sources: &RedirectSources<'_>,
    workspace: &[TurnDeliverable],
) -> Vec<TurnDeliverable> {
    let mut out: Vec<TurnDeliverable> = Vec::new();
    let mut wanted: Vec<String> = Vec::new();
    for command in commands {
        for target in redirect_targets(command, sources.root) {
            match target.literal {
                Some(path) => match std::fs::metadata(&path) {
                    Ok(meta) if meta.is_file() => out.push(describe(&path, meta.len())),
                    // A redirect that produced nothing, or one a `cd` moved out from under: the
                    // name is all that is left of it.
                    _ => {
                        if let Some(name) =
                            target.name.or_else(|| file_name_of(&path)).filter(|n| !has_variable(n))
                        {
                            wanted.push(name);
                        }
                    }
                },
                None => {
                    if let Some(name) = target.name {
                        wanted.push(name);
                    }
                }
            }
        }
    }
    wanted.sort();
    wanted.dedup();
    wanted.truncate(MAX_REDIRECT_NAMES);
    if wanted.is_empty() {
        return out;
    }

    // One pass over each folder rather than a walk per name. The winner for a name is the freshest
    // one inside the turn's window, and the freshest one overall after that: a session that rendered
    // `icon.png` yesterday has one too, and that is not the file this turn wrote.
    let mut best: std::collections::HashMap<String, (SystemTime, TurnDeliverable)> =
        std::collections::HashMap::new();
    let mut offer = |path: &Path| {
        let Some(name) = file_name_of(path) else {
            return;
        };
        if !wanted.contains(&name) {
            return;
        }
        let Ok(meta) = std::fs::metadata(path) else {
            return;
        };
        let at = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let better = best.get(&name).is_none_or(|(seen, _)| {
            *seen < sources.since && at >= sources.since || at > *seen
        });
        if better {
            best.insert(name, (at, describe(path, meta.len())));
        }
    };
    for dir in [sources.deliverables, sources.session_cache] {
        for path in walk_files(dir, MAX_SEARCH_DEPTH) {
            offer(&path);
        }
    }
    // What the workspace walk already found, for a file the search above cannot reach because the
    // name is not in a cache folder.
    for file in workspace {
        if let Some(name) = file_name_of(Path::new(&file.path))
            && wanted.contains(&name)
        {
            offer(Path::new(&file.path));
        }
    }
    out.extend(best.into_values().map(|(_, file)| file));
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out.dedup_by(|a, b| a.path == b.path);
    out
}

/// Every file under `dir`, a few levels deep and within a budget. A search, not a survey: it exists
/// to find a name, and a session that has run for a week can hold a lot of iteration output.
fn walk_files(dir: &Path, depth: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut budget = MAX_SEARCH_ENTRIES;
    let mut stack = vec![(dir.to_path_buf(), 0usize)];
    while let Some((dir, level)) = stack.pop() {
        if budget == 0 || level > depth {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if budget == 0 {
                break;
            }
            budget -= 1;
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => stack.push((path, level + 1)),
                Ok(kind) if kind.is_file() => out.push(path),
                _ => {}
            }
        }
    }
    out
}

/// How many entries one turn is allowed to look at before the walk gives up.
///
/// A budget, not a tuning knob: this runs where the user is waiting for the turn to end, and there
/// is no workspace where the cost of being wrong is worth an unbounded walk. Measured on this
/// machine, an 89,000-entry `target/` costs 590 ms on one thread and 84 ms on eight, so the cap is
/// roughly a second of worst case — reached only by a tree that is mostly build output the prune
/// list failed to recognise.
const MAX_ENTRIES: u64 = 120_000;

/// Files that appeared inside one turn's window in one directory, beyond which the directory is a
/// build's or a copy step's and nobody is being handed any of it.
///
/// The named rule below cannot need this: a turn that names fifty files meant them. The born-here
/// rule can, because a `vite build` dropping its `dist` next to the work is also files appearing in
/// a directory. A turn that renders one chart writes one file; anything past a handful in the same
/// folder is a machine, and a person is not on the other end of it.
const MAX_BORN_PER_DIRECTORY: usize = 8;

/// Directory names a file cannot qualify on being born: machines write here and nobody is given
/// anything from them.
///
/// Deliberately shorter than [`NEVER_WALKED`], which prunes and so costs the named rule nothing.
/// `out`, `output` and `build` are *not* on it, because a script that writes into `output/` is the
/// case this rule exists for and the name is too ordinary to rule out; the per-directory count above
/// is what keeps a build out of those three.
const BUILT_INTO: &[&str] = &[
    "dist",
    "htmlcov",
    "coverage",
    "bin",
    "obj",
    "vendor",
    "deps",
    "_build",
    "cmake-build-debug",
    "cmake-build-release",
];

/// Whether this file came into existence inside the window, rather than being touched by it.
///
/// A file that was already there and got rewritten is somebody else's, however recently — that is
/// what a watcher or a build step does. Windows records the birth time and answers this exactly; on
/// a filesystem that does not, the modification time stands in, and the answer gets weaker rather
/// than wrong.
fn born_since(meta: &std::fs::Metadata, since: SystemTime) -> bool {
    meta.created()
        .or_else(|_| meta.modified())
        .is_ok_and(|at| at >= since)
}

/// Whether the file's own folder is one a build writes into.
fn written_into_a_build(path: &Path) -> bool {
    path.parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .is_some_and(|name| BUILT_INTO.contains(&name))
}

/// Directory names never descended into, whatever the repository says.
///
/// This is deliberately *not* the search tools' list, and the difference is the whole judgement
/// call. `zlogic_tools::search::ignores` skips `out` / `build` / `dist` because for a search those
/// are build artefacts, and its own reasoning is even more cautious than it looks: it applies that
/// half only when there is no `.gitignore` to defer to. Here those names are where a script drops
/// what it generated — `render.py --name out/icon` writing `out/icon-1024.png` is the exact turn
/// this whole function exists for, and pruning `out` makes that turn report nothing at all, with no
/// sign that it looked. So only the half that is both expensive and never a person's output is
/// kept: dependency stores, VCS and tool metadata, Python / Gradle / Terraform caches, and the
/// Rust and Xcode compile directories, which are the two that are genuinely enormous.
///
/// What is left to bound the cost of walking a `dist` is the extension whitelist below, which no
/// build emits anything interesting through, and [`MAX_ENTRIES`] above, which quits a walk that has
/// already seen more of the tree than it is willing to. Losing `dist/index.html` from a build the
/// turn just ran is a fair price for seeing the icon. `vendor` / `Pods` / `third_party` are in
/// neither list, as before: committed source more often than not, and a report written into one is
/// still a report.
const NEVER_WALKED: &[&str] = &[
    // Version control and editor state
    ".git",
    ".hg",
    ".svn",
    ".idea",
    ".vscode",
    // Dependency stores, the reason this walk needs threads at all
    "node_modules",
    "bower_components",
    ".pnpm-store",
    ".yarn",
    // Python
    ".venv",
    "venv",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".tox",
    ".nox",
    // Tool caches
    ".gradle",
    ".terraform",
    ".serverless",
    ".aws-sam",
    ".turbo",
    ".parcel-cache",
    ".cache",
    ".direnv",
    // Compile directories: huge, and nothing anyone is handed comes out of them
    "target",
    "DerivedData",
    // Bundler output, all of them dotfolders the walker's own hidden filter takes as well — kept
    // here for the one case it cannot: a workspace whose own name starts with a dot.
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".angular",
    ".output",
];

fn is_prunable(name: &std::ffi::OsStr) -> bool {
    name.to_str().is_some_and(|name| NEVER_WALKED.contains(&name))
}

/// Things handed to a person rather than edited by one: images, documents, tabular data, media,
/// archives, and the two web formats a report gets opened as.
pub const PRODUCIBLE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "svg", "bmp", "tiff", "tif", "ico",
    "pdf", "docx", "doc", "pptx", "ppt", "odp", "ods", "rtf", "epub",
    "xlsx", "xls", "csv", "tsv", "parquet", "jsonl", "ipynb",
    "mp4", "webm", "mov", "mp3", "wav", "flac",
    "zip", "tar", "gz", "html", "htm",
];

/// Whether a name is one of the things this scan collects. Case-insensitive because Windows and
/// macOS are, and a `.PNG` from a script is the same file.
pub fn is_producible(name: &str) -> bool {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => {
            let ext = ext.to_ascii_lowercase();
            PRODUCIBLE_EXTENSIONS.contains(&ext.as_str())
        }
        _ => false,
    }
}

/// Every producible file under `root` written at or after `since`, capped.
///
/// Three things keep it affordable, in the order they matter: the repository's own ignore rules,
/// the [`is_prunable`] list on top of them, and a thread per core. Measured here: 2,900 entries
/// for this workspace, 89,000 for a `target/` tree nobody ignored, and 84 ms for the second on
/// eight threads.
///
/// Hidden folders go with the standard filters, which takes out `.git` and `.zlogic` in one
/// setting — those two are the engine's own storage, and the delivery folder is read separately,
/// by path, precisely because it lives under `.zlogic`.
///
/// # Whose file it is, and how it qualified
///
/// `own` is this turn's own record and `others` is what the sessions running alongside it in this
/// workspace have written down. A candidate is reported when this turn named it, or when nobody
/// did — and dropped when only a concurrent session did. Two sessions in one workspace are inside
/// each other's time windows, so `since` alone hands each of them the other's output; the turn's own
/// text is what breaks the tie, and it breaks it in both directions: our own command line outranks
/// a stale mention elsewhere (we overwrote the same file, our user should have it), while a file only
/// the other turn has named is that turn's and showing it here would count it twice.
///
/// Two rules let a file in, and the first is the one to trust:
///
/// 1. **Named.** The turn's own records name it — a command line, a card, its own prose — wherever
///    in the workspace it is.
/// 2. **Born here.** Nobody named it, but it did not exist when the turn started. This is what a
///    script that assembles its output paths at runtime leaves behind: `python tile.py` writing
///    `out/quarterly-2026.png` mentions nothing anywhere, and missing it is the whole failure this
///    scan was built for. The guards are a file that merely existed already (that is a watcher or a
///    build touching a file of its own), a folder a build writes into, and a folder that filled up
///    while the turn ran.
pub fn scan_workspace(
    root: &Path,
    since: SystemTime,
    own: &Footprint,
    others: &Footprint,
) -> Vec<TurnDeliverable> {
    if !root.is_dir() {
        return Vec::new();
    }
    let seen = AtomicU64::new(0);
    let exhausted = AtomicBool::new(false);
    let found: Mutex<Vec<(TurnDeliverable, bool, Option<PathBuf>)>> = Mutex::new(Vec::new());
    // Owned because the filter has to be `'static`, and because the root is the one entry the prune
    // must never take: a workspace folder that happens to be called `build` or `out` is the user's
    // project, not its output, and skipping it would make this function return nothing at all.
    let workspace = root.to_path_buf();
    // A dotfolder root is the walker's own blind spot, and it is a total one: the hidden filter is
    // applied to the root like any other entry, so a workspace called `.sandbox` would yield
    // nothing, silently, forever. The prune list is what stands in for the hidden filter in that
    // case, and it covers every dotfolder that is big (`.git`, `.venv`, `.cache`, `.next`, …).
    let hidden = !root
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with('.'));

    let walk = ignore::WalkBuilder::new(root)
        .standard_filters(true)
        // Same reasoning as the search tools: a `.gitignore` is the user's declaration and is
        // honoured whether or not a `.git` happens to sit next to it. A workspace folder that is
        // not a repository is where a half-finished project lives, and `node_modules` is the one
        // folder that must never be walked.
        .require_git(false)
        .hidden(hidden)
        .threads(thread_count())
        .filter_entry(move |entry| {
            // Pruning in the walker rather than skipping afterwards is the whole point: a skipped
            // directory is never enumerated, so its cost is one `read_dir` avoided rather than the
            // thousands of entries beneath it.
            entry.path() == workspace
                || !entry
                    .file_type()
                    .is_some_and(|kind| kind.is_dir() && is_prunable(entry.file_name()))
        })
        .build_parallel();

    walk.run(|| {
        let found = &found;
        let seen = &seen;
        let exhausted = &exhausted;
        Box::new(move |result: Result<ignore::DirEntry, ignore::Error>| {
            if seen.fetch_add(1, Ordering::Relaxed) >= MAX_ENTRIES {
                exhausted.store(true, Ordering::Relaxed);
                return ignore::WalkState::Quit;
            }
            let Ok(entry) = result else {
                return ignore::WalkState::Continue;
            };
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                return ignore::WalkState::Continue;
            }
            let Ok(meta) = entry.metadata() else {
                return ignore::WalkState::Continue;
            };
            if !meta.modified().is_ok_and(|m| m >= since) {
                return ignore::WalkState::Continue;
            }
            let path = entry.path();
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if !is_producible(&name) {
                return ignore::WalkState::Continue;
            }
            // Fresh in this turn's window, but the window is a clock and several sessions share it.
            // Ownership is what the turn's own record decides — see the function's docs.
            if !own.names(path) && others.names(path) {
                return ignore::WalkState::Continue;
            }
            // Two ways to qualify, in the order they are trusted. Named: the turn's own records say
            // so, wherever the file is. Born here: nothing said anything, but the file did not
            // exist when the turn began — which is what a script assembling its output paths at
            // runtime leaves behind, and the reason this scan exists at all.
            let named = own.names(path);
            let born = !named && born_since(&meta, since) && !written_into_a_build(path);
            if !named && !born {
                return ignore::WalkState::Continue;
            }
            let mut files = found.lock().unwrap();
            if files.len() < MAX_PRODUCED {
                files.push((
                    TurnDeliverable {
                        mime: zlogic_tools::file::sniff::detect_mime(&name, &head(path))
                            .map(str::to_string),
                        path: path.to_string_lossy().into_owned(),
                        bytes: meta.len(),
                    },
                    born,
                    path.parent().map(Path::to_path_buf),
                ));
            }
            ignore::WalkState::Continue
        })
    });

    if exhausted.load(Ordering::Relaxed) {
        tracing::warn!(
            target: "zlogic::core",
            root = %root.display(),
            entries = seen.load(Ordering::Relaxed),
            "a turn looked at more of the workspace than it is willing to; some output may be missing"
        );
    }
    let found = found.into_inner().unwrap_or_default();
    // A directory that filled up while the turn ran was a machine's, whatever any single file in it
    // looks like. Counted only over the born ones: the named ones were meant either way.
    let mut born_per_directory: std::collections::HashMap<PathBuf, usize> =
        std::collections::HashMap::new();
    for (_, born, dir) in &found {
        if let (true, Some(dir)) = (*born, dir) {
            *born_per_directory.entry(dir.clone()).or_default() += 1;
        }
    }
    let mut out: Vec<TurnDeliverable> = found
        .into_iter()
        .filter(|(_, born, dir)| {
            !*born
                || dir.as_ref().is_none_or(|dir| {
                    born_per_directory.get(dir).copied().unwrap_or(0) <= MAX_BORN_PER_DIRECTORY
                })
        })
        .map(|(file, _, _)| file)
        .collect();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// One thread per core, capped: the walk is IO-bound on directory handles, so it scales until the
/// disk queue saturates, and past that it is just context switches on a turn the user is waiting for.
fn thread_count() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get().clamp(2, 8))
}

/// Enough bytes to tell a PNG from a mislabelled text file. Empty when the file cannot be read, in
/// which case the name is all the description there is.
fn head(path: &Path) -> Vec<u8> {
    const HEAD: usize = 4096;
    let Ok(mut file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let mut buffer = vec![0u8; HEAD];
    match std::io::Read::read(&mut file, &mut buffer) {
        Ok(read) => {
            buffer.truncate(read);
            buffer
        }
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_suffix_is_not_also_its_short_one() {
        assert!(is_producible("chart.png"));
        assert!(is_producible("CHART.PNG"), "Windows and macOS are case-insensitive");
        assert!(is_producible("icon.svg"));
        assert!(!is_producible("lib.rs"));
        assert!(!is_producible("plot.py"));
        assert!(!is_producible("README"), "no suffix is not a deliverable");
        assert!(!is_producible(".png"), "a dotfile is not a png");
    }

    fn write(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, bytes).unwrap();
    }

    /// A redirect that wrote where it said is taken at its word — the file is there, so there is
    /// nothing to look up. The `cd` in front of it is what makes this interesting: the target is
    /// relative to the folder the line moved itself into, not to the workspace.
    #[test]
    fn a_redirect_lands_where_the_line_had_moved_itself_to() {
        let root = std::env::temp_dir().join(format!("produced-{}", TurnId::new()));
        let since = SystemTime::now() - std::time::Duration::from_secs(60);
        let competitors = root.join(".zlogic/cache/s/competitors");
        write(&competitors.join("localsend-512.png"), b"\x89PNG\r\n\x1a\n");

        let cache = root.join(".zlogic/cache/s");
        let found = redirect_outputs(
            &[format!(
                "cd \"{}\" && gh api x --jq '.content' | base64 -d > localsend-512.png",
                competitors.display()
            )],
            &RedirectSources {
                root: &root,
                deliverables: &cache.join("t/deliverables"),
                session_cache: &cache,
                since,
            },
            &[],
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].path.ends_with("competitors\\localsend-512.png")
                || found[0].path.ends_with("competitors/localsend-512.png"),
            "{found:?}"
        );
        assert_eq!(found[0].mime.as_deref(), Some("image/png"));

        std::fs::remove_dir_all(&root).ok();
    }

    /// The case as it actually happened: the line named a file, and the file is not where the line
    /// said. Only the name survives, so the name is what gets looked up — through the folders a
    /// model writes into, a few levels deep, since the file is usually one folder in.
    #[test]
    fn a_redirect_that_missed_its_folder_is_found_by_name() {
        let root = std::env::temp_dir().join(format!("produced-{}", TurnId::new()));
        let since = SystemTime::now() - std::time::Duration::from_secs(60);
        let cache = root.join(".zlogic/cache/s");
        write(&cache.join("competitors/logo.png"), b"\x89PNG\r\n\x1a\n");
        // Yesterday's render of the same name: same session, different turn.
        let old = cache.join("renders/logo.png");
        write(&old, b"\x89PNG\r\n\x1a\n");
        let yesterday = SystemTime::now() - std::time::Duration::from_secs(86_400);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(yesterday))
            .unwrap();

        let found = redirect_outputs(
            &[r#"python tile.py --all > "$OUT/logo.png""#.to_owned()],
            &RedirectSources {
                root: &root,
                deliverables: &cache.join("t/deliverables"),
                session_cache: &cache,
                since,
            },
            &[],
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].path.contains("competitors"),
            "the file this turn wrote, not yesterday's render: {found:?}"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// A redirect whose file is nowhere is not a row: the entry says the turn wrote something, and
    /// there is nothing behind it to open.
    #[test]
    fn a_redirect_nobody_produced_is_not_reported() {
        let root = std::env::temp_dir().join(format!("produced-{}", TurnId::new()));
        let cache = root.join(".zlogic/cache/s");
        let found = redirect_outputs(
            &["python tile.py > out/chart.png".to_owned()],
            &RedirectSources {
                root: &root,
                deliverables: &cache.join("t/deliverables"),
                session_cache: &cache,
                since: SystemTime::now() - std::time::Duration::from_secs(60),
            },
            &[],
        );
        assert!(found.is_empty(), "{found:?}");

        std::fs::remove_dir_all(&root).ok();
    }

    /// The case that motivated this: a script builds its path at runtime, so nothing anywhere
    /// names the file, and all we have is that it appeared during the turn.
    #[test]
    fn files_written_during_the_turn_are_found_wherever_a_script_put_them() {
        let root = std::env::temp_dir().join(format!("produced-{}", TurnId::new()));
        write(&root.join("render/icon-1024.png"), b"\x89PNG\r\n\x1a\n and then some");
        let since = SystemTime::now() - std::time::Duration::from_secs(60);

        let found = scan_workspace(&root, since, &Footprint::default(), &Footprint::default());
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].path.ends_with("icon-1024.png"));
        assert_eq!(found[0].mime.as_deref(), Some("image/png"));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_source_file_the_turn_edited_is_not_reported() {
        let root = std::env::temp_dir().join(format!("produced-{}", TurnId::new()));
        write(&root.join("src/lib.rs"), b"fn main() {}");
        write(&root.join("output/chart.png"), b"\x89PNG\r\n\x1a\n");
        let since = SystemTime::now() - std::time::Duration::from_secs(60);

        let found = scan_workspace(&root, since, &Footprint::default(), &Footprint::default());
        assert_eq!(found.len(), 1, "the diff card owns the source file: {found:?}");
        assert!(found[0].path.ends_with("chart.png"));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_file_older_than_the_window_is_not_this_turns() {
        let root = std::env::temp_dir().join(format!("produced-{}", TurnId::new()));
        write(&root.join("old.png"), b"\x89PNG\r\n\x1a\n");
        let since = SystemTime::now() + std::time::Duration::from_secs(60);

        assert!(scan_workspace(&root, since, &Footprint::default(), &Footprint::default()).is_empty());

        std::fs::remove_dir_all(&root).ok();
    }

    /// No `.gitignore` anywhere in this tree, so the prune list is the only thing standing between
    /// the walk and a project's dependency tree — which is the case a `.gitignore` cannot be relied
    /// on to cover, and the one that decides whether this function is affordable.
    #[test]
    fn dependency_and_build_trees_are_pruned_without_a_gitignore() {
        let root = std::env::temp_dir().join(format!("produced-{}", TurnId::new()));
        write(&root.join("node_modules/pkg/chart.png"), b"\x89PNG\r\n\x1a\n");
        write(&root.join("target/debug/build/out/chart.png"), b"\x89PNG\r\n\x1a\n");
        write(&root.join(".zlogic/cache/s/t/deliverables/chart.png"), b"\x89PNG\r\n\x1a\n");
        write(&root.join("output/icon-1024.png"), b"\x89PNG\r\n\x1a\n");
        let since = SystemTime::now() - std::time::Duration::from_secs(60);

        let found = scan_workspace(&root, since, &Footprint::default(), &Footprint::default());
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].path.ends_with("icon-1024.png") && found[0].path.contains("output"),
            "only the renderer's own output; dependencies, build output and the engine's storage are not the user's to receive: {}",
            found[0].path
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// The judgement call this scan makes against the search tools', and the two halves of it.
    /// `out` is where a script puts what it generated — pruning it would blind the scan to the
    /// turn it exists for — while the dependency stores and compile directories are the cost that
    /// has to stay bounded.
    #[test]
    fn an_output_directory_is_walked_but_a_dependency_tree_is_not() {
        let outer = std::env::temp_dir().join(format!("produced-{}", TurnId::new()));
        let root = outer.join("project");
        let since = SystemTime::now() - std::time::Duration::from_secs(60);
        write(&root.join("out/icon-1024.png"), b"\x89PNG\r\n\x1a\n");
        write(&root.join("dist/report.html"), b"<html></html>");
        write(&root.join("node_modules/pkg/logo.png"), b"\x89PNG\r\n\x1a\n");
        write(&root.join("target/debug/icon.png"), b"\x89PNG\r\n\x1a\n");
        write(&root.join(".venv/lib/icon.png"), b"\x89PNG\r\n\x1a\n");

        let found: Vec<String> = scan_workspace(&root, since, &Footprint::default(), &Footprint::default())
            .into_iter()
            .map(|file| file.path)
            .collect();
        // These are strings, not `Path`s, so the separator is whatever the platform prints.
        let tail = |path: &String| path.replace('\\', "/");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found.iter().any(|path| tail(path).ends_with("out/icon-1024.png")),
            "out/ is an ordinary place for a script to put what it made: {found:?}"
        );

        // `dist/` is where a build puts things, so a file appearing there on its own is the build's
        // doing — and stays unreported until the turn names it, which is the rule the other half of
        // the scan runs on.
        let named = Footprint::default();
        named.note("npm run build -- --out dist/report.html");
        let named: Vec<String> = scan_workspace(&root, since, &named, &Footprint::default())
            .into_iter()
            .map(|file| file.path)
            .collect();
        assert!(
            named.iter().any(|path| tail(path).ends_with("dist/report.html")),
            "named by the turn that wrote it: {named:?}"
        );

        std::fs::remove_dir_all(&outer).ok();
    }

    /// A file that was already in the tree and got rewritten is not this turn's, however recently:
    /// that is a watcher or a build touching its own output.
    ///
    /// Windows-only because it is the only way to *set* a birth time: the file has to look modified
    /// inside the window and created outside it, and no other platform lets a test say so.
    #[cfg(windows)]
    #[test]
    fn a_file_that_merely_existed_already_is_not_reported() {
        use std::os::windows::fs::FileTimesExt;

        let outer = std::env::temp_dir().join(format!("produced-{}", TurnId::new()));
        let root = outer.join("project");
        let old = root.join("assets/logo.png");
        write(&old, b"\x89PNG\r\n\x1a\n");
        let yesterday = SystemTime::now() - std::time::Duration::from_secs(86_400);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_created(yesterday)
                    .set_modified(SystemTime::now()),
            )
            .unwrap();

        let found = scan_workspace(
            &root,
            SystemTime::now() - std::time::Duration::from_secs(60),
            &Footprint::default(),
            &Footprint::default(),
        );
        assert!(found.is_empty(), "{found:?}");

        std::fs::remove_dir_all(&outer).ok();
    }

    /// One chart is a hand-over; forty documents appearing in one folder while the turn ran is a
    /// `pandoc` or a copy step, and none of them is something the user was given.
    #[test]
    fn a_folder_that_filled_up_while_the_turn_ran_is_a_machines() {
        let outer = std::env::temp_dir().join(format!("produced-{}", TurnId::new()));
        let root = outer.join("project");
        let since = SystemTime::now() - std::time::Duration::from_secs(60);
        for index in 0..=MAX_BORN_PER_DIRECTORY {
            write(&root.join(format!("out/page-{index}.html")), b"<html></html>");
        }
        write(&root.join("out/cover.png"), b"\x89PNG\r\n\x1a\n");

        let found = scan_workspace(&root, since, &Footprint::default(), &Footprint::default());
        assert!(found.is_empty(), "{found:?}");

        std::fs::remove_dir_all(&outer).ok();
    }

    /// Two sessions in one workspace are inside each other's time windows, so the window alone hands
    /// each of them the other's output. The turn's own record is what settles it, in both
    /// directions: a file this turn's command line names is its own even if a concurrent turn also
    /// touched the name, and a file only the *other* turn named is that turn's.
    #[test]
    fn a_file_another_running_session_named_is_not_this_turns() {
        let outer = std::env::temp_dir().join(format!("produced-{}", TurnId::new()));
        let root = outer.join("project");
        let since = SystemTime::now() - std::time::Duration::from_secs(60);
        write(&root.join("out/mine-1024.png"), b"\x89PNG\r\n\x1a\n");
        write(&root.join("out/theirs-512.png"), b"\x89PNG\r\n\x1a\n");

        let own = Footprint::default();
        // `render.py --name out/mine` is how `out/mine-1024.png` came to exist: the command names the
        // stem, never the file.
        own.note(r#"{"command":"python render.py --name out/mine --size 1024"}"#);
        let others = Footprint::default();
        others.note(r#"{"command":"python tile.py --name out/theirs --size 512"}"#);

        let found: Vec<String> = scan_workspace(&root, since, &own, &others)
            .into_iter()
            .map(|file| file.path)
            .collect();
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].ends_with("mine-1024.png"), "{found:?}");
        assert!(found[0].contains("out"), "{found:?}");

        std::fs::remove_dir_all(&outer).ok();
    }

    /// With nothing to compare against, ownership cannot be established and the window still decides:
    /// a file nobody named is a candidate, because the alternative is losing every output whose
    /// name the model assembled at runtime — which is the case the scan is here for.
    #[test]
    fn a_file_nobody_named_is_still_a_candidate() {
        let outer = std::env::temp_dir().join(format!("produced-{}", TurnId::new()));
        let root = outer.join("project");
        let since = SystemTime::now() - std::time::Duration::from_secs(60);
        write(&root.join("deep/nested/out/chart.png"), b"\x89PNG\r\n\x1a\n");

        let found = scan_workspace(
            &root,
            since,
            &Footprint::default(),
            &Footprint::default(),
        );
        assert_eq!(found.len(), 1, "{found:?}");

        std::fs::remove_dir_all(&outer).ok();
    }

    /// The loosest form of a name is a trap: a two-letter base appears in almost any text, so it
    /// would make every file in the workspace look like this turn's.
    #[test]
    fn a_name_too_short_to_mean_anything_does_not_count_as_a_mention() {
        let file = Path::new("/w/out/ab-2.png");
        assert!(!names_in("ran the ab tests", file), "'ab' alone means nothing");
        assert!(names_in("wrote ab-2.png", file), "the full name still counts");
        assert!(names_in("cp /w/out/ab-2.png /tmp", file), "the path still counts");
    }

    /// A dotfolder workspace is still a workspace. The hidden filter is applied to the walk root
    /// like any other entry, so without this the scan returns nothing at all for a folder called
    /// `.sandbox` — and says nothing about having done so.
    #[test]
    fn a_dotfolder_workspace_is_still_walked() {
        let outer = std::env::temp_dir().join(format!("produced-{}", TurnId::new()));
        let root = outer.join(".sandbox");
        write(&root.join("chart.png"), b"\x89PNG\r\n\x1a\n");
        let since = SystemTime::now() - std::time::Duration::from_secs(60);

        let found = scan_workspace(&root, since, &Footprint::default(), &Footprint::default());
        assert_eq!(found.len(), 1, "{found:?}");

        std::fs::remove_dir_all(&outer).ok();
    }

    /// A workspace folder called `build` is the user's project, not its output. The prune is applied
    /// by the walker, which applies it to the root too, so this is the case where forgetting that
    /// would make the whole scan return nothing — silently, on every turn.
    #[test]
    fn a_workspace_whose_own_folder_is_named_like_a_build_is_still_walked() {
        let outer = std::env::temp_dir().join(format!("produced-{}", TurnId::new()));
        let root = outer.join("build");
        write(&root.join("chart.png"), b"\x89PNG\r\n\x1a\n");
        let since = SystemTime::now() - std::time::Duration::from_secs(60);

        let found = scan_workspace(&root, since, &Footprint::default(), &Footprint::default());
        assert_eq!(found.len(), 1, "{found:?}");

        std::fs::remove_dir_all(&outer).ok();
    }

    /// The list that keeps the walk affordable, checked without a walk. The expensive half is what
    /// is pruned; the ambiguous half is deliberately not, because `out` and `dist` are where a
    /// script writes what it generated and a `report.html` in either is a file the user is meant to
    /// open. Only the compile directories among them stay, because nothing anyone is handed comes
    /// out of a `target`.
    #[test]
    fn only_the_expensive_half_of_the_build_output_names_is_pruned() {
        for name in [
            "node_modules", "target", "DerivedData", ".next", "__pycache__",
            ".venv", "bower_components", ".pnpm-store", ".gradle", ".terraform", ".git",
        ] {
            assert!(is_prunable(std::ffi::OsStr::new(name)), "{name} must be pruned");
        }
        for name in [
            "src", "app-icon", "assets", "third_party", "vendor", "output", "out", "dist",
            "build", "bin", "coverage", "_build",
        ] {
            assert!(
                !is_prunable(std::ffi::OsStr::new(name)),
                "{name} is walked: a script may have written something there"
            );
        }
    }

    use zlogic_protocol::TurnId;
}
