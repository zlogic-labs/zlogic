//! # zlogic-logging
//! One place that installs `tracing` consistently.
//! ```text
//! <state>/logs/
//! ├── 2026-08-25/
//! │   ├── policy.log
//! │   ├── llm.log
//! │   ├── app.log
//! │   └── error.log   — every warn/error line, whichever module it came from
//! └── 2026-08-26/
//!     └── …
//! ```
//! # Why a crate and not a Java-style appender abstraction
//! `tracing-subscriber`'s `Layer` *is* the appender concept: one fmt layer to a file, another
//! to the console, composed. Wrapping that in an `Appender` trait would add vocabulary
//! without adding capability.
//! What does deserve to be written down once are three specific hazards:
//! 1. **A TUI must never write to stderr.** Ratatui owns the terminal; a log line lands in the
//!    middle of a frame and corrupts the display until the next full redraw. [`Sink::File`]
//!    exists so this is decided at startup rather than discovered by a user whose screen went
//!    strange.
//! 2. **`tracing-appender`'s non-blocking writer returns a guard, and dropping it silently
//!    stops all file logging.** It is a `#[must_use]` trivially defeated by
//!    `let _ = init(...)`. [`LogGuard`] owns those guards, and holding it is what keeps them alive.
//! 3. **Installing twice fails**, and in tests every case would try. [`init`] reports that as
//!    `Ok(None)` rather than an error, so a test helper can call it unconditionally.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use tracing::Metadata;
use tracing::Subscriber;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::FilterFn;
use tracing_subscriber::layer::{Layer, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use zlogic_config::LogConfig;

mod privacy;

/// Where log lines go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sink {
    /// Per-day directories of per-module files under the given directory
    /// (`<dir>/<YYYY-MM-DD>/policy.log` etc.), plus one `error.log` holding every warn/error
    /// line from any module.
    /// **The only correct choice for a TUI.**
    File { dir: PathBuf },
    /// stderr. For processes that do not own the terminal: one-shot commands, tests.
    Stderr,
    /// Both. What a GUI host wants — devtools sees it live, the files survive a crash.
    Both { dir: PathBuf },
    /// Nothing at all.
    Off,
}

impl Sink {
    /// The sink implied by configuration, given whether this process owns the terminal.
    /// `owns_terminal` is passed in rather than sniffed from `isatty`: a TUI is a TUI whether
    /// or not stderr happens to be a tty, and getting it wrong is exactly the corrupted-screen
    /// failure this crate exists to prevent.
    pub fn from_config(cfg: &LogConfig, dir: &Path, owns_terminal: bool) -> Self {
        match (cfg.to_file, owns_terminal) {
            (true, true) => Sink::File {
                dir: dir.to_path_buf(),
            },
            (true, false) => Sink::Both {
                dir: dir.to_path_buf(),
            },
            (false, true) => Sink::Off,
            (false, false) => Sink::Stderr,
        }
    }
}

/// Keeps the logging machinery alive.
/// **Hold this for the lifetime of the process.** Dropping it flushes and then stops file
/// logging — the writer threads go away with the guards. Losing it is the classic way to end
/// up with empty log files and no error anywhere explaining why.
#[must_use = "dropping this stops file logging; keep it alive for the whole process"]
pub struct LogGuard {
    _file_guards: Vec<tracing_appender::non_blocking::WorkerGuard>,
    log_dir: Option<PathBuf>,
}

