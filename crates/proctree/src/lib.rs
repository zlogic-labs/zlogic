//! The one spawn path for every child process zlogic starts.
//!
//! A bare [`tokio::process::Child`] can only ever stop the process it spawned. Everything here
//! exists to make that untrue. A [`Tree`] carries two guarantees:
//!
//! * **Stopping stops the tree.** On Windows the child is assigned to a job object, so
//!   [`Tree::terminate`] reaches every descendant; on Unix the child leads its own process
//!   group and the stop signal is sent to that group.
//! * **The tree cannot outlive zlogic.** The Windows job carries
//!   `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, so the kernel terminates everything in the job when
//!   the job handle closes — which it does when zlogic exits, however it exits, including a
//!   crash or a forced kill. Dropping a [`Tree`] asks for the same thing deliberately, which is
//!   what covers a tool call that is cancelled or times out mid-`await`. Unix has no kernel
//!   equivalent: the group dies with the [`Tree`], so normal shutdown, cancellation and dropped
//!   calls are covered, but a `SIGKILL` aimed at zlogic itself cannot run any cleanup.
//!
//! [`Tree::wait`] reports the *direct* child's status, matching what `Child::wait` would have
//! returned. A descendant that outlives its parent is cleaned up when the [`Tree`] is dropped
//! rather than by blocking the wait, so a command that leaves something behind cannot hang the
//! call that ran it.

use std::process::{ExitStatus, Output};
use std::time::Duration;

use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
#[cfg(windows)]
use process_wrap::tokio::{CreationFlags, JobObject};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout, Command};
#[cfg(windows)]
use windows::Win32::System::Threading::CREATE_NO_WINDOW;

/// `SIGTERM`, spelled out so the crate needs no `libc` dependency for one call.
#[cfg(unix)]
const SIGTERM: i32 = 15;

/// How long the tree is given to exit by itself after a polite stop request.
#[cfg(unix)]
const TERM_GRACE: Duration = Duration::from_secs(2);

/// Longest [`Tree::wait`] sleeps between checks of the direct child's status.
const WAIT_MAX_INTERVAL: Duration = Duration::from_millis(50);

/// Whether the child is allowed to create a console window on Windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Console {
    /// Start with no console window (`CREATE_NO_WINDOW`). A GUI host flashing a terminal for
    /// every shell call is the reason this exists.
    Hidden,
    /// Leave console creation to the platform default, so inherited stdio stays visible.
    Inherit,
}

/// Configures `command` so the process it starts can be stopped as a whole.
///
/// Used directly where something else owns the child afterwards — the MCP transport, for
/// instance — and by [`Tree::spawn`] everywhere else.
pub fn wrap(command: Command, console: Console) -> CommandWrap {
    let mut wrapped = CommandWrap::from(command);
    #[cfg(windows)]
    {
        if console == Console::Hidden {
            wrapped.wrap(CreationFlags(CREATE_NO_WINDOW));
        }
        wrapped.wrap(KillOnDrop).wrap(JobObject);
    }
    #[cfg(unix)]
    {
        let _ = console;
        wrapped.wrap(KillOnDrop).wrap(ProcessGroup::leader());
    }
    wrapped
}

/// A running child plus whatever it goes on to spawn.
pub struct Tree {
    child: Box<dyn ChildWrapper>,
}

impl Tree {
    /// Starts `command` with its tree already contained.
    pub fn spawn(command: Command, console: Console) -> std::io::Result<Self> {
        Ok(Self {
            child: wrap(command, console).spawn()?,
        })
    }

    /// The direct child's process id, or `None` once it has been reaped.
    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }

    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout().take()
    }

    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.stderr().take()
    }

    /// The direct child's exit status, if it has already exited.
    pub fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    pub fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.stdin().take()
    }

    /// Waits for the direct child to exit.
    ///
    /// Polls rather than using the wrapper's own `wait`, which would block until *every*
    /// process in the job or group is gone — a descendant holding the output pipe would then
    /// hang the call that is only waiting to report an exit code.
    pub async fn wait(&mut self) -> std::io::Result<ExitStatus> {
        let mut interval = Duration::from_millis(1);
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(status);
            }
            tokio::time::sleep(interval).await;
            interval = (interval * 2).min(WAIT_MAX_INTERVAL);
        }
    }

    /// Waits for the direct child and collects what it printed.
    ///
    /// The pipes are read concurrently with the wait, because a child that fills one while
    /// nobody drains it blocks forever. As in [`Self::wait`], only the direct child's exit is
    /// awaited — a descendant that inherited the pipes keeps them open, and holding the wait for
    /// it is what the caller's own timeout is for.
    pub async fn wait_with_output(&mut self) -> std::io::Result<Output> {
        let mut out_pipe = self.take_stdout();
        let mut err_pipe = self.take_stderr();
        let (status, stdout, stderr) = tokio::try_join!(
            self.wait(),
            read_to_end(&mut out_pipe),
            read_to_end(&mut err_pipe),
        )?;
        drop(out_pipe);
        drop(err_pipe);
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    }

    /// Stops the whole tree, asking politely first where the platform has a way to ask.
    ///
    /// Windows has no equivalent of `SIGTERM` for a whole tree, so it is a hard stop there;
    /// Unix sends the group `SIGTERM` first and only escalates once the grace period is up.
    pub async fn terminate(&mut self) {
        #[cfg(unix)]
        {
            if self.child.signal(SIGTERM).is_ok()
                && tokio::time::timeout(TERM_GRACE, self.wait()).await.is_ok()
            {
                return;
            }
        }
        self.start_kill();
        let _ = self.wait().await;
    }

    /// Stops the whole tree immediately, without waiting for it.
    pub fn start_kill(&mut self) {
        let _ = self.child.start_kill();
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        self.start_kill();
    }
}

async fn read_to_end(pipe: &mut Option<impl AsyncRead + Unpin>) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    if let Some(pipe) = pipe.as_mut() {
        pipe.read_to_end(&mut bytes).await?;
    }
    Ok(bytes)
}
