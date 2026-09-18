//! policy-explain — dry-run a command through the analyzer and print the
//! atomic operations a policy engine would judge. This is the debugging /
//! audit entry point: policy mistakes are semantic, not syntactic, and are
//! invisible without a trace like this.

use std::env;
use std::process::exit;

use zlogic_policy::{Dialect, decompose};

const USAGE: &str =
    "usage: policy-explain [--dialect posix|cmd|ps] [--cwd DIR] [--workspace DIR] '<command>'

Prints the atomic operations extracted from the shell command:
  exec    program invocation (head + argv)
  read/write/delete  filesystem access, resolved + zone
  script  execution of a script file (bind authorization to content hash)
  cwd     tracked working-directory change
  opaque  not statically modelable — a policy engine must ask

--dialect defaults to posix. For cmd/ps, --cwd/--workspace are Windows
paths (defaults: C:\\workspace) and resolution is lexical.
--workspace defaults to --cwd, --cwd defaults to the current directory.";

fn main() {
    let mut cwd: Option<String> = None;
    let mut workspace: Option<String> = None;
    let mut dialect = String::from("posix");
    let mut cmd: Option<String> = None;

    let mut args = env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--cwd" => cwd = args.next(),
            "--workspace" => workspace = args.next(),
            "--dialect" => dialect = args.next().unwrap_or_default(),
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            _ => {
                cmd = Some(match cmd {
                    Some(c) => c + " " + &a,
                    None => a,
                })
            }
        }
    }
    let Some(cmd) = cmd else {
        eprintln!("{USAGE}");
        exit(2);
    };

    // dialect-native defaults for cwd / workspace / home
    let (dialect, cwd, workspace, home) = match dialect.as_str() {
        "posix" => {
            let cur = env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "/".into());
            let cwd = cwd.unwrap_or_else(|| cur.clone());
            let workspace = workspace.unwrap_or_else(|| cwd.clone());
            let home = env::var("HOME").unwrap_or_else(|_| "/".into());
            (Dialect::Posix, cwd, workspace, home)
        }
        "cmd" | "ps" => {
            let cwd = cwd.unwrap_or_else(|| "C:\\workspace".into());
            let workspace = workspace.unwrap_or_else(|| cwd.clone());
            let home = env::var("USERPROFILE").unwrap_or_else(|_| "C:\\Users\\user".into());
            let d = if dialect == "cmd" {
                Dialect::Cmd
            } else {
                Dialect::PowerShell
            };
            (d, cwd, workspace, home)
        }
        other => {
            eprintln!("unknown dialect '{other}' (posix|cmd|ps)");
            exit(2);
        }
    };

    let ops = decompose(dialect, &cmd, &cwd, &workspace, &home);

    if ops.is_empty() {
        println!("(no operations extracted)");
        return;
    }
    for op in &ops {
        println!("{op}");
    }
}
