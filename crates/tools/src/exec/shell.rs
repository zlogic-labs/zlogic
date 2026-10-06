//! `shell` — run one command and report what it printed.
//! # The caller does not pick the deadline
//! A call waits for the process to exit, and the budget it waits under comes from the command
//! itself: a quick check gets a minute, a test run ten, a build twenty. Asking the model for a
//! number before it knows how long the work takes produces bad numbers in both directions — too
//! short and it re-runs a suite that was about to finish, too long and one call holds the turn
//! for an hour. So it says *whether* it needs the result instead, and the tool works out the rest.
//! Two flags cover everything outside that:
//! - `wait: true` — the result is required before the turn can continue. The class budget does not
//!   apply; only a stall, a cancellation, or a very generous backstop ends it.
//! - `background: true` — the work outlives the call. The already-running child is handed to the
//!   process-wide task runtime, which never re-runs it.
//!
//! Neither happens on its own. A recognised server/watcher is refused unless it is asked for as
//! background work (waiting for `npm run dev` to exit would hang forever), and a slow command is
//! killed at its ceiling rather than quietly turned into a task: a call whose outcome flips
//! between "here is your result" and "here is a task id" cannot be relied on by the model calling
//! it.
//! # Silence is a different fact from slowness
//! A command that has printed nothing for ten minutes is not slow, it is stuck — waiting on a
//! port, a lock, or input that will never arrive. That deserves a different verdict from "this
//! exceeded its budget", because the two call for opposite responses: one wants a longer run, the
//! other wants a narrower command. So they are reported separately. Meanwhile the console is never
//! left blank: a call that is still running says how long it has been going and how long since its
//! last output, which is what separates "slow" from "wedged" for the person watching it.
//! # Output is bounded in memory but complete on disk
//! Streams are held in a [`HeadTail`] buffer, which keeps a bounded head and a **larger tail**.
//! The tail is where it matters: a failing test's assertion, a compiler's error summary and an
//! installer's conclusion are all at the end, and a plain head cut throws away exactly the part
//! somebody needed. The full transcript goes to the object store, so the UI can show all of it and
//! the model can read the omitted middle on demand.
//! # Credentials are removed from the child's environment
//! A model that writes `zlogic $OPENAI_API_KEY` gets nothing. This is not a permission decision — it
//! is a fact about what this process is willing to hand to a subprocess, and it holds regardless
//! of who approved the command.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::io::AsyncReadExt;
use zlogic_proctree::{Console, Tree};
use zlogic_protocol::llm::ToolDefinition;
use zlogic_protocol::stream::OutputStream;

use crate::{
    ObjectRole, ProcessRequest, Recovery, Result, SpawnedProcess, Tool, ToolCtx, ToolExecResult,
    ToolMeta, ToolRisk, parse_args,
};

use super::decode::StreamDecoder;

/// How long a command of each class may run before it is killed and reported.
/// The point of a per-class budget is that the common case needs no decision from the caller: a
/// test run gets ten minutes whether or not anyone thought to ask for ten minutes, so the failure
/// this replaces — a suite killed at thirty seconds and re-run four times — cannot happen. The
/// numbers are deliberately generous at the top: a budget that is too long costs one slow turn,
/// while one that is too short costs the whole test run plus every retry after it.
const QUICK_BUDGET: Duration = Duration::from_secs(60);
const TEST_BUDGET: Duration = Duration::from_secs(10 * 60);
const BUILD_BUDGET: Duration = Duration::from_secs(20 * 60);

/// Backstop for `wait: true`, which otherwise has no class budget.
/// The flag says the result is required, and killing the work at an arbitrary point hands back
/// the same failure the caller was trying to avoid. But "no deadline at all" means a command
/// waiting on a port nobody opened holds the turn until the user gives up on it, so this exists
/// to bound the damage rather than to be a budget anyone plans around.
const WAIT_BACKSTOP: Duration = Duration::from_secs(60 * 60);

/// No output at all for this long means the command is stuck, not slow.
///
/// Independent of the wall clock on purpose. A build that is compiling for nine minutes and prints
/// `Compiling…` throughout is working; a process that has said nothing for ten minutes is waiting
/// for something that is never going to arrive. Sharing one deadline cannot tell those apart, and
/// guessing wrong in the direction of "still working" is what produces the wedge this detects.
const STALL_LIMIT: Duration = Duration::from_secs(10 * 60);

/// How often a running call reports that it is still running.
/// A blank console is indistinguishable from a hang, so the wait is narrated. The line goes to the
/// UI and not to the model: it is true at every moment and worth nothing in context, whereas the
/// stall verdict below is a fact the model has to act on. Checked on the [`CHILD_EXIT_POLL`] tick
/// rather than on a timer of its own, so the real cadence is the poll rounded up to the interval.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(30);

/// How often a call that is still reading output checks whether its child has already exited.
/// A wrapper that spawns something long-lived and returns leaves pipes that only its descendant
/// holds. Reading them would never reach EOF, and the loop below can otherwise wait on a stream
/// that will never close. Once the direct child is gone and the streams have gone quiet, this
/// call has everything it is going to get.
const CHILD_EXIT_POLL: Duration = Duration::from_secs(1);

/// Per-stream head and tail budgets, in characters.
/// `stderr` gets less than `stdout` because it is usually the short, high-signal channel; both
/// favour the tail. The sum stays under a typical `max_result_chars` so the generic offload path
/// never re-truncates what has already been carefully trimmed and discards the tail.
const STDOUT_HEAD: usize = 6 * 1024;
const STDOUT_TAIL: usize = 12 * 1024;
const STDERR_HEAD: usize = 3 * 1024;
const STDERR_TAIL: usize = 6 * 1024;

#[derive(Debug, Deserialize)]
struct Args {
    command: String,
    /// Shown to the user in the permission prompt. Never used to build the command.
    #[allow(dead_code)]
    description: Option<String>,
    /// Defaults to the working directory.
    path: Option<String>,
    #[serde(default)]
    background: bool,
    /// Wait for the process to exit however long it takes, instead of killing it at the budget
    /// its class would get.
    #[serde(default)]
    wait: bool,
}

/// Syntax understood by the process that actually executes a shell call.
/// The permission adapter consumes this value directly; it must never infer a dialect from the
/// command text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellDialect {
    Posix,
    PowerShell,
    Cmd,
}

/// Requested backend before startup detection resolves it to an executable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellPreference {
    Auto,
    GitBash,
    Ps7,
    Powershell,
    Cmd,
    Bash,
}

#[derive(Debug, Clone)]
struct ShellBackend {
    program: PathBuf,
    launch_args: &'static [&'static str],
    label: &'static str,
    syntax: &'static str,
    dialect: ShellDialect,
}

#[derive(Debug, Clone)]
pub struct Shell {
    backend: ShellBackend,
    /// How long each class of command is allowed to run.
    budgets: ShellBudgets,
}

/// The time limits [`Shell`] applies.
///
/// Public because it is configuration, not an internal detail: the engine reads `tools.shell` from
/// the global config and from a workspace's `.zlogic/settings.yaml` and hands the merged result
/// back through [`Shell::with_budgets`]. Kept as its own type rather than read from the config
/// inside the tool so that the tool never has to know where configuration comes from, and so a
/// test can reach the kill paths with millisecond limits.
#[derive(Debug, Clone, Copy)]
pub struct ShellBudgets {
    pub quick: Duration,
    pub test: Duration,
    pub build: Duration,
    /// Ceiling for `wait: true`, which has no class budget of its own.
    pub waiting: Duration,
    /// No output for this long is a stall, whatever the wall clock says.
    pub stall: Duration,
    /// How often a running call narrates itself to the UI.
    pub progress: Duration,
}

impl Default for ShellBudgets {
    fn default() -> Self {
        Self {
            quick: QUICK_BUDGET,
            test: TEST_BUDGET,
            build: BUILD_BUDGET,
            waiting: WAIT_BACKSTOP,
            stall: STALL_LIMIT,
            progress: PROGRESS_INTERVAL,
        }
    }
}

impl From<&zlogic_protocol::settings::ShellConfig> for ShellBudgets {
    fn from(cfg: &zlogic_protocol::settings::ShellConfig) -> Self {
        let s = Duration::from_secs;
        Self {
            quick: s(cfg.quick_secs),
            test: s(cfg.test_secs),
            build: s(cfg.build_secs),
            waiting: s(cfg.wait_secs),
            stall: s(cfg.stall_secs),
            progress: s(cfg.progress_secs),
        }
    }
}

impl Default for Shell {
    fn default() -> Self {
        // Profile off, unlike `ShellConfig::default()`. This value is the unconfigured test
        // fixture, and a suite that inherits the developer's `~/.bash_profile` is a suite that
        // fails on someone else's machine. Bootstrap is the only production path and it passes
        // the configured flag.
        Self::resolve(ShellPreference::Auto, false)
            // The unconfigured registry is mainly used by tests. Production bootstrap resolves
            // the configured backend and replaces this instance; retaining a platform-shaped
            // definition here is better than making every generic registry constructor fallible.
            .unwrap_or_else(|_| Self::unchecked_platform_default(false))
    }
}

/// The arguments a POSIX shell is launched with.
///
/// `-l` rather than simply dropping both switches: a login shell is what reads `/etc/profile` and
/// `~/.bash_profile`, and on Git Bash `/etc/profile` is also where the Windows-side `PATH` gets
/// translated — `--noprofile --norc` leaves it in the form the parent process handed over.
fn posix_args(read_profile: bool) -> &'static [&'static str] {
    if read_profile {
        &["-l", "-c"]
    } else {
        &["--noprofile", "--norc", "-c"]
    }
}

fn powershell_args(read_profile: bool) -> &'static [&'static str] {
    if read_profile {
        &["-NoLogo", "-NonInteractive", "-Command"]
    } else {
        &["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"]
    }
}

/// `/d` is what suppresses cmd's AutoRun commands, which come from the registry and from
/// `HKCU\...\Command Processor\AutoRun` — a hook that outlives the profile file it was set next to.
fn cmd_args(read_profile: bool) -> &'static [&'static str] {
    if read_profile {
        &["/s", "/c"]
    } else {
        &["/d", "/s", "/c"]
    }
}