impl LogGuard {
    /// The directory being written to, if any — for telling the user where to look.
    pub fn log_dir(&self) -> Option<&Path> {
        self.log_dir.as_deref()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LoggingError {
    #[error("cannot create log directory {path}: {reason}")]
    Io { path: PathBuf, reason: String },
    #[error("invalid log level {0:?}")]
    BadLevel(String),
}

/// Installs the global subscriber.
/// Returns `Ok(None)` when a subscriber is already installed — not an error, just what happens
/// when a test helper runs in more than one test in the same process.
pub fn init(cfg: &LogConfig, sink: Sink) -> Result<Option<LogGuard>, LoggingError> {
    install_panic_hook();
    let (subscriber, guards, log_dir) = build(cfg, &sink)?;
    match subscriber.try_init() {
        Ok(()) => Ok(Some(LogGuard {
            _file_guards: guards,
            log_dir,
        })),
        // Already installed. Deliberately not an error; see the function docs.
        Err(_) => Ok(None),
    }
}

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
                (*s).to_string()
            } else if let Some(s) = info.payload().downcast_ref::<String>() {
                s.clone()
            } else {
                "(non-string panic payload)".to_string()
            };
            let backtrace = std::backtrace::Backtrace::force_capture().to_string();
            match info.location() {
                Some(location) => tracing::error!(
                    target: "zlogic::panic",
                    payload,
                    %location,
                    backtrace,
                    "panic"
                ),
                None => tracing::error!(target: "zlogic::panic", payload, backtrace, "panic"),
            }
        }));
        previous(info);
    }));
}

/// Builds the layered subscriber for a sink, without touching the global default.
/// Split out of [`init`] so tests can install it scoped with
/// `tracing::subscriber::with_default` instead of fighting over the global subscriber.
/// The guards must be kept alive by the caller as long as the subscriber is in use; `init`
/// moves them into [`LogGuard`].
fn build(
    cfg: &LogConfig,
    sink: &Sink,
) -> Result<
    (
        Box<dyn Subscriber + Send + Sync>,
        Vec<tracing_appender::non_blocking::WorkerGuard>,
        Option<PathBuf>,
    ),
    LoggingError,
> {
    let filter = build_filter(&cfg.level)?;

    let mut guards: Vec<tracing_appender::non_blocking::WorkerGuard> = Vec::new();
    let log_dir: Option<PathBuf>;
    let subscriber: Box<dyn Subscriber + Send + Sync> = match sink {
        Sink::Off => {
            log_dir = None;
            Box::new(tracing_subscriber::registry().with(filter))
        }
        Sink::Stderr => {
            log_dir = None;
            Box::new(
                tracing_subscriber::registry()
                    .with(filter)
                    .with(console_layer()),
            )
        }
        Sink::File { dir } | Sink::Both { dir } => {
            ensure_log_root(dir)?;
            let (policy, guard) = file_layer(dir, Module::Policy);
            let (llm, guard2) = file_layer(dir, Module::Llm);
            let (app, guard3) = file_layer(dir, Module::App);
            let (errors, guard4) = error_layer(dir);
            guards.extend([guard, guard2, guard3, guard4]);
            log_dir = Some(dir.clone());
            let files = tracing_subscriber::registry()
                .with(filter)
                .with(policy)
                .with(llm)
                .with(app)
                .with(errors);
            match sink {
                Sink::Both { .. } => Box::new(files.with(console_layer())),
                _ => Box::new(files),
            }
        }
    };
    Ok((subscriber, guards, log_dir))
}

fn ensure_log_root(dir: &Path) -> Result<(), LoggingError> {
    std::fs::create_dir_all(dir).map_err(|e| LoggingError::Io {
        path: dir.to_path_buf(),
        reason: e.to_string(),
    })
}

fn file_layer<S>(
    dir: &Path,
    module: Module,
) -> (impl Layer<S>, tracing_appender::non_blocking::WorkerGuard)
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    file_layer_matching(dir, module.file_stem(), module.target_filter())
}

