//! `shell` — run one command and report what it printed.
//! # Wait for the exit, or ask for the background
//! A call waits for the process to exit, and kills it at its timeout (30 s by default, one minute
//! at most). Two flags cover everything that does not fit in that box:
//! - `wait: true` — the result is required before the turn can continue. The call runs to
//!   completion, however many minutes that takes; `timeout_ms` becomes an optional hard stop.
//! - `background: true` — the work outlives the call. The already-running child is handed to the
//!   process-wide task runtime, which never re-runs it.
//! Neither happens on its own. A recognised server/watcher is refused unless it is asked for as
//! background work (waiting for `npm run dev` to exit would hang forever), and a slow command is
//! killed at its ceiling rather than quietly turned into a task: a call whose outcome flips
//! between "here is your result" and "here is a task id" cannot be relied on by the model calling
//! it.
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
use zlogic_protocol::llm::ToolDefinition;
use zlogic_protocol::stream::OutputStream;

use crate::{
    ObjectRole, ProcessRequest, Recovery, Result, SpawnedProcess, Tool, ToolCtx, ToolExecResult,
    ToolMeta, ToolRisk, parse_args,
};

const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// Ceiling for a call that is **not** willing to wait for a result.
/// Past this the caller has to say which it wants: `wait: true` to have the result in this call,
/// or `background: true` to stop waiting. A silent promotion to a background task used to make
/// that decision on the caller's behalf, which is what made an ordinary call's outcome
/// unpredictable.
const MAX_TIMEOUT_MS: u64 = 60_000;

/// Largest `timeout_ms` a waiting call may ask for.
/// `wait: true` **without** `timeout_ms` has no deadline at all: the flag exists because the
/// result is required, and killing the work at some arbitrary minute hands back the same failure
/// the caller was trying to avoid. This bound only applies to a caller that wants a hard stop.
const WAIT_MAX_TIMEOUT_MS: u64 = 30 * 60_000;

/// How often a call that is still reading output checks whether its child has already exited.
/// A wrapper that spawns something long-lived and returns leaves pipes that only its descendant
/// holds. Reading them would never reach EOF, and with `wait: true` there is no deadline to end
/// that wait: the call would sit there until the user stopped the turn. Once the direct child is
/// gone and the streams have gone quiet, this call has everything it is going to get.
const CHILD_EXIT_POLL: Duration = Duration::from_secs(1);

/// How long a terminated process gets before it is killed outright.
/// Only meaningful on Unix, where a graceful SIGTERM phase precedes SIGKILL. On Windows
/// `start_kill()` is already a hard TerminateProcess, so there is no grace period to bound.
#[cfg(unix)]
const SIGKILL_GRACE: Duration = Duration::from_secs(2);

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
    timeout_ms: Option<u64>,
    #[serde(default)]
    background: bool,
    /// Wait for the process to exit however long it takes, instead of killing it at the timeout.
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
}

impl Default for Shell {
    fn default() -> Self {
        Self::resolve(ShellPreference::Auto)
            // The unconfigured registry is mainly used by tests. Production bootstrap resolves
            // the configured backend and replaces this instance; retaining a platform-shaped
            // definition here is better than making every generic registry constructor fallible.
            .unwrap_or_else(|_| Self::unchecked_platform_default())
    }
}

impl Shell {
    /// Resolves a preference once at startup. Explicit preferences never fall back to another
    /// shell: doing that would make the prompt, policy dialect and actual process disagree.
    pub fn resolve(preference: ShellPreference) -> std::result::Result<Self, String> {
        let backend = match preference {
            ShellPreference::Auto => detect_default_backend()?,
            ShellPreference::GitBash => git_bash_backend().ok_or_else(|| {
                "git_bash is configured, but Git for Windows' bash.exe was not found".to_string()
            })?,
            ShellPreference::Ps7 => named_backend(
                "ps7",
                &["pwsh.exe", "pwsh"],
                &["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"],
                "PowerShell 7",
                "PowerShell",
                ShellDialect::PowerShell,
            )?,
            ShellPreference::Powershell => named_backend(
                "powershell",
                &["powershell.exe", "powershell"],
                &["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"],
                "Windows PowerShell",
                "PowerShell",
                ShellDialect::PowerShell,
            )?,
            ShellPreference::Cmd => named_backend(
                "cmd",
                &["cmd.exe", "cmd"],
                &["/d", "/s", "/c"],
                "Command Prompt",
                "cmd.exe",
                ShellDialect::Cmd,
            )?,
            ShellPreference::Bash => named_backend(
                "bash",
                &["bash.exe", "bash"],
                &["--noprofile", "--norc", "-c"],
                "Bash",
                "POSIX shell",
                ShellDialect::Posix,
            )?,
        };
        Ok(Self { backend })
    }