impl Shell {
    /// Resolves a preference once at startup. Explicit preferences never fall back to another
    /// shell: doing that would make the prompt, policy dialect and actual process disagree.
    pub fn resolve(
        preference: ShellPreference,
        read_profile: bool,
    ) -> std::result::Result<Self, String> {
        let backend = match preference {
            ShellPreference::Auto => detect_default_backend(read_profile)?,
            ShellPreference::GitBash => git_bash_backend(read_profile).ok_or_else(|| {
                "git_bash is configured, but Git for Windows' bash.exe was not found".to_string()
            })?,
            ShellPreference::Ps7 => named_backend(
                "ps7",
                &["pwsh.exe", "pwsh"],
                powershell_args(read_profile),
                "PowerShell 7",
                "PowerShell",
                ShellDialect::PowerShell,
            )?,
            ShellPreference::Powershell => named_backend(
                "powershell",
                &["powershell.exe", "powershell"],
                powershell_args(read_profile),
                "Windows PowerShell",
                "PowerShell",
                ShellDialect::PowerShell,
            )?,
            ShellPreference::Cmd => named_backend(
                "cmd",
                &["cmd.exe", "cmd"],
                cmd_args(read_profile),
                "Command Prompt",
                "cmd.exe",
                ShellDialect::Cmd,
            )?,
            ShellPreference::Bash => named_backend(
                "bash",
                &["bash.exe", "bash"],
                posix_args(read_profile),
                "Bash",
                "POSIX shell",
                ShellDialect::Posix,
            )?,
        };
        Ok(Self {
            backend,
            budgets: ShellBudgets::default(),
        })
    }

    pub fn dialect(&self) -> ShellDialect {
        self.backend.dialect
    }

    pub fn backend_name(&self) -> &'static str {
        self.backend.label
    }

    fn unchecked_platform_default(read_profile: bool) -> Self {
        #[cfg(windows)]
        let backend = ShellBackend {
            program: PathBuf::from("cmd.exe"),
            launch_args: cmd_args(read_profile),
            label: "Command Prompt",
            syntax: "cmd.exe",
            dialect: ShellDialect::Cmd,
        };
        #[cfg(not(windows))]
        let backend = ShellBackend {
            program: PathBuf::from("bash"),
            launch_args: posix_args(read_profile),
            label: "Bash",
            syntax: "POSIX shell",
            dialect: ShellDialect::Posix,
        };
        Self {
            backend,
            budgets: ShellBudgets::default(),
        }
    }

    /// The same shell under different time limits.
    ///
    /// Takes `&self` rather than being built from a preference so the backend is not re-probed:
    /// a workspace that changes one budget must not be able to end up launching a different shell
    /// than the one its definition, its policy dialect and its sibling tools all refer to.
    pub fn with_budgets(&self, budgets: ShellBudgets) -> Self {
        Self {
            backend: self.backend.clone(),
            budgets,
        }
    }

    pub fn budgets(&self) -> ShellBudgets {
        self.budgets
    }
}

fn detect_default_backend(read_profile: bool) -> std::result::Result<ShellBackend, String> {
    #[cfg(windows)]
    {
        if let Some(backend) = git_bash_backend(read_profile) {
            return Ok(backend);
        }
        if let Ok(backend) = named_backend(
            "ps7",
            &["pwsh.exe", "pwsh"],
            powershell_args(read_profile),
            "PowerShell 7",
            "PowerShell",
            ShellDialect::PowerShell,
        ) {
            return Ok(backend);
        }
        if let Ok(backend) = named_backend(
            "powershell",
            &["powershell.exe", "powershell"],
            powershell_args(read_profile),
            "Windows PowerShell",
            "PowerShell",
            ShellDialect::PowerShell,
        ) {
            return Ok(backend);
        }
        return named_backend(
            "cmd",
            &["cmd.exe", "cmd"],
            cmd_args(read_profile),
            "Command Prompt",
            "cmd.exe",
            ShellDialect::Cmd,
        );
    }
    #[cfg(not(windows))]
    {
        named_backend(
            "bash",
            &["bash"],
            posix_args(read_profile),
            "Bash",
            "POSIX shell",
            ShellDialect::Posix,
        )
    }
}

fn named_backend(
    config_name: &str,
    executables: &[&str],
    launch_args: &'static [&'static str],
    label: &'static str,
    syntax: &'static str,
    dialect: ShellDialect,
) -> std::result::Result<ShellBackend, String> {
    let program = executables
        .iter()
        .find_map(|name| find_on_path(name))
        .or_else(|| known_windows_program(config_name))
        .ok_or_else(|| {
            format!("{config_name} is configured, but no matching executable was found at startup")
        })?;
    Ok(ShellBackend {
        program,
        launch_args,
        label,
        syntax,
        dialect,
    })
}

#[cfg(windows)]
fn git_bash_backend(read_profile: bool) -> Option<ShellBackend> {
    Some(ShellBackend {
        program: git_bash_program()?,
        launch_args: posix_args(read_profile),
        label: "Git Bash",
        syntax: "POSIX shell",
        dialect: ShellDialect::Posix,
    })
}

/// Git for Windows ships two `bash.exe`. `usr\bin\bash.exe` is the 2.4 MB shell; `bin\bash.exe`
/// is a 47 KB launcher whose only job is to re-execute the first one. That re-execution is not
/// free: it drops the console the parent was handed and asks for a new one, so a spawn that
/// correctly asked for no window still puts a terminal window on screen — once per `shell` call,
/// on any machine whose default console host is Windows Terminal. The shell itself is the target,
/// and the launcher stays only as a fallback for a layout with no `usr\bin`.
#[cfg(windows)]
pub fn git_bash_program() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    for key in ["ProgramFiles", "ProgramFiles(x86)", "LocalAppData"] {
        if let Some(base) = std::env::var_os(key) {
            let base = PathBuf::from(base);
            push_git_bash_candidates(&mut candidates, &base.join("Git"));
            push_git_bash_candidates(&mut candidates, &base.join("Programs").join("Git"));
        }
    }
    if let Some(git) = find_on_path("git.exe").or_else(|| find_on_path("git"))
        && let Some(parent) = git.parent()
    {
        // `git.exe` sits in `<root>\cmd` or `<root>\bin`, so its grandparent is the root either way.
        if let Some(root) = parent.parent() {
            push_git_bash_candidates(&mut candidates, root);
        }
        candidates.push(parent.join("bash.exe"));
    }
    candidates.into_iter().find(|path| path.is_file())
}

#[cfg(windows)]
fn push_git_bash_candidates(candidates: &mut Vec<PathBuf>, root: &Path) {
    candidates.push(root.join("usr").join("bin").join("bash.exe"));
    candidates.push(root.join("bin").join("bash.exe"));
}

#[cfg(windows)]
fn known_windows_program(config_name: &str) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    match config_name {
        "ps7" => {
            if let Some(base) = std::env::var_os("ProgramFiles") {
                candidates.push(PathBuf::from(base).join("PowerShell/7/pwsh.exe"));
            }
        }
        "powershell" => {
            if let Some(root) = std::env::var_os("SystemRoot") {
                candidates.push(
                    PathBuf::from(root).join("System32/WindowsPowerShell/v1.0/powershell.exe"),
                );
            }
        }
        "cmd" => {
            if let Some(comspec) = std::env::var_os("ComSpec") {
                candidates.push(PathBuf::from(comspec));
            }
            if let Some(root) = std::env::var_os("SystemRoot") {
                candidates.push(PathBuf::from(root).join("System32/cmd.exe"));
            }
        }
        _ => {}
    }
    candidates.into_iter().find(|path| path.is_file())
}

#[cfg(not(windows))]
fn known_windows_program(_config_name: &str) -> Option<PathBuf> {
    None
}