/// The one file not chosen by target: every warn and error line, whatever module it came from, so
/// a bug report is a single file to read. Duplicated on purpose — the module file stays the place
/// where an error has its context. Like every other file here it is created on first write, so a
/// quiet day has no `error.log`.
///
/// Spans are let through whatever their level. A per-layer filter also filters the **spans**, and
/// an INFO-level turn span failing this test makes the layer lose the scope — which is exactly the
/// `turn_id=…` an `error.log` line most needs to carry.
fn error_layer<S>(dir: &Path) -> (impl Layer<S>, tracing_appender::non_blocking::WorkerGuard)
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    file_layer_matching(dir, "error", |meta: &Metadata<'_>| {
        meta.is_span() || *meta.level() <= tracing::Level::WARN
    })
}

fn file_layer_matching<S>(
    dir: &Path,
    stem: &'static str,
    accepts: fn(&Metadata<'_>) -> bool,
) -> (impl Layer<S>, tracing_appender::non_blocking::WorkerGuard)
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    let (writer, guard) = tracing_appender::non_blocking(DailyModuleWriter::new(dir, stem));
    let layer = tracing_subscriber::fmt::layer()
        .with_writer(writer)
        // ANSI off when a file is involved: escape codes make `grep` output unreadable and
        // confuse every log viewer.
        .with_ansi(false)
        .with_target(true)
        // The gate every field goes through, on events and on spans alike.
        .fmt_fields(privacy::SafeFields)
        .with_filter(FilterFn::new(accepts));
    (layer, guard)
}

fn console_layer<S>() -> impl Layer<S>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_ansi(true)
        .with_target(true)
        // Same gate as the files: a devtools console is still a place content must not land.
        .fmt_fields(privacy::SafeFields)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Module {
    Policy,
    Llm,
    App,
}

impl Module {
    fn file_stem(self) -> &'static str {
        match self {
            Module::Policy => "policy",
            Module::Llm => "llm",
            Module::App => "app",
        }
    }

    fn target_filter(self) -> fn(&Metadata<'_>) -> bool {
        match self {
            Module::Policy => |meta| route(Module::Policy, meta.target()),
            Module::Llm => |meta| route(Module::Llm, meta.target()),
            Module::App => |meta| route(Module::App, meta.target()),
        }
    }
}

fn route(module: Module, target: &str) -> bool {
    match module {
        Module::Policy => target_matches("zlogic::policy", target),
        Module::Llm => target_matches("zlogic::llm", target),
        Module::App => {
            !target_matches("zlogic::policy", target) && !target_matches("zlogic::llm", target)
        }
    }
}

fn target_matches(prefix: &str, target: &str) -> bool {
    target
        .strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
}

struct DailyModuleWriter {
    root: PathBuf,
    module: &'static str,
    current: Option<(String, File)>,
    written: u64,
    capped: bool,
}

/// Per day, per module. A debug run that goes wrong produces a log nobody can grep and a disk that
/// fills; past this the day is marked incomplete and further lines are dropped.
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;

impl DailyModuleWriter {
    fn new(root: &Path, stem: &'static str) -> Self {
        Self {
            root: root.to_path_buf(),
            module: stem,
            current: None,
            written: 0,
            capped: false,
        }
    }

    fn open_today(&mut self) -> io::Result<&mut File> {
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        if self.current.as_ref().map(|(d, _)| d.as_str()) != Some(today.as_str()) {
            let dir = self.root.join(&today);
            std::fs::create_dir_all(&dir).map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("cannot create log directory {}: {e}", dir.display()),
                )
            })?;
            let path = dir.join(format!("{}.log", self.module));
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(|e| {
                    io::Error::new(
                        e.kind(),
                        format!("cannot open log file {}: {e}", path.display()),
                    )
                })?;
            // Reopening an existing day has to start from its real size, not from zero.
            self.written = file.metadata().map(|m| m.len()).unwrap_or(0);
            self.capped = false;
            self.current = Some((today, file));
        }
        Ok(&mut self.current.as_mut().expect("just set").1)
    }
}