    pub fn dialect(&self) -> ShellDialect {
        self.backend.dialect
    }

    pub fn backend_name(&self) -> &'static str {
        self.backend.label
    }

    fn unchecked_platform_default() -> Self {
        #[cfg(windows)]
        let backend = ShellBackend {
            program: PathBuf::from("cmd.exe"),
            launch_args: &["/d", "/s", "/c"],
            label: "Command Prompt",
            syntax: "cmd.exe",
            dialect: ShellDialect::Cmd,
        };
        #[cfg(not(windows))]
        let backend = ShellBackend {
            program: PathBuf::from("bash"),
            launch_args: &["--noprofile", "--norc", "-c"],
            label: "Bash",
            syntax: "POSIX shell",
            dialect: ShellDialect::Posix,
        };
        Self { backend }
    }
}

fn detect_default_backend() -> std::result::Result<ShellBackend, String> {
    #[cfg(windows)]
    {
        if let Some(backend) = git_bash_backend() {
            return Ok(backend);
        }
        if let Ok(backend) = named_backend(
            "ps7",
            &["pwsh.exe", "pwsh"],
            &["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"],
            "PowerShell 7",
            "PowerShell",
            ShellDialect::PowerShell,
        ) {
            return Ok(backend);
        }
        if let Ok(backend) = named_backend(
            "powershell",
            &["powershell.exe", "powershell"],
            &["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"],
            "Windows PowerShell",
            "PowerShell",
            ShellDialect::PowerShell,
        ) {
            return Ok(backend);
        }
        return named_backend(
            "cmd",
            &["cmd.exe", "cmd"],
            &["/d", "/s", "/c"],
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
            &["--noprofile", "--norc", "-c"],
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
fn git_bash_backend() -> Option<ShellBackend> {
    let mut candidates = Vec::new();
    for key in ["ProgramFiles", "ProgramFiles(x86)", "LocalAppData"] {
        if let Some(base) = std::env::var_os(key) {
            let base = PathBuf::from(base);
            candidates.push(base.join("Git").join("bin").join("bash.exe"));
            candidates.push(
                base.join("Programs")
                    .join("Git")
                    .join("bin")
                    .join("bash.exe"),
            );
        }
    }
    if let Some(git) = find_on_path("git.exe").or_else(|| find_on_path("git")) {
        if let Some(parent) = git.parent() {
            candidates.push(parent.join("bash.exe"));
            if let Some(root) = parent.parent() {
                candidates.push(root.join("bin").join("bash.exe"));
            }
        }
    }
    let program = candidates.into_iter().find(|path| path.is_file())?;
    Some(ShellBackend {
        program,
        launch_args: &["--noprofile", "--norc", "-c"],
        label: "Git Bash",
        syntax: "POSIX shell",
        dialect: ShellDialect::Posix,
    })
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
        ToolDefinition {
            name: "shell".into(),
            description: format!(
                "Run a {syntax} command with {backend}. Returns its exit status, stdout and \
                 stderr. The call waits for the command to exit. If the result is something you \
                 must have before you can continue — a test suite, a type check, a slow build — \
                 pass `wait: true` and leave `timeout_ms` out: the call then runs to completion, \
                 however long that takes. Without `wait`, a command still running after {default}s \
                 is killed (the ceiling is {max}s). Pass `background: true` for work that outlives \
                 the call — servers, watchers, a build you want to run while you do something \
                 else: it returns a task id immediately and the process keeps running. Commands \
                 that never exit are refused unless `background: true` is given. When a background \
                 task comes from a command that terminates (compile, test), the turn waits up to \
                 the configured budget for it before ending, so the result can land in the same \
                 reply; servers and watchers are never waited on. Filter large output at the \
                 source. Prefer the file tools for filesystem changes because they report the \
                 changed paths.",
                default = DEFAULT_TIMEOUT_MS / 1000,
                max = MAX_TIMEOUT_MS / 1000,
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
                    "timeout_ms": {
                        "type": "integer", "minimum": 1000, "maximum": WAIT_MAX_TIMEOUT_MS,
                        "description": format!(
                            "Optional hard stop in milliseconds; a waiting call without it runs to \
                             completion. Default {DEFAULT_TIMEOUT_MS}, maximum {MAX_TIMEOUT_MS} \
                             unless wait is true."
                        )
                    },
                    "wait": {
                        "type": "boolean",
                        "default": false,
                        "description": "Run until the process exits, however long it takes, and \
                                        return its result in this call. For anything whose result \
                                        you need — tests, type checks, builds. Leave timeout_ms \
                                        out unless you want a hard stop"
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
        // with `wait` it would hang the call forever, and without it the call would be killed at
        // its timeout having reported nothing useful. Background is the one answer that works.
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
        // Past the ceiling the caller has to choose, not be silently clamped: answering a request
        // for ten minutes with a one-minute kill answers a different question. `wait: true` is
        // what raises the ceiling.
        let cap = if a.wait {
            WAIT_MAX_TIMEOUT_MS
        } else {
            MAX_TIMEOUT_MS
        };
        if let Some(ms) = a.timeout_ms
            && ms > cap
        {
            return Ok(ToolExecResult::failed(if a.wait {
                format!(
                    "timeout_ms is capped at {cap}. Leave it out and the command runs to \
                     completion."
                )
            } else {
                format!(
                    "timeout_ms is capped at {cap} unless `wait: true` is passed. If this command's \
                     result is what you need, pass `wait: true` and leave `timeout_ms` out — it \
                     then runs to completion, however long that takes. If the work outlives this \
                     call, pass `background: true`."
                )
            }));
        }
        // `Some` is a hard stop. `None` — only reachable with `wait: true` and no `timeout_ms` —
        // means the call ends when the command does, or when it is cancelled.
        let deadline = if a.wait && a.timeout_ms.is_none() {
            None
        } else {
            Some(Duration::from_millis(
                a.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).min(cap),
            ))
        };
        let timeout = deadline.unwrap_or(Duration::from_millis(DEFAULT_TIMEOUT_MS));

        let mut process = tokio::process::Command::new(&self.backend.program);
        process
            .args(self.backend.launch_args)
            .arg(&command)
            .current_dir(&cwd)
            // Cleared and rebuilt, not amended: `env_remove` per name would need the names in
            // advance, and the point is to filter by *shape*.
            .env_clear()
            .envs(child_env(&ctx.runtime_paths))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);
        // cmd.exe / pwsh / powershell / bash are console apps: from a GUI host (desktop) a plain
        // spawn would open a console window for every shell call. CREATE_NO_WINDOW runs them
        // headless — output still flows through the pipes above.
        #[cfg(windows)]
        process.creation_flags(0x0800_0000);
        // A shell wrapper commonly launches the real server/test process as a child. Giving the
        // command its own group lets cancellation stop that whole tree instead of orphaning the
        // descendant with our stdout/stderr pipes still open.
        #[cfg(unix)]
        process.process_group(0);

        let mut child = match process.spawn() {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolExecResult::failed(format!(
                    "cannot run {} ({}): {e}",
                    self.backend.label,
                    self.backend.program.display()
                )));
            }
        };

        let mut stdout = child.stdout.take().expect("stdout was piped");
        let mut stderr = child.stderr.take().expect("stderr was piped");
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
        // One deadline, or none. `None` only happens for a waiting call that gave no `timeout_ms`:
        // it ends when the command does, or when the turn is cancelled.
        let deadline = deadline.map(|after| tokio::time::Instant::now() + after);
        let mut ending: Option<Ending> = None;
        // Set when the loop stopped because the child was gone while its pipes were not: a
        // descendant is still holding them, and anything it prints from here is not this call's
        // output. Reported rather than silently dropped.
        let mut detached_output = false;

        // Read both pipes as output arrives, so a long command shows progress instead of a blank
        // screen, and so neither pipe can fill and deadlock the child.
        while out_open || err_open {
            tokio::select! {
                n = stdout.read(&mut out_buf), if out_open => match n {
                    Ok(0) | Err(_) => out_open = false,
                    Ok(n) => {
                        let chunk = String::from_utf8_lossy(&out_buf[..n]);
                        ctx.emit(OutputStream::Stdout, &chunk);
                        transcript.push_str(&chunk);
                        out.push(&chunk);
                    }
                },
                n = stderr.read(&mut err_buf), if err_open => match n {
                    Ok(0) | Err(_) => err_open = false,
                    Ok(n) => {
                        let chunk = String::from_utf8_lossy(&err_buf[..n]);
                        ctx.emit(OutputStream::Stderr, &chunk);
                        transcript.push_str(&chunk);
                        err.push(&chunk);
                    }
                },
                // Terminate, then kill. A process that ignores the signal must not be left
                // running: by the time this returns it is dead, and the model's recourse is
                // `wait: true`, a larger timeout_ms, or handing the job to the background.
                _ = tokio::time::sleep_until(
                    deadline.unwrap_or_else(tokio::time::Instant::now)
                ), if deadline.is_some() => {
                    ending = Some(Ending::TimedOut);
                    terminate(&mut child).await;
                    break;
                }
                // Cancellation reaches the child, not just the loop. Output already collected is
                // kept and reported: a killed build's first error is still the answer.
                _ = ctx.cancel.cancelled() => {
                    ending = Some(Ending::Cancelled);
                    terminate(&mut child).await;
                    break;
                }
                // Nothing arrived for a second: if the child is already gone, the pipes it left
                // behind belong to a descendant and will never reach EOF (see [`CHILD_EXIT_POLL`]).
                _ = tokio::time::sleep(CHILD_EXIT_POLL) => {
                    if matches!(child.try_wait(), Ok(Some(_))) {
                        detached_output = true;
                        break;
                    }
                }
            }
        }

        let status = child.wait().await;
        let (status_line, failed) = match (&ending, &status) {
            (Some(Ending::TimedOut), _) => (
                format!("timed out after {}ms and was killed", timeout.as_millis()),
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
            "command: {}\ncwd: {}\nstatus: {status_line}\n",
            summarize_command(&command),
            cwd.display()
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
        if matches!(ending, Some(Ending::TimedOut)) {
            body.push_str(&format!("\n{}", timeout_guidance(a.wait)));
        }
        if detached_output {
            body.push_str(
                "\n--- output may be incomplete ---\n\
                 - The command exited, but a process it started still holds its output stream, so \
                 this call stopped reading rather than wait for an EOF that will never come.\n\
                 - Anything printed after this point is not in the result above.",
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
        } else if matches!(ending, Some(Ending::TimedOut)) {
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
    Cancelled,
}

#[allow(clippy::too_many_arguments)]
async fn adopt_process(
    shell: &Shell,
    ctx: &ToolCtx,
    command: String,
    cwd: PathBuf,
    mut child: tokio::process::Child,
    stdout: tokio::process::ChildStdout,
    stderr: tokio::process::ChildStderr,
) -> Result<ToolExecResult> {
    let Some(tasks) = &ctx.tasks else {
        terminate(&mut child).await;
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

/// Asks the child to stop, then makes it.
async fn terminate(child: &mut tokio::process::Child) {
    // `start_kill` is SIGKILL on Unix in tokio's API, so the polite signal has to be sent
    // directly. Without a graceful phase a shell wrapper dies while its own children keep
    // running, and the output they were about to produce is lost.
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // SIGTERM (15) via `kill`: sending a signal ourselves would mean a `libc` dependency for
        // one call, and `kill` is present on every platform where `bash -c` is.
        let _ = tokio::process::Command::new("kill")
            .arg("-TERM")
            .arg("--")
            .arg(format!("-{pid}"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
        if tokio::time::timeout(SIGKILL_GRACE, child.wait())
            .await
            .is_ok()
        {
            return;
        }
    }
    let _ = child.start_kill();
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
             through grep, head or tail …\n{tail}",
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

fn is_shell_backgrounded(command: &str) -> bool {
    let command = command.trim_end();
    command.ends_with('&') && !command.ends_with("&&")
}

/// What to do when a non-waiting call was killed at its deadline.
/// Leads with the two things that actually help — `background` for something meant to keep
/// running, `wait` **without** a `timeout_ms` for a result that cannot be given up on. The
/// deadline itself is deliberately not the subject here: a call that needs its result should not
/// be choosing a number at all.
fn timeout_guidance(waiting: bool) -> String {
    let mut s = String::from("--- what to do about the timeout ---\n");
    s.push_str(
        "- If it was meant to keep running, retry with background: true so it gets a task id, \
         durable output, and can be stopped.\n",
    );
    s.push_str(if waiting {
        "- This was the hard stop this call asked for. Leave `timeout_ms` out next time and the \
         command runs to completion.\n"
    } else {
        "- If its result is what you need, retry with `wait: true` and no `timeout_ms`: tests, \
         builds and installs should always be run that way. It then runs to completion, however \
         long that takes.\n"
    });
    s.push_str("- If it was waiting for input, it will never get any here.");
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

fn child_env(runtime_paths: &[PathBuf]) -> Vec<(String, String)> {
    let mut env = scrubbed_env();
    if runtime_paths.is_empty() {
        return env;
    }
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
    env
}

/// Whether a variable name looks like a credential.
/// A name-shape denylist rather than a value scan: a value that merely looks random is very often
/// a legitimate build hash or revision, whereas `*_API_KEY` is unambiguous. Nothing here is
/// configurable — a setting that could re-admit these would defeat the point of removing them.
fn is_credential_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.ends_with("_API_KEY")
        || upper.ends_with("_TOKEN")
        || upper.ends_with("_SECRET")
        || upper.ends_with("_PASSWORD")
        || matches!(upper.as_str(), "API_KEY" | "TOKEN" | "SECRET" | "PASSWORD")
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
            terminate(&mut process.child).await;
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
        // The schema offers the waiting ceiling; code enforces the smaller one for a call that
        // is not willing to wait, and says so rather than clamping.
        assert_eq!(
            definition.parameters["properties"]["timeout_ms"]["maximum"],
            json!(WAIT_MAX_TIMEOUT_MS)
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

    /// The timeout kills the process and says what to do instead.
    #[tokio::test]
    async fn a_slow_command_times_out_and_is_killed() {
        let (_d, ctx) = setup();
        let out = Shell::default()
            .execute(
                &ctx,
                &json!({ "command": "sleep 30", "timeout_ms": 1000 }).to_string(),
            )
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Timeout);
        let text = out.model_text();
        assert!(text.contains("timed out"), "{text}");
        assert!(
            text.contains("timeout_ms"),
            "it must say how to proceed: {text}"
        );
    }

    /// Past the ceiling the caller has to choose. Silently clamping a request for ten minutes
    /// into a one-minute kill answers a different question than the one that was asked.
    #[tokio::test]
    async fn a_timeout_above_the_ceiling_is_refused_not_clamped() {
        let (_d, ctx) = setup();
        let out = Shell::default()
            .execute(
                &ctx,
                &json!({ "command": "echo x", "timeout_ms": 600_000 }).to_string(),
            )
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Failed);
        let text = out.model_text();
        assert!(text.contains("wait: true"), "{text}");
        assert!(text.contains("background: true"), "{text}");
    }

    /// The point of `wait`: a result the turn cannot go on without, however long it takes. It is
    /// never handed to the task runtime, and the ceiling that applies to a non-waiting call does
    /// not apply here.
    #[cfg(unix)]
    #[tokio::test]
    async fn wait_runs_to_completion_and_creates_no_task() {
        let (_d, mut ctx) = setup();
        let task_id = TaskId::new();
        ctx.tasks = Some(Arc::new(AdoptingHost { task_id }));

        let out = Shell::default()
            .execute(
                &ctx,
                &json!({
                    "command": "sleep 2; echo done",
                    "wait": true,
                    "timeout_ms": 600_000
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

    /// `wait` with a `timeout_ms` is still a hard stop: waiting for a result is not the same as
    /// promising to wait forever.
    #[cfg(unix)]
    #[tokio::test]
    async fn wait_with_a_timeout_still_kills_a_command_that_overruns_it() {
        let (_d, ctx) = setup();
        let out = Shell::default()
            .execute(
                &ctx,
                &json!({ "command": "sleep 30", "wait": true, "timeout_ms": 1000 }).to_string(),
            )
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Timeout);
        assert!(
            out.model_text().contains("timed out"),
            "{}",
            out.model_text()
        );
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