#[cfg(not(windows))]
fn git_bash_backend() -> Option<ShellBackend> {
    None
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let candidate = Path::new(name);
    if candidate.components().count() > 1 {
        return candidate.is_file().then(|| candidate.to_path_buf());
    }
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

#[async_trait]
impl Tool for Shell {
    fn meta(&self) -> ToolMeta {
        // High unconditionally. This is a *signal*, not a verdict: most calls are `git status` or
        // a test run, and the approval pipeline is what decides — a rule that refused on static
        // risk alone would short-circuit every safe command straight to the user.
        ToolMeta {
            name: "shell".into(),
            source: "builtin",
            risk: ToolRisk::High,
        }
    }

    fn definition(&self) -> ToolDefinition {
        let budgets = self.budgets;
        ToolDefinition {
            name: "shell".into(),
            description: format!(
                "Run a {syntax} command with {backend}. Returns its exit status, stdout and \
                 stderr. The call waits for the command to exit, and the time it is allowed to \
                 take comes from the command itself — up to {quick} for a quick check, {test} for \
                 a test run, {build} for a build or install — so a test suite is not cut off at \
                 the same budget as `git status`. There is no timeout argument: you say whether \
                 you need the result, and the tool decides how long that takes. Pass `wait: true` \
                 when the result is something you must have before you can continue and the \
                 budget might not be enough; the call then runs until the command exits, until it \
                 goes silent for {stall}, or until the conversation is cancelled. Pass \
                 `background: true` for work that outlives the call — servers, watchers, a build \
                 you want to run while you do something else: it returns a task id immediately and \
                 the process keeps running. Commands that never exit are refused unless \
                 `background: true` is given. A running call reports its progress, and a command \
                 that stops producing output is reported as stuck rather than as slow. When a \
                 background task comes from a command that terminates (compile, test), the turn \
                 waits up to the configured budget for it before ending, so the result can land \
                 in the same reply; servers and watchers are never waited on. Filter large output \
                 at the source with grep or head; `tail` is not one of them, because it prints \
                 nothing until its input ends and so silences the live log for the whole run. \
                 Prefer the file tools for filesystem changes because they report the changed \
                 paths.",
                quick = elapsed(budgets.quick),
                test = elapsed(budgets.test),
                build = elapsed(budgets.build),
                stall = elapsed(budgets.stall),
                syntax = self.backend.syntax,
                backend = self.backend.label,
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "minLength": 1,
                        "description": format!(
                            "A {} command, executed by {}",
                            self.backend.syntax,
                            self.backend.label,
                        )
                    },
                    "description": { "type": "string", "description": "One sentence shown to the user when asking for permission" },
                    "path": { "type": "string", "description": "Directory to run in; defaults to the working directory" },
                    "wait": {
                        "type": "boolean",
                        "default": false,
                        "description": "Run until the process exits rather than at the budget its \
                                        command would get, and return its result in this call. For \
                                        anything whose result you need and whose runtime you cannot \
                                        predict — a long test suite, a cold build"
                    },
                    "background": {
                        "type": "boolean",
                        "default": false,
                        "description": "Start as a durable background task and return its task id immediately"
                    },
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, ctx: &ToolCtx, args: &str) -> Result<ToolExecResult> {
        let a: Args = parse_args(args)?;
        let command = a.command.trim().to_string();
        if command.is_empty() {
            return Ok(ToolExecResult::failed("command is required"));
        }
        if is_shell_backgrounded(&command) {
            return Ok(ToolExecResult::failed(
                "Do not append `&` to detach a process from zlogic; use background: true so the \
                 process gets a task id, durable output, and can be stopped.",
            ));
        }
        if a.wait && a.background {
            return Ok(ToolExecResult::failed(
                "wait: true and background: true contradict each other: waiting means this call \
                 returns with the result, background means it returns before there is one. Pass \
                 one of them.",
            ));
        }
        // A command that never exits is refused rather than started, whatever else was asked for:
        // with `wait` it would hold the call for the backstop, and without it the call would be
        // killed at its budget having reported nothing useful. Background is the one answer that
        // works.
        if let Some(hint) = looks_long_running(&command)
            && !a.background
        {
            return Ok(ToolExecResult::failed(if ctx.tasks.is_some() {
                format!(
                    "{command:?} looks like it never exits ({hint}). shell waits for the command \
                     to finish, so this call would hang. Pass background: true: the process then \
                     gets a task id, durable output, and can be stopped."
                )
            } else {
                format!(
                    "{command:?} looks like it never exits ({hint}). shell waits for the command \
                     to finish and this deployment has no background task runtime. Ask the user to \
                     run it in their own terminal, or run a one-shot equivalent."
                )
            }));
        }

        let cwd = match &a.path {
            Some(p) => ctx.resolve_path(p).path,
            None => ctx.exec_cwd.clone(),
        };
        let class = classify(&command);
        // `wait: true` replaces the class budget rather than adding to it. The class is what says
        // how long this kind of work takes; a caller that knows it needs the result anyway is
        // saying the estimate does not apply here, and the backstop bounds the mistake.
        let budget = if a.wait {
            self.budgets.waiting
        } else {
            class.budget(self.budgets)
        };
        let stall_limit = self.budgets.stall;
        let progress_interval = self.budgets.progress;
        ctx.progress(&format!(
            "running · {label} · up to {budget}\n",
            label = class.label(),
            budget = elapsed(budget),
        ));
        let deadline = tokio::time::Instant::now() + budget;

        let mut process = tokio::process::Command::new(&self.backend.program);
        process
            .args(self.backend.launch_args)
            .arg(&command)
            .current_dir(&cwd)
            // Cleared and rebuilt, not amended: `env_remove` per name would need the names in
            // advance, and the point is to filter by *shape*.
            .env_clear()
            .envs(child_env(&ctx.runtime_paths, &ctx.env_pairs()))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // cmd.exe / pwsh / powershell / bash are console apps: from a GUI host (desktop) a plain
        // spawn would open a console window for every shell call, so the console is suppressed.
        // A shell wrapper also commonly launches the real server/test process as a child, which
        // is why this is a `Tree` and not a bare `Child`: the wrapper, its descendants, and — on
        // Windows, where the job object is closed by the kernel when zlogic exits — anything
        // either of them goes on to spawn all die with this call.
        let mut child = match Tree::spawn(process, Console::Hidden) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolExecResult::failed(format!(
                    "cannot run {} ({}): {e}",
                    self.backend.label,
                    self.backend.program.display()
                )));
            }
        };

        let mut stdout = child.take_stdout().expect("stdout was piped");
        let mut stderr = child.take_stderr().expect("stderr was piped");
        if a.background {
            return adopt_process(self, ctx, command, cwd, child, stdout, stderr).await;
        }

        let mut out = HeadTail::new(STDOUT_HEAD, STDOUT_TAIL);
        let mut err = HeadTail::new(STDERR_HEAD, STDERR_TAIL);
        // The full transcript, interleaved in arrival order — which is how a person read it in
        // their terminal, and is not reconstructible from the two separated streams.
        let mut transcript = String::new();

        let mut out_buf = [0u8; 8192];
        let mut err_buf = [0u8; 8192];
        let mut out_open = true;
        let mut err_open = true;
        // What decides the encoding is a window, not the first byte that fails — see `decode`.
        // Each stream gets its own, because one pipe saying GBK says nothing about the other.
        let mut out_dec = StreamDecoder::new();
        let mut err_dec = StreamDecoder::new();
        // Deliberately *not* the instant `deadline` was derived from: the budget covers the spawn
        // (a spawn that never returns must still end the call), but the stall clock and the
        // reported runtime start here, because before this point the command had no chance to say
        // anything and calling that silence would be wrong.
        let started = tokio::time::Instant::now();
        let mut ending: Option<Ending> = None;
        // When the last byte of output arrived. Reset on every chunk from either stream, because
        // the question the stall timer asks is "has this said anything lately", not "has it ever
        // said anything" — a test suite that reports each case is visibly alive even in a long
        // quiet stretch between them.
        let mut last_output = started;
        // When the last progress line went out. The UI needs to be told the call is alive well
        // before the stall limit, not at it.
        let mut last_progress = started;
        // Set when the loop stopped because the child was gone while its pipes were not: a
        // descendant is still holding them, and anything it prints from here is not this call's
        // output. Reported rather than silently dropped.
        let mut detached_output = false;

        // Read both pipes as output arrives, so a long command shows progress instead of a blank
        // screen, and so neither pipe can fill and deadlock the child.
        while out_open || err_open {
            // An undecided stream is holding bytes it needs either more of or a quiet moment to
            // judge. Three lines of error text never fill a window, so the idle timer is what
            // lets them through; without it a command that printed its banner and is now waiting
            // would show nothing until it exited.
            let probe_at = [out_dec.probe_deadline(), err_dec.probe_deadline()]
                .into_iter()
                .flatten()
                .min();
            tokio::select! {
                n = stdout.read(&mut out_buf), if out_open => match n {
                    Ok(0) | Err(_) => out_open = false,
                    Ok(n) => {
                        let chunk = out_dec.push(&out_buf[..n], tokio::time::Instant::now());
                        publish(&chunk, OutputStream::Stdout, ctx, &mut transcript, &mut out);
                        last_output = tokio::time::Instant::now();
                    }
                },
                n = stderr.read(&mut err_buf), if err_open => match n {
                    Ok(0) | Err(_) => err_open = false,
                    Ok(n) => {
                        let chunk = err_dec.push(&err_buf[..n], tokio::time::Instant::now());
                        publish(&chunk, OutputStream::Stderr, ctx, &mut transcript, &mut err);
                        last_output = tokio::time::Instant::now();
                    }
                },
                _ = tokio::time::sleep_until(probe_at.unwrap_or(started)), if probe_at.is_some() => {
                    let now = tokio::time::Instant::now();
                    let a = out_dec.flush_if_idle(now);
                    let b = err_dec.flush_if_idle(now);
                    publish(&a, OutputStream::Stdout, ctx, &mut transcript, &mut out);
                    publish(&b, OutputStream::Stderr, ctx, &mut transcript, &mut err);
                },
                // Terminate, then kill. A process that ignores the signal must not be left
                // running: by the time this returns it is dead, and the model's recourse is
                // `wait: true` or handing the job to the background.
                _ = tokio::time::sleep_until(deadline) => {
                    ending = Some(Ending::TimedOut);
                    child.terminate().await;
                    break;
                }
                // Nothing at all for the stall limit. Separate from the wall clock because the
                // two mean opposite things to whoever reads the result: a command that ran out of
                // budget is slow and wants a longer run, and one that has been silent for ten
                // minutes is waiting for something that is never coming — a port, a lock, input
                // this call cannot give it — and wants a narrower command instead. Reporting the
                // second as the first sends the model off to re-run the thing that is already
                // wedged.
                _ = tokio::time::sleep_until(last_output + stall_limit) => {
                    ending = Some(Ending::Stalled);
                    child.terminate().await;
                    break;
                }
                // Cancellation reaches the child, not just the loop. Output already collected is
                // kept and reported: a killed build's first error is still the answer.
                _ = ctx.cancel.cancelled() => {
                    ending = Some(Ending::Cancelled);
                    child.terminate().await;
                    break;
                }
                // Ticks every second. Carries the two things that are checked rather than
                // awaited — the child-exit poll, which cannot be a deadline because the condition
                // it guards against has no time bound, and the progress line.
                _ = tokio::time::sleep(CHILD_EXIT_POLL) => {
                    let now = tokio::time::Instant::now();
                    if matches!(child.try_wait(), Ok(Some(_))) {
                        detached_output = true;
                        break;
                    }
                    if now.duration_since(last_progress) >= progress_interval {
                        last_progress = now;
                        // How long it has been going, and how long since it last said anything.
                        // The second number is the one that matters: a gap that keeps growing is
                        // what a person watching a blank console cannot see for themselves.
                        ctx.progress(&format!(
                            "still running · {} elapsed · {} since the last output\n",
                            elapsed(now.duration_since(started)),
                            elapsed(now.duration_since(last_output)),
                        ));
                    }
                }
            }
        }

        // The loop can end with a decoder still holding bytes — a window that never filled, or
        // the tail of a character split across two reads — and what it holds is the last thing
        // the command said.
        let out_tail = out_dec.flush();
        publish(
            &out_tail,
            OutputStream::Stdout,
            ctx,
            &mut transcript,
            &mut out,
        );
        let err_tail = err_dec.flush();
        publish(
            &err_tail,
            OutputStream::Stderr,
            ctx,
            &mut transcript,
            &mut err,
        );

        let status = child.wait().await;
        let ran_for = tokio::time::Instant::now().duration_since(started);
        let (status_line, failed) = match (&ending, &status) {
            (Some(Ending::TimedOut), _) => (
                format!("ran out of time after {} and was killed", elapsed(budget)),
                true,
            ),
            (Some(Ending::Stalled), _) => (
                format!(
                    "printed nothing for {} and was killed — it was waiting for something, not \
                     working",
                    elapsed(ran_for)
                ),
                true,
            ),
            (Some(Ending::Cancelled), _) => ("interrupted".to_string(), true),
            (None, Ok(s)) if s.success() => ("exit 0".to_string(), false),
            (None, Ok(s)) => (
                match s.code() {
                    Some(c) => format!("exit {c}"),
                    // No code means a signal killed it, which is worth saying rather than
                    // reporting a confusing "exit ?".
                    None => "killed by a signal".to_string(),
                },
                true,
            ),
            (None, Err(e)) => (format!("could not be waited for: {e}"), true),
        };

        let mut body = format!(
            "command: {}\ncwd: {}\nstatus: {status_line}\nran for: {}\n",
            summarize_command(&command),
            cwd.display(),
            elapsed(ran_for),
        );
        let omitted = out.omitted() + err.omitted();
        if omitted > 0 {
            body.push_str(&format!(
                "[{omitted} characters omitted from the middle; head and tail kept]\n"
            ));
        }
        body.push_str(&match out.render() {
            s if s.is_empty() => "\n--- stdout (empty) ---\n".to_string(),
            s => format!("\n--- stdout ({} chars) ---\n{s}\n", out.total),
        });
        if err.total > 0 {
            body.push_str(&format!(
                "\n--- stderr ({} chars) ---\n{}\n",
                err.total,
                err.render()
            ));
        }
        match &ending {
            Some(Ending::TimedOut) => {
                body.push_str(&format!("\n{}", timeout_guidance(a.wait, class)));
            }
            Some(Ending::Stalled) => {
                body.push_str(&format!("\n{}", stall_guidance(class)));
            }
            _ => {}
        }
        if detached_output {
            body.push_str(
                "\n--- output may be incomplete ---\n\
                 - The command exited, but a process it started still holds its output stream, so \
                 this call stopped reading rather than wait for an EOF that will never come.\n\
                 - That leftover process was stopped along with the rest of the command's tree; \
                 anything printed after this point is not in the result above.",
            );
        }
        if let Some(guidance) = not_found_guidance(
            &command,
            &err.render(),
            status.as_ref().ok().and_then(|s| s.code()),
        ) {
            body.push_str(&format!("\n{guidance}"));
        }
        // Only when it worked: after a failed `git worktree add` there is nothing to say anything
        // about, and the note would describe a checkout that does not exist.
        if !failed && let Some(note) = worktree_note(&command) {
            body.push_str(&format!("\n{note}"));
        }

        let mut result = if matches!(ending, Some(Ending::Cancelled)) {
            ToolExecResult::cancelled(body)
        } else if matches!(ending, Some(Ending::TimedOut) | Some(Ending::Stalled)) {
            // Both are `Timeout`: a call that ran out of room and one that wedged are the same
            // fact to an audit — it did not finish. The distinction the model acts on is in the
            // body, and it is a large one.
            ToolExecResult::timeout(body)
        } else if failed {
            // A non-zero exit reaches the model as an error, so it does not build on a failed
            // step. The output is still there — that is what it needs in order to fix it.
            ToolExecResult::failed(body)
        } else {
            ToolExecResult::success(body)
        };

        // The full transcript is a reference, never part of the context: the head and tail above
        // are what the model reads. Empty output stores nothing rather than an empty object.
        if !transcript.is_empty() {
            let cap = ctx.capture(
                ObjectRole::Output,
                None,
                &transcript,
                Recovery::FilterAtSource,
            )?;
            result = result.with_display(cap.display).with_object(cap.object);
        }
        Ok(result)
    }
}

