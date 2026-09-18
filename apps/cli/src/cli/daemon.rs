//! `zlogic daemon …` — hand the command line over to `zlogic-daemon`.
//!
//! The daemon is a separate program, not part of this crate: this terminal front end is open
//! source, the daemon is not (yet). Released packages ship both binaries next to each other,
//! which is what makes `zlogic daemon …` the way to run it; a build without the daemon says so
//! instead of failing obscurely.
//!
//! Everything after `daemon` belongs to the daemon's own command line (`devices`, `revoke <id>`,
//! `doctor`, `--listen …`, its `--help` included), so the words are passed through verbatim and
//! none of them is parsed here.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// Where the daemon is when it does not sit next to this executable: a system-wide install, a
/// build tree, a packaging step that keeps the two apart.
const DAEMON_ENV: &str = "ZLOGIC_DAEMON";

/// The daemon's file name here. A bare name is also the fallback, which lets the OS search
/// `PATH` for it.
const DAEMON_NAME: &str = if cfg!(windows) {
    "zlogic-daemon.exe"
} else {
    "zlogic-daemon"
};

pub fn run(words: Vec<String>) -> ExitCode {
    let program = resolve(env_override(), exe_dir().as_deref());
    let mut command = Command::new(&program);
    command.args(&words);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        // `exec` rather than spawn: the daemon replaces this process, so it receives signals
        // itself, its exit status is ours, and no wrapper is left behind. Only the failure
        // path returns.
        let error = command.exec();
        report(&program, &error)
    }

    #[cfg(not(unix))]
    {
        match command.status() {
            Ok(status) => match status.code() {
                // The daemon's own exit status: 0 done, anything else failed.
                Some(code) => ExitCode::from(code as u8),
                None => ExitCode::FAILURE,
            },
            Err(error) => report(&program, &error),
        }
    }
}

fn report(program: &Path, error: &std::io::Error) -> ExitCode {
    if error.kind() == ErrorKind::NotFound {
        eprintln!(
            "zlogic: {} not found — this build does not include the daemon.\n\
             \x20       Released packages ship it next to `zlogic`; otherwise point\n\
             \x20       {DAEMON_ENV} at the daemon binary.",
            program.display()
        );
    } else {
        eprintln!("zlogic: cannot run {}: {error}", program.display());
    }
    ExitCode::FAILURE
}

fn env_override() -> Option<PathBuf> {
    let value = std::env::var_os(DAEMON_ENV)?;
    (!value.is_empty()).then(|| PathBuf::from(value))
}

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
}

/// Which daemon to run: an explicit path, else a sibling of this executable, else the bare
/// name (and then the OS searches `PATH`).
fn resolve(explicit: Option<PathBuf>, exe_dir: Option<&Path>) -> PathBuf {
    if let Some(path) = explicit {
        return path;
    }
    if let Some(dir) = exe_dir {
        let sibling = dir.join(DAEMON_NAME);
        if sibling.is_file() {
            return sibling;
        }
    }
    PathBuf::from(DAEMON_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_path_wins() {
        let explicit = PathBuf::from("/somewhere/else/zlogic-daemon");
        assert_eq!(
            resolve(Some(explicit.clone()), Some(Path::new("/usr/bin"))),
            explicit
        );
    }

    #[test]
    fn a_sibling_is_preferred_over_the_path_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let sibling = dir.path().join(DAEMON_NAME);
        std::fs::write(&sibling, b"fake").unwrap();

        assert_eq!(resolve(None, Some(dir.path())), sibling);
    }

    #[test]
    fn no_sibling_falls_back_to_the_bare_name() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(resolve(None, Some(dir.path())), PathBuf::from(DAEMON_NAME));
    }
}