impl Write for DailyModuleWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.capped {
            return Ok(buf.len());
        }
        if self.written.saturating_add(buf.len() as u64) > MAX_FILE_BYTES {
            let module = self.module;
            let file = self.open_today()?;
            let _ = writeln!(
                file,
                "--- {module}.log hit the {MAX_FILE_BYTES} byte cap; later lines today are missing ---"
            );
            let _ = file.flush();
            self.capped = true;
            return Ok(buf.len());
        }
        self.open_today()?.write(buf)?;
        self.written += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some((_, file)) = &mut self.current {
            file.flush()?;
        }
        Ok(())
    }
}

/// Builds the filter from a level name.
/// `RUST_LOG` wins when set, so someone chasing a bug can raise verbosity for one run without
/// editing configuration — and without that temporary change being written back to disk.
fn build_filter(level: &str) -> Result<EnvFilter, LoggingError> {
    if let Ok(from_env) = EnvFilter::try_from_default_env() {
        return Ok(from_env);
    }
    let level = level.trim().to_ascii_lowercase();
    if !matches!(
        level.as_str(),
        "error" | "warn" | "info" | "debug" | "trace" | "off"
    ) {
        return Err(LoggingError::BadLevel(level));
    }
    // Our own crates at the requested level; dependencies stay at warn, so a debug session is
    let directives = format!(
        "warn,zlogic={level},zlogic_desktop={level},zlogic_daemon={level}"
    );
    EnvFilter::try_new(&directives).map_err(|_| LoggingError::BadLevel(level))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(level: &str, to_file: bool) -> LogConfig {
        LogConfig {
            level: level.into(),
            to_file,
        }
    }

    /// The rule this crate exists for: a process owning the terminal must not log to it.
    #[test]
    fn a_tui_never_gets_a_stderr_sink() {
        let dir = Path::new("/tmp/zlogic-logs");
        assert_eq!(
            Sink::from_config(&cfg("info", true), dir, true),
            Sink::File {
                dir: dir.to_path_buf()
            }
        );
        // With file logging off a TUI gets silence, not a corrupted screen.
        assert_eq!(Sink::from_config(&cfg("info", false), dir, true), Sink::Off);
    }

    #[test]
    fn non_tui_processes_get_the_console_too() {
        let dir = Path::new("/tmp/zlogic-logs");
        assert_eq!(
            Sink::from_config(&cfg("info", true), dir, false),
            Sink::Both {
                dir: dir.to_path_buf()
            }
        );
        assert_eq!(
            Sink::from_config(&cfg("info", false), dir, false),
            Sink::Stderr
        );
    }

    #[test]
    fn bad_levels_are_rejected() {
        assert!(build_filter("verbose").is_err());
        assert!(build_filter("").is_err());
        for good in ["error", "warn", "info", "debug", "trace", "off", " INFO "] {
            assert!(build_filter(good).is_ok(), "{good}");
        }
    }

    /// Dependencies stay quiet so a debug session stays readable.
    #[test]
    fn dependencies_are_not_raised_with_our_crates() {
        let f = build_filter("debug").unwrap().to_string();
        assert!(
            f.contains("warn"),
            "third-party default must stay warn: {f}"
        );
        assert!(f.contains("debug"));
    }

    /// One process, several `init` calls. The second must be a benign `Ok(None)`, or every
    /// test wanting logs would have to coordinate with every other.
    #[test]
    fn initialising_twice_is_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let sink = Sink::File {
            dir: tmp.path().join("logs"),
        };

        let first = init(&cfg("info", true), sink.clone()).unwrap();
        assert!(first.is_some(), "the first call installs the subscriber");
        let second = init(&cfg("info", true), sink).unwrap();
        assert!(second.is_none(), "the second call must not fail");
    }

    #[test]
    fn module_routing_is_exclusive_by_target_prefix() {
        assert!(route(Module::Policy, "zlogic::policy"));
        assert!(
            !route(Module::Policy, "zlogic::policyx"),
            "the prefix must sit on a :: boundary"
        );
        assert!(!route(Module::Policy, "zlogic::llm::retry"));

        assert!(route(Module::Llm, "zlogic::llm"));
        assert!(route(Module::Llm, "zlogic::llm::retry"));
        assert!(!route(Module::Llm, "zlogic::policy"));

        assert!(route(Module::App, "zlogic::engine"));
        assert!(route(Module::App, "zlogic::daemon::http"));
        assert!(route(Module::App, "hello"));
        assert!(!route(Module::App, "zlogic::policy"));
        assert!(!route(Module::App, "zlogic::llm"));

        for target in [
            "zlogic::policy",
            "zlogic::llm::retry",
            "zlogic::engine",
            "",
            "anything",
        ] {
            let hits = [
                route(Module::Policy, target),
                route(Module::Llm, target),
                route(Module::App, target),
            ]
            .iter()
            .filter(|hit| **hit)
            .count();
            assert_eq!(hits, 1, "target {target:?} must go to exactly one file");
        }
    }

    #[test]
    fn daily_writer_puts_files_under_a_date_directory() {
        use std::io::Write as _;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("logs");
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        let mut policy = DailyModuleWriter::new(&root, "policy");
        let mut llm = DailyModuleWriter::new(&root, "llm");
        writeln!(policy, "audit line").unwrap();
        writeln!(llm, "llm line").unwrap();
        policy.flush().unwrap();
        llm.flush().unwrap();

        let day_dir = root.join(&today);
        assert!(
            day_dir.is_dir(),
            "a directory must be created per day: {}",
            day_dir.display()
        );
        let policy_log = day_dir.join("policy.log");
        let llm_log = day_dir.join("llm.log");
        assert_eq!(
            std::fs::read_to_string(&policy_log).unwrap().trim(),
            "audit line"
        );
        assert_eq!(
            std::fs::read_to_string(&llm_log).unwrap().trim(),
            "llm line"
        );
        assert!(
            !day_dir.join("app.log").exists(),
            "a module that never wrote anything must not get an empty file"
        );
    }

    #[test]
    fn events_route_into_per_module_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("logs");
        let (subscriber, guards) = build(&cfg("info", true), &Sink::File { dir: dir.clone() })
            .map(|(s, g, _)| (s, g))
            .unwrap();

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "zlogic::policy", "policy audit");
            tracing::error!(target: "zlogic::llm::retry", "llm retry");
            tracing::info!(target: "zlogic::engine", "engine line");
        });
        drop(guards);

        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let day_dir = dir.join(&today);
        let policy = std::fs::read_to_string(day_dir.join("policy.log")).unwrap();
        let llm = std::fs::read_to_string(day_dir.join("llm.log")).unwrap();
        let app = std::fs::read_to_string(day_dir.join("app.log")).unwrap();

        assert!(policy.contains("policy audit"), "{policy}");
        assert!(!policy.contains("llm retry"), "{policy}");

        assert!(llm.contains("llm retry"), "{llm}");
        assert!(!llm.contains("policy audit"), "{llm}");

        assert!(app.contains("engine line"), "{app}");
        assert!(
            !app.contains("policy audit") && !app.contains("llm retry"),
            "{app}"
        );
    }

    #[test]
    fn errors_and_warnings_also_land_in_error_log() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("logs");
        let (subscriber, guards) = build(&cfg("info", true), &Sink::File { dir: dir.clone() })
            .map(|(s, g, _)| (s, g))
            .unwrap();

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "zlogic::engine", "engine info");
            tracing::warn!(target: "zlogic::llm::retry", "llm slow");
            tracing::error!(target: "zlogic::policy", "policy denied");
            tracing::error!(target: "zlogic::daemon::http", "http failed");
        });
        drop(guards);

        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let day_dir = dir.join(&today);
        let error = std::fs::read_to_string(day_dir.join("error.log")).unwrap();

        assert!(error.contains("llm slow"), "{error}");
        assert!(error.contains("policy denied"), "{error}");
        assert!(error.contains("http failed"), "{error}");
        assert!(
            !error.contains("engine info"),
            "info is not a problem: {error}"
        );

        // The module files keep their errors — error.log is an extra copy, not a move.
        let llm = std::fs::read_to_string(day_dir.join("llm.log")).unwrap();
        let policy = std::fs::read_to_string(day_dir.join("policy.log")).unwrap();
        assert!(llm.contains("llm slow"), "{llm}");
        assert!(policy.contains("policy denied"), "{policy}");
    }

    #[test]
    fn a_dropped_field_never_reaches_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("logs");
        let (subscriber, guards) = build(&cfg("info", true), &Sink::File { dir: dir.clone() })
            .map(|(s, g, _)| (s, g))
            .unwrap();

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(
                target: "zlogic::engine",
                text = "the user's bank password is hunter2",
                api_key = "sk-live-abcdef",
                session_id = "s-1",
                "tool started"
            );
        });
        drop(guards);

        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let app = std::fs::read_to_string(dir.join(&today).join("app.log")).unwrap();

        assert!(app.contains("tool started"), "{app}");
        assert!(app.contains("session_id"), "{app}");
        assert!(!app.contains("hunter2"), "{app}");
        assert!(!app.contains("sk-live-abcdef"), "{app}");
    }

    /// One grep has to return the turn, not one line of it. This also pins the error layer's span
    /// pass-through: a per-layer filter that rejected the INFO-level span made `error.log` lose the
    /// very `turn_id` an error line most needs.
    #[test]
    fn an_in_scope_span_prints_on_every_line() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("logs");
        let (subscriber, guards) = build(&cfg("info", true), &Sink::File { dir: dir.clone() })
            .map(|(s, g, _)| (s, g))
            .unwrap();

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("turn", turn_id = "t-7", session_id = "s-1");
            let _guard = span.enter();
            tracing::info!(target: "zlogic::engine", "first thing");
            tracing::error!(target: "zlogic::engine", "second thing");
        });
        drop(guards);

        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let day = dir.join(&today);
        let app = std::fs::read_to_string(day.join("app.log")).unwrap();
        let error = std::fs::read_to_string(day.join("error.log")).unwrap();

        for (name, text) in [("app", &app), ("error", &error)] {
            let matched = text
                .lines()
                .filter(|line| line.contains("turn{") && line.contains("turn_id="))
                .count();
            assert!(matched > 0, "{name} lost the span context:\n{text}");
        }
        assert!(
            app.contains("second thing"),
            "the error line is also in app.log"
        );
    }

    #[test]
    fn daily_writer_rolls_over_on_date_change() {
        use std::io::Write as _;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("logs");
        let mut writer = DailyModuleWriter::new(&root, "app");

        let day_a = chrono::Local::now().format("%Y-%m-%d").to_string();
        writeln!(writer, "first").unwrap();
        writer.flush().unwrap();
        assert!(root.join(&day_a).join("app.log").exists());

        let day_b = "2000-01-01".to_string();
        let yesterday_dir = root.join(&day_b);
        std::fs::create_dir_all(&yesterday_dir).unwrap();
        let old = OpenOptions::new()
            .create(true)
            .append(true)
            .open(yesterday_dir.join("app.log"))
            .unwrap();
        writer.current = Some((day_b.clone(), old));

        writeln!(writer, "second").unwrap();
        writer.flush().unwrap();

        assert!(
            root.join(&day_a).join("app.log").exists(),
            "the old date directory is kept"
        );
        assert!(
            root.join(&day_b).join("app.log").exists(),
            "crossing into a new day opens a new file instead of overwriting the old one"
        );
        let content = std::fs::read_to_string(root.join(&day_a).join("app.log")).unwrap();
        assert!(
            content.contains("first") && content.contains("second"),
            "{content}"
        );
    }
}