enum Ending {
    TimedOut,
    /// Alive, but silent past the point where silence means it is waiting rather than working.
    Stalled,
    Cancelled,
}

#[allow(clippy::too_many_arguments)]
async fn adopt_process(
    shell: &Shell,
    ctx: &ToolCtx,
    command: String,
    cwd: PathBuf,
    mut child: Tree,
    stdout: tokio::process::ChildStdout,
    stderr: tokio::process::ChildStderr,
) -> Result<ToolExecResult> {
    let Some(tasks) = &ctx.tasks else {
        child.terminate().await;
        return Ok(ToolExecResult::failed(
            "background was requested, but this deployment has no background task runtime",
        ));
    };
    let args = shell
        .backend
        .launch_args
        .iter()
        .map(|arg| (*arg).to_string())
        .chain(std::iter::once(command.clone()))
        .collect();
    let request = ProcessRequest {
        spec: zlogic_task::ProcessSpec {
            program: shell.backend.program.display().to_string(),
            args,
            cwd: Some(cwd.display().to_string()),
            // The child already has the scrubbed environment. Persisting a resolved environment
            // would both duplicate host state and risk retaining values that should not be durable.
            env: BTreeMap::new(),
        },
        parent_session_id: ctx.session_id,
        parent_turn_id: ctx.turn_id,
        anchor_call_id: ctx.call_id.clone(),
        cancel: ctx.cancel.clone(),
        turn_scoped: looks_long_running(&command).is_none(),
    };
    let task_id = tasks
        .start_process(
            request,
            SpawnedProcess {
                child,
                stdout,
                stderr,
            },
        )
        .await
        .map_err(crate::ToolError::Failed)?;
    let mut result = ToolExecResult::success(format!(
        "Command started as background task {task_id}. Its completion notification — including \
         an output preview — arrives automatically; keep working and do not poll. task_get reads \
         its state and output, task_stop stops it."
    ));
    result = result.with_display(crate::ToolDisplay::Task {
        task_id: task_id.to_string(),
        command: Some(summarize_command(&command)),
    });
    Ok(result)
}

fn summarize_command(command: &str) -> String {
    const MAX_CHARS: usize = 240;
    let one_line = command.lines().next().unwrap_or("").trim();
    let total = command.chars().count();
    let mut summary: String = one_line.chars().take(MAX_CHARS).collect();
    if total > summary.chars().count() || command.contains('\n') {
        summary.push_str(&format!(" … ({total} characters total)"));
    }
    summary
}

/// Hands a piece of output to everywhere it has to reach: the live console, the interleaved
/// transcript, and the bounded head/tail the model reads. One function for all three because a
/// piece that reached two of them is worse than one that reached none — the console and the
/// result would disagree about what the command printed.
fn publish(
    text: &str,
    stream: OutputStream,
    ctx: &ToolCtx,
    transcript: &mut String,
    sink: &mut HeadTail,
) {
    if text.is_empty() {
        return;
    }
    ctx.emit(stream, text);
    transcript.push_str(text);
    sink.push(text);
}

/// Bounded accumulator that keeps the head and a larger tail while counting everything.
struct HeadTail {
    head: String,
    tail: String,
    head_cap: usize,
    tail_cap: usize,
    total: usize,
    /// Whether the stream has any line structure at all. Tracked rather than inferred from the
    /// retained pieces — see [`Self::aligned`].
    saw_newline: bool,
}

impl HeadTail {
    fn new(head_cap: usize, tail_cap: usize) -> Self {
        Self {
            head: String::new(),
            tail: String::new(),
            head_cap,
            tail_cap,
            total: 0,
            saw_newline: false,
        }
    }

    fn push(&mut self, piece: &str) {
        self.total += piece.chars().count();
        self.saw_newline |= piece.contains('\n');
        let mut rest = piece;
        if self.head.chars().count() < self.head_cap {
            let room = self.head_cap - self.head.chars().count();
            // Split on a character boundary: a byte slice through a multi-byte character would
            // put replacement characters into output that was perfectly valid.
            let take: String = rest.chars().take(room).collect();
            self.head.push_str(&take);
            rest = &rest[take.len()..];
        }
        if rest.is_empty() {
            return;
        }
        self.tail.push_str(rest);
        let over = self.tail.chars().count().saturating_sub(self.tail_cap);
        if over > 0 {
            self.tail = self.tail.chars().skip(over).collect();
        }
    }

    /// The head and tail as they will actually be shown, trimmed to line boundaries.
    /// Alignment happens **here** rather than during accumulation: a stream arrives in arbitrary
    /// chunks, so keeping the buffers line-aligned as they fill would mean holding a partial line
    /// aside on every push. Doing it once at the end is the same result for less machinery.
    /// The head loses its trailing partial line and the tail its leading one. Both are fragments of
    /// a line whose other half is in the omitted middle, and a fragment of a log line — half a
    /// stack frame, half a file path — reads as a fact that is not true.
    /// Two cases keep an unaligned piece, and they are decided by [`Self::saw_newline`] rather than
    /// by whether *this piece* happens to contain one:
    /// - the stream has no line structure at all (a progress bar rewriting itself with `\r`, a
    ///   single-line JSON response) — there is nothing to preserve;
    /// - alignment would leave both pieces empty, because the budget is smaller than a single line.
    /// Inferring the first from "the head contains no newline" would be wrong: with a head budget
    /// below the length of the first line, a perfectly line-structured log would look structureless
    /// and be shown as a fragment.
    fn aligned(&self) -> (&str, &str) {
        if !self.saw_newline {
            return (&self.head, &self.tail);
        }
        let head = match self.head.rfind('\n') {
            Some(i) => &self.head[..=i],
            None => "",
        };
        let tail = match self.tail.find('\n') {
            Some(i) => &self.tail[i + 1..],
            None => "",
        };
        // Showing a fragment beats showing nothing.
        if head.is_empty() && tail.is_empty() {
            return (&self.head, &self.tail);
        }
        (head, tail)
    }

    fn omitted(&self) -> usize {
        let (head, tail) = self.aligned();
        self.total
            .saturating_sub(head.chars().count() + tail.chars().count())
    }

    fn render(&self) -> String {
        let (head, tail) = self.aligned();
        let omitted = self.omitted();
        if omitted == 0 {
            return format!("{head}{tail}");
        }
        format!(
            "{head}\n… {omitted} characters omitted of {} total; whole lines from the start and \
             the end are shown. To see the middle, re-run with the filtering at the source — pipe \
             through grep or head, not tail …\n{tail}",
            self.total
        )
    }
}

/// Recognises commands that are not meant to exit.
/// Deliberately narrow: a false positive refuses legitimate work, which is worse than the timeout
/// it was trying to prevent. Returns the reason, so the refusal can say *why* it thinks so and
/// point at `background: true`.
fn looks_long_running(command: &str) -> Option<&'static str> {
    let c = command.to_ascii_lowercase();
    const PATTERNS: &[(&[&str], &str)] = &[
        (
            &[
                "npm run dev",
                "npm start",
                "pnpm dev",
                "yarn dev",
                "bun dev",
            ],
            "a package script that serves",
        ),
        (
            &[
                "next dev",
                "next start",
                "nuxt dev",
                "vite dev",
                "astro dev",
                "remix dev",
            ],
            "a framework dev server",
        ),
        (
            &[
                "nodemon",
                "webpack serve",
                "webpack-dev-server",
                "live-server",
                "http-server",
            ],
            "a watcher or static server",
        ),
        (
            &[
                "flask run",
                "uvicorn",
                "gunicorn",
                "hypercorn",
                "daphne",
                "manage.py runserver",
            ],
            "a Python application server",
        ),
        (
            &["rails server", "rails s ", "php -s", "caddy run"],
            "an application server",
        ),
        (
            &["bootrun", "spring-boot:run", "quarkus:dev"],
            "a JVM dev server",
        ),
        (
            &["python -m http.server", "python3 -m http.server"],
            "a static server",
        ),
        (
            &["--watch", "-w --", "tail -f", "tail --follow"],
            "a watch or follow mode",
        ),
    ];
    for (needles, why) in PATTERNS {
        if needles.iter().any(|n| c.contains(n)) {
            return Some(why);
        }
    }
    // `vite` on its own serves; `vite build` does not. Token-compared rather than substring-matched:
    // `vitest run` **contains** "vite", and treating the test runner as a dev server is how a
    // perfectly ordinary test command got refused.
    if let Some(index) = words(&c).position(|word| word == "vite" || word.ends_with("/vite")) {
        let next = words(&c).nth(index + 1);
        if next != Some("build") {
            return Some("a framework dev server");
        }
    }
    None
}

