//! Atomic operations — the closed vocabulary a policy engine matches on.
//! One shell invocation expands into N ops; the caller's merge rule is
//! strictest-wins: any deny → deny, else any ask (Unknown always asks) →
//! ask, else allow.

use crate::path::ResolvedPath;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    Read,
    Write,
    Delete,
}

impl std::fmt::Display for Access {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Access::Read => "read",
            Access::Write => "write",
            Access::Delete => "delete",
        })
    }
}

#[derive(Debug, Clone)]
pub enum Op {
    /// A program invocation, matched by policy on (head, argv).
    Exec {
        /// basename of `argv[0]` — `/usr/bin/git` matches rules for `git`
        head: String,
        argv: Vec<String>,
        /// some argv values are not statically knowable (vars, substitutions,
        /// globs, xargs stdin) — prefix-style grants must not apply
        dynamic_args: bool,
    },
    /// A filesystem access extracted from the command.
    Path { access: Access, path: ResolvedPath },
    /// Execution of a script file — authorization should bind to the file
    /// CONTENT (path + hash), not just the path.
    Script {
        interpreter: String,
        path: ResolvedPath,
    },
    /// `cd`/`pushd`/`popd` — surfaced so `cd ~/.ssh && cat x` is judged on
    /// the resolved target.
    CwdChange { path: ResolvedPath },
    /// Anything we could not model. Fail closed: always ask.
    Unknown { reason: String, snippet: String },
}

impl std::fmt::Display for Op {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Op::Exec {
                head,
                argv,
                dynamic_args,
            } => {
                write!(f, "exec    {}  [{}]", head, argv.join(" "))?;
                if *dynamic_args {
                    write!(f, "  (dynamic args)")?;
                }
                Ok(())
            }
            Op::Path { access, path } => write!(f, "{:<7} {}", access.to_string(), path),
            Op::Script { interpreter, path } => write!(f, "script  {}: {}", interpreter, path),
            Op::CwdChange { path } => write!(f, "cwd     {}", path),
            Op::Unknown { reason, snippet } => {
                write!(f, "opaque  {}", reason)?;
                if !snippet.is_empty() {
                    write!(f, ": {}", snippet)?;
                }
                Ok(())
            }
        }
    }
}