/// Whitespace-separated words. Enough for the program-name comparisons above, which never span
/// spaces — the multi-word needles stay substring-matched because a command line is not a shell
/// parse and pretending otherwise is how a heuristic like this grows bugs.
fn words(command: &str) -> impl Iterator<Item = &str> {
    command.split_whitespace()
}

/// How much room a command needs, inferred from what it is rather than from what the caller
/// thinks it costs.
///
/// This is the whole reason `timeout_ms` is gone. The caller knows whether it needs the result and
/// has no way to know how long the work takes, so a number it supplies is a guess in both
/// directions — and both directions are expensive. Too short and a test suite is killed at thirty
/// seconds, four times, having never run; too long and one call holds the turn for an hour. The
/// class is the same judgement [`looks_long_running`] already makes, extended from "will this ever
/// exit" to "how long does it take when it does".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandClass {
    /// Something whose answer is a fact about the repository: status, a diff, a file listing.
    Quick,
    /// A test run. Slow by nature and worthless if cut off — a killed suite has told nobody
    /// anything, which is exactly why the retry loop it used to cause was so expensive.
    Test,
    /// A compile, a type check, an install. Longer than a test run and, unlike one, usually says
    /// something while it works.
    Build,
}

impl CommandClass {
    fn budget(self, budgets: ShellBudgets) -> Duration {
        match self {
            Self::Quick => budgets.quick,
            Self::Test => budgets.test,
            Self::Build => budgets.build,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Quick => "a quick command",
            Self::Test => "a test run",
            Self::Build => "a build or install",
        }
    }
}

fn classify(command: &str) -> CommandClass {
    let c = command.to_ascii_lowercase();
    // Test before build: `cargo test` compiles on its way to running, and the run is the part
    // that has to finish. `gradle test` likewise.
    if is_any(
        &c,
        &[
            "cargo test",
            "cargo nextest",
            "go test",
            "npm test",
            "npm run test",
            "pnpm test",
            "yarn test",
            "bun test",
            "vitest",
            "jest",
            "pytest",
            "phpunit",
            "rspec",
            "mvn test",
            "gradle test",
            "dotnet test",
            "playwright test",
            "cypress run",
        ],
    ) {
        return CommandClass::Test;
    }
    if is_any(
        &c,
        &[
            "cargo build",
            "cargo check",
            "cargo clippy",
            "go build",
            "npm run build",
            "pnpm build",
            "yarn build",
            "vite build",
            "next build",
            "nuxt build",
            "tsc",
            "webpack",
            "rollup",
            "esbuild",
            "make",
            "cmake",
            "gradle build",
            "gradlew",
            "mvn package",
            "mvn compile",
            "dotnet build",
            "pip install",
            "npm install",
            "npm ci",
            "pnpm install",
            "yarn install",
            "cargo install",
            "uv pip",
            "poetry install",
            "bundle install",
        ],
    ) {
        return CommandClass::Build;
    }
    CommandClass::Quick
}

fn is_any(command: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| command.contains(n))
}

/// A duration as a person would say it out loud: `45s`, `10m`, `1h 5m`.
/// Millisecond counts in a result read as a machine talking, and the reader is trying to work out
/// whether to wait longer or give up — which is a question about minutes.
fn elapsed(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    }
}

fn is_shell_backgrounded(command: &str) -> bool {
    let command = command.trim_end();
    command.ends_with('&') && !command.ends_with("&&")
}

/// What to do when a call was killed at its budget.
/// The advice is split by what the reader knows, which is not the same as what went wrong: whether
/// they said `wait: true` decides whether there is anything left to change. A non-waiting call has
/// a budget it never chose, and the one move that reliably helps is asking for the result.
fn timeout_guidance(waiting: bool, class: CommandClass) -> String {
    let mut s = String::from("--- what to do about the timeout ---\n");
    let first = if waiting {
        format!(
            "- This ran for the full backstop that `wait: true` allows. The command is {}; if it \
             legitimately needs longer than that, run it with background: true and read the \
             result when its notification arrives.\n",
            class.label()
        )
    } else {
        format!(
            "- This is {}'s budget, which shell chose from the command. If its result is what you \
             need, pass `wait: true` next time and it runs to completion instead.\n",
            class.label()
        )
    };
    s.push_str(&first);
    s.push_str(
        "- If it was meant to keep running, retry with background: true so it gets a task id, \
         durable output, and can be stopped.\n",
    );
    s.push_str(
        "- Do not simply re-run it unchanged: the same command under the same budget will \
                be killed the same way.\n",
    );
    s.push_str("- If it was waiting for input, it will never get any here.");
    s
}

/// What to do when a call was killed for printing nothing.
///
/// Deliberately does not say "try again with a longer timeout". A process that has said nothing
/// for ten minutes is not slow, it is blocked, and giving the block more time is the one move
/// guaranteed not to help. The leads are the things that actually unblock it: something the
/// command is waiting for that this call cannot provide, or a smaller piece of the work that does
/// finish.
fn stall_guidance(class: CommandClass) -> String {
    let mut s = String::from("--- what to do about the stall ---\n");
    s.push_str(
        "- It was alive and silent, not slow. Something it needs is never arriving: a port, a \
         lock, a file another process holds, or input on stdin (which is empty here, so a \
         command that reads stdin sees EOF and stops).\n",
    );
    s.push_str(&format!(
        "- Do not re-run it unchanged, and do not give it more time. It is {} and it is blocked, \
         so a longer run ends the same way.\n",
        class.label()
    ));
    s.push_str(
        "- Narrow the work until it produces output: one test file, one package, one failing \
         case. If the block is on a resource, start that resource first and then run this.\n",
    );
    s.push_str(
        "- If it is legitimately quiet for a long time and you still need the result, pass \
         background: true and let it finish while you do something else.",
    );
    s
}

/// Says what a hand-rolled `git worktree` command did **not** do.
/// # Why a note and not a refusal
/// `git worktree add` is a legitimate thing to run, and blocking it would be this tool deciding a
/// policy question it has no standing to decide (see the crate docs: tools report facts). What it
/// *is* entitled to report is a fact the model will otherwise get wrong — because the trap here is
/// silent:
/// - after `git worktree add`, the session's working directory is **unchanged**. The next
///   `write_file src/a.rs` lands in the original tree, not the new checkout, and looks like it
///   worked. `enter_worktree` is the one that actually moves the session.
/// - `git worktree remove` can delete the checkout the session is currently running in, leaving
///   `exec_cwd` pointing at a directory that no longer exists. `exit_worktree` moves the session
///   back *and then* removes.
/// Deliberately narrow: it fires only on a `git … worktree <add|remove|prune>` command, so an
/// unrelated command that merely contains the word "worktree" (a grep, a path) says nothing.
fn worktree_note(command: &str) -> Option<&'static str> {
    let words: Vec<&str> = command.split_whitespace().collect();
    let git = words
        .iter()
        .position(|w| *w == "git" || w.ends_with("/git"))?;
    let worktree = words.iter().skip(git + 1).position(|w| *w == "worktree")? + git + 1;
    match words.get(worktree + 1)? {
        &"add" => Some(
            "--- this checkout is not managed by zlogic ---\n\
             - The session's working directory has NOT changed: later tool calls still run where \
             they did before, and relative paths still resolve there.\n\
             - Use enter_worktree instead when the point is to *work* in a worktree — it moves the \
             session, and exit_worktree keeps or removes the checkout in one step.",
        ),
        &"remove" | &"prune" => Some(
            "--- careful with the session's own checkout ---\n\
             - If this removed the worktree the session is running in, its working directory now \
             points at a directory that no longer exists and every later tool call will fail.\n\
             - exit_worktree is the one that moves the session back first, then removes.",
        ),
        _ => None,
    }
}

/// Turns exit code 127 into something actionable.
/// Only for 127 with a matching message: guessing "not installed" from any failure would produce
/// confident nonsense on a command that simply returned an error.
fn not_found_guidance(command: &str, stderr: &str, code: Option<i32>) -> Option<String> {
    if code != Some(127) {
        return None;
    }
    let lower = stderr.to_ascii_lowercase();
    if !["command not found", "not found", "not recognized"]
        .iter()
        .any(|m| lower.contains(m))
    {
        return None;
    }
    // The first word, past any `VAR=value` prefixes.
    let name = command
        .split_whitespace()
        .find(|w| !w.contains('=') || w.starts_with('/'))
        .map(|w| w.rsplit('/').next().unwrap_or(w))?;
    Some(format!(
        "--- {name} is not on the PATH ---\n\
         - Check with `command -v {name}`.\n\
         - If it is a project dependency, install the project's dependencies and retry.\n\
         - If it is installed somewhere unusual, call it by its full path."
    ))
}

/// This process's environment, minus anything shaped like a credential.
pub(crate) fn scrubbed_env() -> Vec<(String, String)> {
    scrub(std::env::vars())
}

fn child_env(runtime_paths: &[PathBuf], vars: &[(String, String)]) -> Vec<(String, String)> {
    let mut env = scrubbed_env();
    if !runtime_paths.is_empty() {
        let mut entries = std::env::var_os("PATH")
            .as_ref()
            .map(|path| std::env::split_paths(path).collect::<Vec<_>>())
            .unwrap_or_default();
        for dir in runtime_paths {
            if !entries.contains(dir) {
                entries.insert(0, dir.clone());
            }
        }
        if let Ok(path) = std::env::join_paths(entries) {
            env.retain(|(name, _)| name != "PATH");
            env.push(("PATH".into(), path.to_string_lossy().into_owned()));
        }
    }
    // Configured last, so a name the user declared wins over the inherited one. They are *not*
    // passed through `scrub`: that filter exists to stop the parent process's credentials leaking
    // into a child that never asked for them, and a variable the user wrote down on purpose is
    // the opposite case. The credential-shaped names are refused at the layer that can see where
    // they came from instead, which is the only place that can tell a workspace's `settings.yaml`
    // from a hand-edited global one.
    for (name, value) in vars {
        env.retain(|(existing, _)| !same_env_name(existing, name));
        env.push((name.clone(), value.clone()));
    }
    env
}

/// Windows environment names are case-insensitive, so an override has to evict the inherited
/// spelling as well or the child sees whichever the loader happened to read first. On Unix `FOO`
/// and `foo` are two different variables and both have to survive.
fn same_env_name(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

/// Whether a variable name looks like a credential.
/// A name-shape denylist rather than a value scan: a value that merely looks random is very often
/// a legitimate build hash or revision, whereas `*_API_KEY` is unambiguous. Nothing here is
/// configurable — a setting that could re-admit these would defeat the point of removing them.
fn is_credential_name(name: &str) -> bool {
    zlogic_protocol::settings::looks_like_credential(name)
}

/// Split out from [`scrubbed_env`] so the rule can be tested against a constructed environment.
/// The alternative — setting real variables in a test — is `unsafe` in this edition and would
/// leak across tests sharing the process.
fn scrub(vars: impl Iterator<Item = (String, String)>) -> Vec<(String, String)> {
    vars.filter(|(k, _)| !is_credential_name(k)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::{AgentRequest, AgentSpawner, ProcessRequest, SpawnedProcess, TaskHost};
    use crate::{OutputSink, ToolExecStatus, test_ctx};
    use std::sync::Arc;
    #[cfg(unix)]
    use zlogic_protocol::SessionId;
    #[cfg(unix)]
    use zlogic_task::{TaskId, TaskRun};

    #[cfg(unix)]
    struct AdoptingHost {
        task_id: TaskId,
    }

    #[cfg(unix)]
    #[async_trait]
    impl TaskHost for AdoptingHost {
        async fn start_process(
            &self,
            _request: ProcessRequest,
            mut process: SpawnedProcess,
        ) -> std::result::Result<TaskId, String> {
            (&mut process.child).terminate().await;
            Ok(self.task_id)
        }

        async fn start_agent(
            &self,
            _request: AgentRequest,
            _spawner: Arc<dyn AgentSpawner>,
        ) -> std::result::Result<TaskId, String> {
            unreachable!()
        }

        async fn get(
            &self,
            _session_id: SessionId,
            _task_id: TaskId,
        ) -> std::result::Result<Option<TaskRun>, String> {
            Ok(None)
        }

        async fn report(
            &self,
            _session_id: SessionId,
            _task_id: TaskId,
        ) -> std::result::Result<Option<crate::TaskReport>, String> {
            Ok(None)
        }

        async fn list(&self, _session_id: SessionId) -> std::result::Result<Vec<TaskRun>, String> {
            Ok(Vec::new())
        }

        async fn send_agent_message(
            &self,
            _session_id: SessionId,
            _task_id: TaskId,
            _message: String,
        ) -> std::result::Result<(), String> {
            Ok(())
        }

        async fn stop(
            &self,
            _session_id: SessionId,
            _task_id: TaskId,
        ) -> std::result::Result<(), String> {
            Ok(())
        }
    }

    fn setup() -> (tempfile::TempDir, ToolCtx) {
        let d = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(d.path());
        ctx.max_result_chars = 100_000;
        (d, ctx)
    }

    fn args(command: &str) -> String {
        json!({ "command": command }).to_string()
    }

    /// Every limit short enough for a test to sit through, so the kill paths are reachable
    /// without a ten-minute test. The *relationships* are what production encodes — a test run
    /// outlasting a quick check, a stall outlasting both — and those are preserved here.
    fn instant_budgets() -> ShellBudgets {
        ShellBudgets {
            quick: Duration::from_millis(400),
            // Wide against the stall limit below, because on Windows the spawn itself costs tens
            // of milliseconds (assigning the child to a job object walks the machine's threads)
            // and that latency is charged to the budget but not to the stall clock. A narrow gap
            // would let a slow spawn decide which of the two fires first.
            test: Duration::from_millis(2000),
            build: Duration::from_millis(800),
            waiting: Duration::from_millis(700),
            stall: Duration::from_millis(500),
            progress: Duration::from_millis(200),
        }
    }

    fn impatient() -> Shell {
        Shell::default().with_budgets(instant_budgets())
    }

    #[test]
    fn command_summaries_are_single_line_and_bounded() {
        let command = format!("zlogic start\n{}", "x".repeat(1_000));
        let summary = summarize_command(&command);
        assert!(summary.starts_with("zlogic start"));
        assert!(!summary.contains('\n'));
        assert!(summary.contains("characters total"));
        assert!(summary.chars().count() < 300);
    }

    #[test]
    fn read_profile_turns_the_startup_file_switches_back_on_and_off() {
        assert_eq!(posix_args(true), &["-l", "-c"]);
        assert_eq!(posix_args(false), &["--noprofile", "--norc", "-c"]);
        assert!(!powershell_args(true).contains(&"-NoProfile"));
        assert!(powershell_args(false).contains(&"-NoProfile"));
        assert!(!cmd_args(true).contains(&"/d"));
        assert!(cmd_args(false).contains(&"/d"));
        assert!(
            Shell::default().backend.launch_args.contains(&"--noprofile"),
            "the unconfigured fixture stays hermetic; only bootstrap reads the flag"
        );
    }

    #[test]
    fn definition_names_the_resolved_backend_and_its_syntax() {
        let shell = Shell::default();
        let definition = shell.definition();
        assert!(definition.description.contains(shell.backend_name()));
        match shell.dialect() {
            ShellDialect::Posix => assert!(definition.description.contains("POSIX shell")),
            ShellDialect::PowerShell => assert!(definition.description.contains("PowerShell")),
            ShellDialect::Cmd => assert!(definition.description.contains("cmd.exe")),
        }
        assert!(
            definition.parameters["properties"]["command"]["description"]
                .as_str()
                .unwrap()
                .contains(shell.backend_name())
        );
        assert_eq!(
            definition.parameters["properties"]["background"]["type"],
            "boolean"
        );
        assert_eq!(
            definition.parameters["properties"]["wait"]["type"],
            "boolean"
        );
        // No timeout argument at all. A number the model picks is a guess in both directions, and
        // both are expensive; the schema must not offer one.
        assert!(
            definition.parameters["properties"]
                .get("timeout_ms")
                .is_none(),
            "the caller chooses whether it needs the result, not how long that takes"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn explicit_background_hands_the_live_process_to_task_host() {
        let (_d, mut ctx) = setup();
        let task_id = TaskId::new();
        ctx.tasks = Some(Arc::new(AdoptingHost { task_id }));
        let out = Shell::default()
            .execute(
                &ctx,
                &json!({ "command": "sleep 30", "background": true }).to_string(),
            )
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Success);
        assert!(out.model_text().contains(&task_id.to_string()));
    }

    /// A recognised server is refused, not silently promoted: waiting for it would never return,
    /// and a call whose outcome flips between "result" and "task id" cannot be relied on.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_recognised_server_must_be_asked_for_as_background_work() {
        let (_d, mut ctx) = setup();
        let task_id = TaskId::new();
        ctx.tasks = Some(Arc::new(AdoptingHost { task_id }));

        let out = Shell::default()
            .execute(&ctx, &args("tail -f /dev/null"))
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Failed);
        let text = out.model_text();
        assert!(text.contains("watch or follow mode"), "{text}");
        assert!(text.contains("background: true"), "{text}");

        // The same command with the flag runs as a task, which is the only way it can run.
        let out = Shell::default()
            .execute(
                &ctx,
                &json!({ "command": "tail -f /dev/null", "background": true }).to_string(),
            )
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Success);
        assert!(out.model_text().contains(&task_id.to_string()));
    }

    /// `wait` and `background` are opposite requests; asking for both is a mistake, not a
    /// preference to resolve silently.
    #[tokio::test]
    async fn wait_and_background_together_are_refused() {
        let (_d, ctx) = setup();
        let out = Shell::default()
            .execute(
                &ctx,
                &json!({ "command": "echo x", "wait": true, "background": true }).to_string(),
            )
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Failed);
        assert!(
            out.model_text().contains("contradict"),
            "{}",
            out.model_text()
        );
    }

    #[tokio::test]
    async fn captures_stdout_and_a_zero_exit() {
        let (_d, ctx) = setup();
        let out = Shell::default()
            .execute(&ctx, &args("echo hello"))
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Success);
        let text = out.model_text();
        assert!(text.contains("hello"), "{text}");
        assert!(text.contains("exit 0"), "{text}");
    }

    /// Output that is not UTF-8 has to arrive as text. A Windows console program writes its
    /// messages in the console code page, so this is the everyday case there and a synthetic one
    /// here — the same bytes reach the pipe either way.
    #[tokio::test]
    async fn a_gbk_stream_arrives_as_text_rather_than_mojibake() {
        let (_d, ctx) = setup();
        // GBK for 「中文」. Read as UTF-8 these are not "damaged" but *reinterpreted*: a lead byte
        // in C2..DF starts a valid two-byte sequence, so the failure is a plausible wrong answer
        // rather than an obvious one.
        let out = Shell::default()
            .execute(&ctx, &args(r"printf '\xD6\xD0\xCE\xC4'"))
            .await
            .unwrap();
        let text = out.model_text();
        assert!(text.contains("中文"), "{text}");
        assert!(!text.contains('\u{fffd}'), "{text}");
    }

    /// A non-zero exit is an error result, so the model does not build on a failed step.
    #[tokio::test]
    async fn a_non_zero_exit_is_an_error_but_keeps_the_output() {
        let (_d, ctx) = setup();
        let out = Shell::default()
            .execute(&ctx, &args("zlogic before; exit 3"))
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Failed);
        let text = out.model_text();
        assert!(text.contains("exit 3"), "{text}");
        assert!(
            text.contains("before"),
            "the output is what it needs to fix it: {text}"
        );
    }

    #[tokio::test]
    async fn stderr_is_reported_separately() {
        let (_d, ctx) = setup();
        let out = Shell::default()
            .execute(&ctx, &args("zlogic oops 1>&2"))
            .await
            .unwrap();
        let text = out.model_text();
        assert!(text.contains("--- stderr"), "{text}");
        assert!(text.contains("oops"), "{text}");
    }

    #[tokio::test]
    async fn runs_in_the_requested_directory() {
        let (d, ctx) = setup();
        std::fs::create_dir(d.path().join("sub")).unwrap();
        let out = Shell::default()
            .execute(
                &ctx,
                &json!({ "command": "pwd", "path": "sub" }).to_string(),
            )
            .await
            .unwrap();
        assert!(out.model_text().contains("sub"), "{}", out.model_text());
    }

    /// The full transcript is reachable, and the reference that keeps it alive is present.
    #[tokio::test]
    async fn the_full_transcript_reaches_the_object_store() {
        let (_d, ctx) = setup();
        let shell = Shell::default();
        // One word to each stream, spelled for the shell that was resolved. A command named after
        // this product only produced a transcript where a `zlogic` happened to be installed, and a
        // runner has none: the object held two "command not found" lines and nothing else.
        let command = match shell.dialect() {
            ShellDialect::Cmd => "echo one & echo two 1>&2",
            ShellDialect::Posix | ShellDialect::PowerShell => "echo one; echo two 1>&2",
        };
        let out = shell.execute(&ctx, &args(command)).await.unwrap();

        let id = out
            .object_with_role(ObjectRole::Output)
            .expect("a transcript was captured");
        let stored = String::from_utf8(ctx.objects.get(id).unwrap()).unwrap();
        assert!(
            stored.contains("one") && stored.contains("two"),
            "interleaved: {stored}"
        );
        assert!(out.dangling_display_objects().is_empty());
    }

    /// No output means no object: an empty capture is a row and a card that say nothing.
    #[tokio::test]
    async fn silence_stores_nothing() {
        let (_d, ctx) = setup();
        let out = Shell::default().execute(&ctx, &args("true")).await.unwrap();
        assert!(out.objects.is_empty());
        assert!(out.display.is_empty());
        assert!(out.model_text().contains("stdout (empty)"));
    }

    #[tokio::test]
    async fn output_reaches_the_sink_as_it_arrives() {
        #[derive(Default)]
        struct Recorder(std::sync::Mutex<Vec<(OutputStream, String)>>);
        impl OutputSink for Recorder {
            fn emit(&self, stream: OutputStream, chunk: &str) {
                self.0.lock().unwrap().push((stream, chunk.to_string()));
            }
        }
        let sink = Arc::new(Recorder::default());
        let (_d, mut ctx) = setup();
        ctx.output = Some(sink.clone());

        Shell::default()
            .execute(&ctx, &args("echo streamed"))
            .await
            .unwrap();
        let seen = sink.0.lock().unwrap().clone();
        assert!(
            seen.iter()
                .any(|(s, c)| *s == OutputStream::Stdout && c.contains("streamed"))
        );
    }

    /// The budget kills the process and says what to do instead.
    #[tokio::test]
    async fn a_slow_command_runs_out_of_time_and_is_killed() {
        let (_d, ctx) = setup();
        let out = impatient().execute(&ctx, &args("sleep 30")).await.unwrap();
        assert_eq!(out.status, ToolExecStatus::Timeout);
        let text = out.model_text();
        assert!(text.contains("ran out of time"), "{text}");
        assert!(
            text.contains("wait: true"),
            "it must say how to proceed: {text}"
        );
    }

    /// A command that prints nothing is reported as *stuck*, not as slow — the two call for
    /// opposite responses, and conflating them is what sends the model off to re-run the thing
    /// that is already wedged.
    #[tokio::test]
    async fn a_silent_command_is_reported_as_stuck_rather_than_slow() {
        let (_d, ctx) = setup();
        // A test-class command — so its budget is longer than the 500ms stall limit and silence,
        // not the clock, is what ends it. The `sleep` is the whole command; the rest is there to
        // be classified.
        let out = impatient()
            .execute(&ctx, &args("sleep 30 # cargo test"))
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Timeout);
        let text = out.model_text();
        assert!(text.contains("printed nothing"), "{text}");
        assert!(text.contains("what to do about the stall"), "{text}");
        assert!(
            !text.contains("wait: true"),
            "a stall is not fixed by waiting longer: {text}"
        );
    }

    /// Output resets the stall clock. A test suite that reports each case is visibly alive, and a
    /// shared deadline would call a long quiet stretch between two results a hang.
    #[tokio::test]
    async fn output_keeps_a_command_alive_past_the_stall_limit() {
        let (_d, ctx) = setup();
        // Prints every ~0.5s for three seconds against a 1.5s stall limit. Under a single shared
        // deadline this is a kill; under a stall clock it is a command that keeps talking. The
        // wall clock is generous so only the stall rule can end it, and the gaps are wide enough
        // that a slow process spawn cannot be mistaken for silence.
        let budgets = ShellBudgets {
            quick: Duration::from_secs(30),
            stall: Duration::from_millis(1500),
            ..instant_budgets()
        };
        let out = Shell::default()
            .with_budgets(budgets)
            .execute(
                &ctx,
                &args("for i in 1 2 3 4 5 6; do echo tick $i; sleep 0.5; done"),
            )
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Success, "{}", out.model_text());
        assert!(out.model_text().contains("tick 5"), "{}", out.model_text());
    }

    /// The point of `wait`: a result the turn cannot go on without, however long it takes. It is
    /// never handed to the task runtime, and the class budget that applies to a non-waiting call
    /// does not apply here.
    #[cfg(unix)]
    #[tokio::test]
    async fn wait_runs_to_completion_and_creates_no_task() {
        let (_d, mut ctx) = setup();
        let task_id = TaskId::new();
        ctx.tasks = Some(Arc::new(AdoptingHost { task_id }));

        let out = impatient()
            .execute(
                &ctx,
                &json!({
                    "command": "sleep 0.2; echo done",
                    "wait": true
                })
                .to_string(),
            )
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Success, "{}", out.model_text());
        assert!(out.model_text().contains("done"), "{}", out.model_text());
        assert!(
            !out.model_text().contains(&task_id.to_string()),
            "a waiting call must not become a background task: {}",
            out.model_text()
        );
    }

    /// `wait: true` is not a promise to wait forever: the backstop still ends it, and the result
    /// says the backstop is what ran out rather than pretending the caller asked for a deadline.
    #[tokio::test]
    async fn wait_still_has_a_backstop() {
        let (_d, ctx) = setup();
        // Prints often enough to never trip the stall limit, so only the backstop can end this.
        let budgets = ShellBudgets {
            stall: Duration::from_secs(30),
            ..instant_budgets()
        };
        let out = Shell::default()
            .with_budgets(budgets)
            .execute(
                &ctx,
                &json!({ "command": "while true; do echo tick; sleep 0.2; done", "wait": true })
                    .to_string(),
            )
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Timeout);
        let text = out.model_text();
        assert!(text.contains("ran out of time"), "{text}");
        assert!(text.contains("backstop"), "{text}");
    }

    /// Cancelling stops the child and still reports what it printed first.
    #[tokio::test]
    async fn cancelling_kills_the_child_and_keeps_its_output() {
        let (_d, ctx) = setup();
        let token = ctx.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            token.cancel();
        });

        let out = Shell::default()
            .execute(&ctx, &args("zlogic early; sleep 30"))
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Cancelled);
        assert!(out.model_text().contains("early"), "{}", out.model_text());
    }

    /// A command that exits while a process it started still holds the output stream must not hang
    /// the call: with `wait: true` there is no deadline that would eventually end it, so "the child
    /// is gone and nothing is arriving" is the only signal left.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_descendant_holding_the_pipe_does_not_hang_the_call() {
        let (_d, ctx) = setup();
        let out = tokio::time::timeout(
            Duration::from_secs(10),
            Shell::default().execute(&ctx, &args("echo started; (sleep 30 &)")),
        )
        .await
        .expect("the call must not wait for an EOF that will never come")
        .unwrap();
        let text = out.model_text();
        assert!(text.contains("started"), "{text}");
        assert!(text.contains("output may be incomplete"), "{text}");
    }

    /// Refused before spawning: a started dev server cannot be usefully reported on.
    #[tokio::test]
    async fn a_command_that_never_exits_is_refused_up_front() {
        let (_d, ctx) = setup();
        let out = Shell::default()
            .execute(&ctx, &args("npm run dev"))
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Failed);
        assert!(
            out.model_text().contains("never exits"),
            "{}",
            out.model_text()
        );
    }

    #[test]
    fn the_long_running_check_is_narrow_enough_to_be_safe() {
        // Recognised.
        for c in [
            "npm run dev",
            "pnpm dev",
            "vite",
            "npx vite --host 0.0.0.0",
            "/usr/local/bin/vite",
            "next dev",
            "uvicorn app:main",
            "tail -f log.txt",
            "cargo watch --watch src",
        ] {
            assert!(looks_long_running(c).is_some(), "{c} should be recognised");
        }
        // Ordinary work that must not be blocked — including the build variants whose names
        // overlap with the server ones, and `vitest`, which merely *contains* `vite`.
        for c in [
            "npm run build",
            "npm test",
            "vite build",
            "npx vite build",
            "bunx vitest run src/features/chat/x.test.ts",
            "npx vitest run",
            "cargo build",
            "cargo test",
            "git status",
            "ls -la",
            "python script.py",
            "tail -n 100 log.txt",
        ] {
            assert!(looks_long_running(c).is_none(), "{c} must not be blocked");
        }
        // Shell-level detachment is rejected in favour of the typed background argument.
        assert!(is_shell_backgrounded("npm run dev &"));
        assert!(!is_shell_backgrounded("npm run dev && zlogic x"));
        // `&&` is not backgrounding.
        assert!(looks_long_running("npm run dev && zlogic x").is_some());
    }

    /// The budget is inferred, so getting a class wrong is the whole mechanism failing. These are
    /// the commands whose misclassification this design was built to stop: a test suite cut off at
    /// a quick command's budget, and a build killed for taking as long as a build takes.
    #[test]
    fn the_class_decides_the_budget_and_test_outranks_build() {
        for c in [
            "cargo test",
            "cargo test --workspace",
            "cargo nextest run",
            "npm test",
            "pnpm test -- --run",
            "bunx vitest run src/x.test.ts",
            "pytest -q",
            "go test ./...",
            "dotnet test",
            "phpunit",
            "mvn test",
            "gradle test",
            "playwright test",
            // Compiles on the way to running, and the run is the part that has to finish.
            "gradle test --tests Foo",
        ] {
            assert_eq!(classify(c), CommandClass::Test, "{c}");
        }
        for c in [
            "cargo build --release",
            "cargo check",
            "cargo clippy --all-targets",
            "npm run build",
            "pnpm build",
            "vite build",
            "next build",
            "npx tsc --noEmit",
            "make",
            "cmake --build .",
            "go build ./...",
            "pip install -r requirements.txt",
            "npm ci",
            "cargo install ripgrep",
        ] {
            assert_eq!(classify(c), CommandClass::Build, "{c}");
        }
        for c in [
            "git status",
            "ls -la",
            "grep -r foo src",
            "cat Cargo.toml",
            "python script.py",
            "tail -n 100 log.txt",
            // A file that happens to be named `test` is not a test run: the needles are whole
            // commands, not a bare `test`.
            "ls test",
        ] {
            assert_eq!(classify(c), CommandClass::Quick, "{c}");
        }
        // The ordering that makes the last case work, stated directly: `vitest` contains neither
        // `vite build` nor `npm run build`, but `npm run build && npm test` is both.
        assert_eq!(
            classify("npm run build && npm test"),
            CommandClass::Test,
            "a command that builds and then tests needs the test budget"
        );
    }

    /// A stalled command and a slow one need different advice, and the stall advice must not be
    /// the one that says "wait longer".
    #[test]
    fn the_two_deadlines_give_opposite_advice() {
        let slow = timeout_guidance(false, CommandClass::Test);
        assert!(slow.contains("wait: true"), "{slow}");

        let stuck = stall_guidance(CommandClass::Test);
        assert!(!stuck.contains("wait: true"), "{stuck}");
        assert!(stuck.contains("Do not re-run it unchanged"), "{stuck}");
    }

    /// The running call says so, and says how long it has been quiet. A blank console and a
    /// wedged process look identical from the outside; the elapsed-since-output number is the
    /// only thing that tells them apart.
    #[tokio::test]
    async fn a_running_call_narrates_its_own_progress() {
        #[derive(Default)]
        struct Recorder(std::sync::Mutex<Vec<(OutputStream, String)>>);
        impl OutputSink for Recorder {
            fn emit(&self, stream: OutputStream, chunk: &str) {
                self.0.lock().unwrap().push((stream, chunk.to_string()));
            }
        }
        let sink = Arc::new(Recorder::default());
        let (_d, mut ctx) = setup();
        ctx.output = Some(sink.clone());

        // Prints once, then goes quiet for longer than the progress interval but less than the
        // stall limit — a working command, not a stuck one. The interval has to clear
        // `CHILD_EXIT_POLL`, because that is the only tick the loop makes: a 200ms interval with
        // a one-second poll would never fire.
        let budgets = ShellBudgets {
            quick: Duration::from_secs(30),
            stall: Duration::from_secs(30),
            progress: Duration::from_millis(300),
            ..instant_budgets()
        };
        Shell::default()
            .with_budgets(budgets)
            .execute(&ctx, &args("echo started; sleep 1.4"))
            .await
            .unwrap();

        let seen = sink.0.lock().unwrap().clone();
        let progress: Vec<&String> = seen
            .iter()
            .filter(|(s, c)| *s == OutputStream::Progress && c.contains("still running"))
            .map(|(_, c)| c)
            .collect();
        assert!(
            !progress.is_empty(),
            "a quiet console must be narrated: {seen:?}"
        );
        assert!(
            progress.iter().any(|c| c.contains("since the last output")),
            "and the gap is the number that matters: {progress:?}"
        );
    }

    /// The point of the buffer: a huge stream stays bounded and the tail survives.
    #[test]
    fn the_buffer_keeps_the_head_and_prefers_the_tail() {
        let mut b = HeadTail::new(10, 20);
        for i in 0..100 {
            b.push(&format!("{i:03}\n"));
        }
        assert_eq!(b.total, 400);
        assert!(b.omitted() > 0);

        let rendered = b.render();
        assert!(
            rendered.starts_with("000\n001"),
            "the head is kept: {rendered}"
        );
        assert!(
            rendered.ends_with("099\n"),
            "the tail — where failures are — is kept: {rendered}"
        );
        assert!(
            rendered.contains("omitted"),
            "and the gap is declared: {rendered}"
        );
    }

    #[test]
    fn nothing_is_omitted_from_a_small_stream() {
        let mut b = HeadTail::new(10, 20);
        b.push("short");
        assert_eq!(b.omitted(), 0);
        assert_eq!(b.render(), "short");
    }

    /// Neither piece may end or begin mid-line: half a stack frame or half a path reads as a fact
    /// that is not true.
    #[test]
    fn the_buffer_shows_only_whole_lines() {
        let mut b = HeadTail::new(50, 100);
        for i in 0..100 {
            b.push(&format!("line {i:03} of the log\n"));
        }
        let (head, tail) = b.aligned();
        assert!(head.ends_with('\n'), "head: {head:?}");
        assert!(tail.ends_with('\n'), "tail: {tail:?}");
        assert!(b.omitted() > 0, "this is the truncating case");

        let source: Vec<String> = (0..100)
            .map(|i| format!("line {i:03} of the log"))
            .collect();
        for piece in [head, tail] {
            for line in piece.lines() {
                assert!(
                    source.iter().any(|s| s == line),
                    "{line:?} is a fragment, not a line"
                );
            }
        }
    }

    /// A budget below the length of one line must not be mistaken for "this stream has no lines".
    /// Inferring structure from "the head contains no newline" would show a fragment of a
    /// perfectly line-structured log — which is the exact failure the alignment exists to prevent.
    #[test]
    fn a_budget_smaller_than_one_line_still_aligns_what_it_can() {
        let mut b = HeadTail::new(10, 60);
        for i in 0..100 {
            b.push(&format!("line {i:03} of the log\n"));
        }
        let (head, tail) = b.aligned();
        assert_eq!(head, "", "no whole line fits the head, so it shows none");
        assert!(
            tail.starts_with("line "),
            "the tail still shows whole lines: {tail:?}"
        );
        assert!(tail.ends_with("line 099 of the log\n"));
    }

    /// …but when alignment would leave nothing at all, a fragment beats an empty result.
    #[test]
    fn alignment_never_reduces_the_output_to_nothing() {
        let mut b = HeadTail::new(5, 5);
        b.push("a very long single line that dwarfs both budgets\nand another one just as long\n");
        let (head, tail) = b.aligned();
        assert!(
            !head.is_empty() || !tail.is_empty(),
            "showing something beats showing nothing"
        );
    }

    /// Output with no line structure is kept rather than dropped — a progress bar rewriting itself,
    /// or a single-line JSON response, has no structure to preserve.
    #[test]
    fn a_stream_with_no_newlines_is_still_shown() {
        let mut b = HeadTail::new(5, 5);
        b.push(&"x".repeat(500));
        let rendered = b.render();
        assert!(rendered.starts_with("xxxxx"), "{rendered}");
        assert!(rendered.contains("omitted"));
    }

    /// A byte-offset cut would have put replacement characters into valid output.
    #[test]
    fn the_buffer_cuts_on_character_boundaries() {
        let mut b = HeadTail::new(2, 2);
        b.push("中文内容中文内容");
        let rendered = b.render();
        assert!(rendered.starts_with("中文"), "{rendered}");
        assert!(rendered.ends_with("内容"), "{rendered}");
        assert!(
            !rendered.contains('\u{fffd}'),
            "no mangled characters: {rendered}"
        );
    }

    /// A model asking for a key gets nothing, whoever approved the command.
    /// Checked against a constructed environment rather than by setting real variables: that is
    /// `unsafe` in this edition, and a leaked variable would follow every other test in the
    /// process.
    #[test]
    fn credential_shaped_variables_are_removed_and_ordinary_ones_are_not() {
        let env = [
            ("OPENAI_API_KEY", "sk-secret"),
            ("GITHUB_TOKEN", "ghp_secret"),
            ("aws_secret", "shh"),
            ("DB_PASSWORD", "hunter2"),
            ("API_KEY", "bare"),
            ("PATH", "/usr/bin"),
            // These merely mention a key without being one; removing them would break builds.
            ("API_KEY_FILE", "/etc/keys"),
            ("GIT_COMMIT", "deadbeef"),
        ];
        let kept: Vec<&str> = scrub(env.iter().map(|(k, v)| (k.to_string(), v.to_string())))
            .iter()
            .map(|(k, _)| env.iter().find(|(n, _)| n == k).unwrap().0)
            .collect();

        assert_eq!(kept, ["PATH", "API_KEY_FILE", "GIT_COMMIT"]);
    }

    /// …and the filter really is what the child gets, not just a function nobody calls.
    #[tokio::test]
    async fn the_child_receives_the_scrubbed_environment() {
        let (_d, ctx) = setup();
        let out = Shell::default().execute(&ctx, &args("env")).await.unwrap();
        let text = out.model_text();

        assert!(
            text.contains("PATH="),
            "an ordinary variable reaches the child: {text}"
        );
        for line in text.lines() {
            if let Some(name) = line.split('=').next() {
                assert!(!is_credential_name(name), "{name} reached the child");
            }
        }
    }

    /// The trap this note exists for: a hand-rolled `git worktree add` does not move the session,
    /// and nothing else in the output says so.
    #[tokio::test]
    async fn a_hand_rolled_worktree_add_is_annotated_not_blocked() {
        let (d, ctx) = setup();
        std::fs::create_dir(d.path().join("wt")).unwrap();
        // `git worktree add` on a repo-less directory fails, so this asserts on the note's own
        // matcher with a command that succeeds and mentions the same subcommand shape.
        let out = Shell::default()
            .execute(
                &ctx,
                &args("zlogic pretend; git worktree add ../wt -b feat || true"),
            )
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Success);
        let text = out.model_text();
        assert!(text.contains("not managed by zlogic"), "{text}");
        assert!(
            text.contains("enter_worktree"),
            "and what to use instead: {text}"
        );
    }

    #[test]
    fn the_worktree_note_is_narrow_enough_not_to_fire_on_unrelated_commands() {
        assert!(worktree_note("git worktree add ../x -b y").is_some());
        assert!(worktree_note("cd /tmp && /usr/bin/git worktree add ../x").is_some());
        assert!(worktree_note("git -C /repo worktree remove ../x").is_some());
        assert!(worktree_note("git worktree prune").is_some());
        // Reading about worktrees is not creating one.
        assert!(worktree_note("git worktree list").is_none());
        assert!(worktree_note("grep -r worktree crates/").is_none());
        assert!(worktree_note("ls ../repo-worktrees").is_none());
        assert!(worktree_note("git status").is_none());
    }

    #[test]
    fn only_a_matching_127_gets_the_not_found_guidance() {
        assert!(not_found_guidance("gh pr list", "gh: command not found", Some(127)).is_some());
        // A plain failure must not be explained as a missing binary.
        assert!(not_found_guidance("gh pr list", "no such pull request", Some(1)).is_none());
        // 127 with an unrelated message is not evidence either.
        assert!(not_found_guidance("gh pr list", "something else", Some(127)).is_none());
    }

    #[test]
    fn the_guidance_names_the_command_not_its_path_or_env_prefix() {
        let g = not_found_guidance(
            "FOO=1 /usr/local/bin/kubectl get pods",
            "not found",
            Some(127),
        )
        .unwrap();
        assert!(g.contains("kubectl"), "{g}");
        assert!(!g.contains("FOO=1"), "{g}");
    }

    #[tokio::test]
    async fn missing_required_args_are_rejected() {
        let (_d, ctx) = setup();
        assert!(Shell::default().execute(&ctx, "{}").await.is_err());
        assert!(Shell::default().execute(&ctx, "not json").await.is_err());
        assert_eq!(
            Shell::default()
                .execute(&ctx, &args("   "))
                .await
                .unwrap()
                .status,
            ToolExecStatus::Failed
        );
    }
}
