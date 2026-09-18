//! Declarative POSIX command specifications + a generic option scanner.
//! The bug class this module exists to kill: "one option missed → under-
//! report". Previously each tool's argument handling was hand-rolled in a
//! match arm, so forgetting that (say) `env -C` or `xargs -a` consumes a
//! value silently mistook that value for the command head / a positional.
//! Here the ONE thing that must be right — which options consume a value —
//! is declared per tool in a table, and a single scanner applies it. Two
//! deliberate asymmetries, matched to risk (fail-closed):
//! - **Wrappers** (`sudo`, `env`, `xargs`, ...) peel down to an inner command.
//!   An unknown option there is fatal: we cannot know whether it consumes the
//!   next token, so guessing which token is the command head could execute-
//!   analyze the wrong thing. Rule: unknown wrapper option → `Unknown`, stop.
//!   Forgetting to list an option therefore fails CLOSED (an extra ask), never
//!   open.
//! - **Path commands** (`rm`, `cp`, ...) use explicit operand models. The
//!   scanner only decides which tokens are options and which option values are
//!   path reads; the model decides what remaining operands mean. This keeps
//!   command-specific semantics visible (`PatternThenFiles`, `ModeThenTargets`,
//!   `LastDest`) instead of hiding them behind "skip first" folklore.

use crate::lexer::Word;

/// How an option token relates to a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Opt {
    /// Boolean flag; consumes no value.
    Flag,
    /// Consumes the following token (or `=inline` / glued `-xVAL`) as a value
    /// we don't otherwise interpret.
    Value,
    /// Like `Value`, but the value is itself a path that is READ
    /// (`xargs -a FILE`, `grep -f FILE`, `touch -r FILE`).
    ValueRead,
}

/// A consumed option value: a following word, or text glued into the option
/// token (`--chdir=/x`, `-C/x`).
#[derive(Debug, Clone)]
pub(crate) enum OptVal<'a> {
    None,
    Word(&'a Word),
    Inline(String),
}

/// Match one option token against a spec's option table, handling long
/// (`--name`, `--name=val`) and short (`-x`, `-xVAL` glued) forms.
/// Returns the canonical spec name, its kind, and any inline value.
pub(crate) fn match_opt(
    token: &str,
    opts: &[(&'static str, Opt)],
) -> Option<(&'static str, Opt, Option<String>)> {
    if let Some(long) = token.strip_prefix("--") {
        let (name, inline) = match long.split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (long, None),
        };
        for (opt, kind) in opts {
            if let Some(o) = opt.strip_prefix("--") {
                if o == name {
                    return Some((opt, *kind, inline));
                }
            }
        }
        return None;
    }
    if token.starts_with('-') && token.len() > 1 {
        // exact short (`-x`)
        for (opt, kind) in opts {
            if *opt == token {
                return Some((opt, *kind, None));
            }
        }
        // glued short valued (`-xVAL`) — only for options declared valued.
        // The glued form can only match an ASCII option name (every declared
        // option here is ASCII), and a non-ASCII token after `-` must not be
        // byte-sliced: `&token[..2]` would cut through a multi-byte character.
        // Skipping it is fail-closed — the cluster check below then rejects it.
        if token.len() > 2 && token.is_ascii() {
            let short = &token[..2];
            for (opt, kind) in opts {
                if *opt == short && matches!(kind, Opt::Value | Opt::ValueRead) {
                    return Some((opt, *kind, Some(token[2..].to_string())));
                }
            }
            // pure short flag cluster (`-rf`, `-in`). Only match when EVERY
            // character is declared as a boolean flag; otherwise it might be a
            // glued value form or an unknown option, and the caller decides.
            if token[1..].chars().all(|ch| {
                let mut short = String::from("-");
                short.push(ch);
                opts.iter()
                    .any(|(opt, kind)| *kind == Opt::Flag && *opt == short)
            }) {
                return Some(("<flag-cluster>", Opt::Flag, None));
            }
        }
        return None;
    }
    None
}

// ─────────────────────────── wrappers ───────────────────────────

/// A command that runs another command, e.g. `sudo`, `env`, `timeout`.
pub(crate) struct WrapperSpec {
    pub opts: &'static [(&'static str, Opt)],
    /// Emit an `Exec` for the wrapper itself (privilege escalation floor).
    pub self_exec: bool,
    /// The wrapped command's real argv arrives via stdin (`xargs`) → mark the
    /// inner exec's args dynamic.
    pub stdin_driven: bool,
    /// Positional args the wrapper consumes before the command (`timeout`'s
    /// DURATION).
    pub leading_positionals: usize,
    /// Leading `NAME=val` assignments precede the command (`env`).
    pub assignments: bool,
    /// Options whose value is a directory to run the command in (`env -C`).
    pub chdir_opts: &'static [&'static str],
    /// Options whose presence means we cannot model the invocation
    /// (`env -S` re-tokenizes) → `Unknown`.
    pub bail_opts: &'static [&'static str],
}

const WRAP_SUDO: &[(&str, Opt)] = &[
    ("-u", Opt::Value),
    ("--user", Opt::Value),
    ("-g", Opt::Value),
    ("--group", Opt::Value),
    ("-p", Opt::Value),
    ("--prompt", Opt::Value),
    ("-h", Opt::Value),
    ("--host", Opt::Value),
    ("-r", Opt::Value),
    ("--role", Opt::Value),
    ("-t", Opt::Value),
    ("--type", Opt::Value),
    ("-T", Opt::Value),
    ("--command-timeout", Opt::Value),
    ("-R", Opt::Value),
    ("--chroot", Opt::Value),
    ("-U", Opt::Value),
    ("--other-user", Opt::Value),
    ("-C", Opt::Value),
    ("--close-from", Opt::Value),
    ("-n", Opt::Flag),
    ("--non-interactive", Opt::Flag),
    ("-k", Opt::Flag),
    ("-K", Opt::Flag),
    ("-b", Opt::Flag),
    ("--background", Opt::Flag),
    ("-E", Opt::Flag),
    ("--preserve-env", Opt::Flag),
    ("-H", Opt::Flag),
    ("--set-home", Opt::Flag),
    ("-i", Opt::Flag),
    ("--login", Opt::Flag),
    ("-s", Opt::Flag),
    ("--shell", Opt::Flag),
    ("-S", Opt::Flag),
    ("--stdin", Opt::Flag),
    ("-v", Opt::Flag),
    ("--validate", Opt::Flag),
    ("-l", Opt::Flag),
    ("--list", Opt::Flag),
    ("-A", Opt::Flag),
    ("--askpass", Opt::Flag),
    ("-P", Opt::Flag),
    ("--preserve-groups", Opt::Flag),
    ("-N", Opt::Flag),
    ("--no-update", Opt::Flag),
];

const WRAP_ENV: &[(&str, Opt)] = &[
    ("-u", Opt::Value),
    ("--unset", Opt::Value),
    ("-C", Opt::Value),
    ("--chdir", Opt::Value),
    ("-S", Opt::Value),
    ("--split-string", Opt::Value),
    ("-i", Opt::Flag),
    ("--ignore-environment", Opt::Flag),
    ("-0", Opt::Flag),
    ("--null", Opt::Flag),
    ("-v", Opt::Flag),
    ("--debug", Opt::Flag),
];

const WRAP_NICE: &[(&str, Opt)] = &[("-n", Opt::Value), ("--adjustment", Opt::Value)];

const WRAP_TIMEOUT: &[(&str, Opt)] = &[
    ("-s", Opt::Value),
    ("--signal", Opt::Value),
    ("-k", Opt::Value),
    ("--kill-after", Opt::Value),
    ("-v", Opt::Flag),
    ("--verbose", Opt::Flag),
    ("--preserve-status", Opt::Flag),
    ("--foreground", Opt::Flag),
];

const WRAP_STDBUF: &[(&str, Opt)] = &[
    ("-i", Opt::Value),
    ("--input", Opt::Value),
    ("-o", Opt::Value),
    ("--output", Opt::Value),
    ("-e", Opt::Value),
    ("--error", Opt::Value),
];

const WRAP_XARGS: &[(&str, Opt)] = &[
    ("-a", Opt::ValueRead),
    ("--arg-file", Opt::ValueRead),
    ("-E", Opt::Value),
    ("--eof", Opt::Value),
    ("-I", Opt::Value),
    ("--replace", Opt::Value),
    ("-d", Opt::Value),
    ("--delimiter", Opt::Value),
    ("-L", Opt::Value),
    ("-l", Opt::Value),
    ("-n", Opt::Value),
    ("--max-args", Opt::Value),
    ("-P", Opt::Value),
    ("--max-procs", Opt::Value),
    ("-s", Opt::Value),
    ("--max-chars", Opt::Value),
    ("-0", Opt::Flag),
    ("--null", Opt::Flag),
    ("-r", Opt::Flag),
    ("--no-run-if-empty", Opt::Flag),
    ("-t", Opt::Flag),
    ("--verbose", Opt::Flag),
    ("-x", Opt::Flag),
    ("--exit", Opt::Flag),
    ("-p", Opt::Flag),
    ("--interactive", Opt::Flag),
    ("-i", Opt::Flag), // -i has an OPTIONAL arg; marking valued would eat the command
];

const WRAP_COMMAND: &[(&str, Opt)] = &[("-p", Opt::Flag), ("-v", Opt::Flag), ("-V", Opt::Flag)];
const WRAP_EXEC: &[(&str, Opt)] = &[("-a", Opt::Value), ("-c", Opt::Flag), ("-l", Opt::Flag)];
const WRAP_TIME: &[(&str, Opt)] = &[
    ("-o", Opt::Value),
    ("--output", Opt::Value),
    ("-f", Opt::Value),
    ("--format", Opt::Value),
    ("-p", Opt::Flag),
    ("-v", Opt::Flag),
    ("-a", Opt::Flag),
    ("--append", Opt::Flag),
    ("--verbose", Opt::Flag),
    ("--portability", Opt::Flag),
];
const WRAP_SETSID: &[(&str, Opt)] = &[
    ("-f", Opt::Flag),
    ("--fork", Opt::Flag),
    ("-w", Opt::Flag),
    ("--wait", Opt::Flag),
    ("-c", Opt::Flag),
    ("--ctty", Opt::Flag),
];
const NO_OPTS: &[(&str, Opt)] = &[];

pub(crate) fn wrapper_spec(head: &str) -> Option<WrapperSpec> {
    let base =
        |opts, self_exec, stdin_driven, leading_positionals, assignments, chdir_opts, bail_opts| {
            Some(WrapperSpec {
                opts,
                self_exec,
                stdin_driven,
                leading_positionals,
                assignments,
                chdir_opts,
                bail_opts,
            })
        };
    match head {
        "sudo" | "doas" => base(WRAP_SUDO, true, false, 0, false, &[], &[]),
        "env" => base(
            WRAP_ENV,
            false,
            false,
            0,
            true,
            &["-C", "--chdir"],
            &["-S", "--split-string"],
        ),
        "nice" => base(WRAP_NICE, false, false, 0, false, &[], &[]),
        "timeout" => base(WRAP_TIMEOUT, false, false, 1, false, &[], &[]),
        "stdbuf" => base(WRAP_STDBUF, false, false, 0, false, &[], &[]),
        "xargs" => base(WRAP_XARGS, false, true, 0, false, &[], &[]),
        "nohup" => base(NO_OPTS, false, false, 0, false, &[], &[]),
        "setsid" => base(WRAP_SETSID, false, false, 0, false, &[], &[]),
        "command" | "builtin" => base(WRAP_COMMAND, false, false, 0, false, &[], &[]),
        "exec" => base(WRAP_EXEC, false, false, 0, false, &[], &[]),
        "time" => base(WRAP_TIME, false, false, 0, false, &[], &[]),
        _ => None,
    }
}

// ─────────────────────────── path commands ───────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    Read,
    Write,
    Delete,
    ReadDelete,
}

/// How a command's positional operands map to path roles.
#[derive(Debug, Clone, Copy)]
pub(crate) enum OperandModel {
    /// Every remaining operand is a path with the same role.
    AllTargets(Role),
    /// `grep PATTERN FILE...`; options like `-e PATTERN` / `-f PATTERNFILE`
    /// supply the pattern, so no operand should be skipped.
    PatternThenFiles {
        pattern_opts: &'static [&'static str],
    },
    /// `chmod MODE TARGET...`; options like `--reference=RFILE` supply the
    /// mode, so no operand should be skipped.
    ModeThenTargets {
        target: Role,
        mode_opts: &'static [&'static str],
    },
    /// `cp SRC... DST`, `mv SRC... DST`, `ln SRC... DST`.
    LastDest { src: Role, dst: Role },
}

pub(crate) struct PathSpec {
    pub opts: &'static [(&'static str, Opt)],
    pub operands: OperandModel,
    /// `-t DIR` / `--target-directory`: DIR becomes the `last`-role destination
    /// and ALL positionals become `rest`-role sources.
    pub target_dir_opts: &'static [&'static str],
    /// The `rsync --delete*` family: also emit a Delete on the destination.
    pub delete_family: bool,
    /// Unknown options can consume values. For commands where role assignment
    /// is easy to get dangerously wrong, emit Unknown while continuing with a
    /// conservative best-effort parse.
    pub strict_unknown_opts: bool,
}

const P_RM: &[(&str, Opt)] = &[
    ("-r", Opt::Flag),
    ("-R", Opt::Flag),
    ("--recursive", Opt::Flag),
    ("-f", Opt::Flag),
    ("--force", Opt::Flag),
    ("-d", Opt::Flag),
    ("-i", Opt::Flag),
    ("-v", Opt::Flag),
    ("--verbose", Opt::Flag),
];
const P_SHRED: &[(&str, Opt)] = &[
    ("-n", Opt::Value),
    ("--iterations", Opt::Value),
    ("-s", Opt::Value),
    ("--size", Opt::Value),
];
const P_READ: &[(&str, Opt)] = &[];
const P_GREP: &[(&str, Opt)] = &[
    ("-e", Opt::Value),
    ("--regexp", Opt::Value),
    ("-f", Opt::ValueRead),
    ("--file", Opt::ValueRead),
    ("-m", Opt::Value),
    ("--max-count", Opt::Value),
    ("-A", Opt::Value),
    ("--after-context", Opt::Value),
    ("-B", Opt::Value),
    ("--before-context", Opt::Value),
    ("-C", Opt::Value),
    ("--context", Opt::Value),
    ("-d", Opt::Value),
    ("--directories", Opt::Value),
    ("--include", Opt::Value),
    ("--exclude", Opt::Value),
    ("--exclude-dir", Opt::Value),
    ("-i", Opt::Flag),
    ("--ignore-case", Opt::Flag),
    ("-v", Opt::Flag),
    ("--invert-match", Opt::Flag),
    ("-n", Opt::Flag),
    ("--line-number", Opt::Flag),
    ("-r", Opt::Flag),
    ("-R", Opt::Flag),
    ("--recursive", Opt::Flag),
    ("-H", Opt::Flag),
    ("-h", Opt::Flag),
];
const P_RG: &[(&str, Opt)] = &[
    ("-e", Opt::Value),
    ("--regexp", Opt::Value),
    ("-f", Opt::ValueRead),
    ("--file", Opt::ValueRead),
    ("-g", Opt::Value),
    ("--glob", Opt::Value),
    ("-t", Opt::Value),
    ("--type", Opt::Value),
    ("-T", Opt::Value),
    ("--type-not", Opt::Value),
    ("-m", Opt::Value),
    ("--max-count", Opt::Value),
    ("-A", Opt::Value),
    ("--after-context", Opt::Value),
    ("-B", Opt::Value),
    ("--before-context", Opt::Value),
    ("-C", Opt::Value),
    ("--context", Opt::Value),
    ("-i", Opt::Flag),
    ("--ignore-case", Opt::Flag),
    ("-v", Opt::Flag),
    ("--invert-match", Opt::Flag),
    ("-n", Opt::Flag),
    ("--line-number", Opt::Flag),
    ("-S", Opt::Flag),
    ("--smart-case", Opt::Flag),
    ("-u", Opt::Flag),
    ("--unrestricted", Opt::Flag),
    ("--hidden", Opt::Flag),
];
const P_AWK: &[(&str, Opt)] = &[
    ("-v", Opt::Value),
    ("-f", Opt::ValueRead),
    ("-F", Opt::Value),
    ("--field-separator", Opt::Value),
];
const P_TOUCH: &[(&str, Opt)] = &[
    ("-r", Opt::ValueRead),
    ("--reference", Opt::ValueRead),
    ("-d", Opt::Value),
    ("--date", Opt::Value),
    ("-t", Opt::Value),
    ("-a", Opt::Flag),
    ("-m", Opt::Flag),
    ("-c", Opt::Flag),
    ("--no-create", Opt::Flag),
];
const P_MKDIR: &[(&str, Opt)] = &[
    ("-m", Opt::Value),
    ("--mode", Opt::Value),
    ("-p", Opt::Flag),
    ("--parents", Opt::Flag),
    ("-v", Opt::Flag),
    ("--verbose", Opt::Flag),
];
const P_TEE: &[(&str, Opt)] = &[
    ("-a", Opt::Flag),
    ("--append", Opt::Flag),
    ("-i", Opt::Flag),
    ("--ignore-interrupts", Opt::Flag),
];
const P_TRUNCATE: &[(&str, Opt)] = &[
    ("-s", Opt::Value),
    ("--size", Opt::Value),
    ("-r", Opt::ValueRead),
    ("--reference", Opt::ValueRead),
];
const P_CHMOD: &[(&str, Opt)] = &[
    ("--reference", Opt::ValueRead),
    ("--from", Opt::Value),
    ("-R", Opt::Flag),
    ("--recursive", Opt::Flag),
];
const P_CHOWN: &[(&str, Opt)] = &[
    ("--reference", Opt::ValueRead),
    ("-R", Opt::Flag),
    ("--recursive", Opt::Flag),
];
const P_CPMV: &[(&str, Opt)] = &[
    ("-t", Opt::Value),
    ("--target-directory", Opt::Value),
    ("-S", Opt::Value),
    ("--suffix", Opt::Value),
    ("-r", Opt::Flag),
    ("-R", Opt::Flag),
    ("--recursive", Opt::Flag),
    ("-f", Opt::Flag),
    ("--force", Opt::Flag),
    ("-p", Opt::Flag),
    ("-v", Opt::Flag),
    ("--verbose", Opt::Flag),
];
const P_INSTALL: &[(&str, Opt)] = &[
    ("-t", Opt::Value),
    ("--target-directory", Opt::Value),
    ("-S", Opt::Value),
    ("--suffix", Opt::Value),
    ("-m", Opt::Value),
    ("--mode", Opt::Value),
    ("-o", Opt::Value),
    ("--owner", Opt::Value),
    ("-g", Opt::Value),
    ("--group", Opt::Value),
    ("-D", Opt::Flag),
    ("-d", Opt::Flag),
    ("--directory", Opt::Flag),
];
const P_LN: &[(&str, Opt)] = &[
    ("-t", Opt::Value),
    ("--target-directory", Opt::Value),
    ("-S", Opt::Value),
    ("--suffix", Opt::Value),
    ("-s", Opt::Flag),
    ("--symbolic", Opt::Flag),
    ("-f", Opt::Flag),
    ("--force", Opt::Flag),
];
const P_RSYNC: &[(&str, Opt)] = &[
    ("-e", Opt::Value),
    ("--rsh", Opt::Value),
    ("--exclude", Opt::Value),
    ("--include", Opt::Value),
    ("--files-from", Opt::ValueRead),
    ("-f", Opt::Value),
    ("--filter", Opt::Value),
    ("--compare-dest", Opt::Value),
    ("--backup-dir", Opt::Value),
    ("-T", Opt::Value),
    ("--temp-dir", Opt::Value),
    ("--max-size", Opt::Value),
    ("--min-size", Opt::Value),
    ("--bwlimit", Opt::Value),
    ("--timeout", Opt::Value),
    ("--chmod", Opt::Value),
    ("-a", Opt::Flag),
    ("--archive", Opt::Flag),
    ("-r", Opt::Flag),
    ("--recursive", Opt::Flag),
    ("-v", Opt::Flag),
    ("--verbose", Opt::Flag),
    ("-n", Opt::Flag),
    ("--dry-run", Opt::Flag),
    ("-z", Opt::Flag),
    ("--compress", Opt::Flag),
    ("--delete", Opt::Flag),
    ("--del", Opt::Flag),
    ("--delete-before", Opt::Flag),
    ("--delete-during", Opt::Flag),
    ("--delete-delay", Opt::Flag),
    ("--delete-after", Opt::Flag),
    ("--delete-excluded", Opt::Flag),
];

const CAT_FAMILY: &[&str] = &[
    "cat",
    "less",
    "more",
    "head",
    "tail",
    "wc",
    "sort",
    "uniq",
    "strings",
    "xxd",
    "od",
    "file",
    "stat",
    "readlink",
    "du",
    "md5",
    "md5sum",
    "sha1sum",
    "sha256sum",
    "shasum",
    "base64",
    "hexdump",
    "nl",
    "cut",
    "column",
    "diff",
];

pub(crate) fn path_spec(base: &str) -> Option<PathSpec> {
    use Role::*;
    let spec = |opts, operands, target_dir_opts, delete_family, strict_unknown_opts| {
        Some(PathSpec {
            opts,
            operands,
            target_dir_opts,
            delete_family,
            strict_unknown_opts,
        })
    };
    match base {
        "rm" | "rmdir" | "unlink" => spec(P_RM, OperandModel::AllTargets(Delete), &[], false, true),
        "shred" => spec(P_SHRED, OperandModel::AllTargets(Delete), &[], false, true),
        "touch" => spec(P_TOUCH, OperandModel::AllTargets(Write), &[], false, true),
        "mkdir" => spec(P_MKDIR, OperandModel::AllTargets(Write), &[], false, true),
        "tee" => spec(P_TEE, OperandModel::AllTargets(Write), &[], false, true),
        "truncate" => spec(
            P_TRUNCATE,
            OperandModel::AllTargets(Write),
            &[],
            false,
            true,
        ),
        "chmod" => spec(
            P_CHMOD,
            OperandModel::ModeThenTargets {
                target: Write,
                mode_opts: &["--reference"],
            },
            &[],
            false,
            true,
        ),
        "chown" | "chgrp" => spec(
            P_CHOWN,
            OperandModel::ModeThenTargets {
                target: Write,
                mode_opts: &["--reference"],
            },
            &[],
            false,
            true,
        ),
        "grep" | "egrep" | "fgrep" | "ag" | "ack" => spec(
            P_GREP,
            OperandModel::PatternThenFiles {
                pattern_opts: &["-e", "--regexp", "-f", "--file"],
            },
            &[],
            false,
            true,
        ),
        "rg" => spec(
            P_RG,
            OperandModel::PatternThenFiles {
                pattern_opts: &["-e", "--regexp", "-f", "--file"],
            },
            &[],
            false,
            true,
        ),
        "awk" => spec(
            P_AWK,
            OperandModel::PatternThenFiles {
                pattern_opts: &["-f"],
            },
            &[],
            false,
            true,
        ),
        "cp" => spec(
            P_CPMV,
            OperandModel::LastDest {
                src: Read,
                dst: Write,
            },
            &["-t", "--target-directory"],
            false,
            true,
        ),
        "install" => spec(
            P_INSTALL,
            OperandModel::LastDest {
                src: Read,
                dst: Write,
            },
            &["-t", "--target-directory"],
            false,
            true,
        ),
        "mv" => spec(
            P_CPMV,
            OperandModel::LastDest {
                src: ReadDelete,
                dst: Write,
            },
            &["-t", "--target-directory"],
            false,
            true,
        ),
        "ln" => spec(
            P_LN,
            OperandModel::LastDest {
                src: Read,
                dst: Write,
            },
            &["-t", "--target-directory"],
            false,
            true,
        ),
        // rsync's dest is the trailing operand (no -t); --delete* also removes
        "rsync" => spec(
            P_RSYNC,
            OperandModel::LastDest {
                src: Read,
                dst: Write,
            },
            &[],
            true,
            true,
        ),
        _ if CAT_FAMILY.contains(&base) => {
            spec(P_READ, OperandModel::AllTargets(Read), &[], false, false)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_long_short_glued() {
        assert_eq!(
            match_opt("--chdir", WRAP_ENV).map(|(n, k, _)| (n, k)),
            Some(("--chdir", Opt::Value))
        );
        let (n, k, inline) = match_opt("--chdir=/x", WRAP_ENV).unwrap();
        assert_eq!(
            (n, k, inline.as_deref()),
            ("--chdir", Opt::Value, Some("/x"))
        );
        let (n, k, inline) = match_opt("-C/x", WRAP_ENV).unwrap();
        assert_eq!((n, k, inline.as_deref()), ("-C", Opt::Value, Some("/x")));
        assert_eq!(
            match_opt("-i", WRAP_ENV).map(|(n, k, _)| (n, k)),
            Some(("-i", Opt::Flag))
        );
        // unknown
        assert!(match_opt("--nope", WRAP_ENV).is_none());
        // a flag glued with junk is not a valued match
        assert!(match_opt("-iX", WRAP_ENV).is_none());
    }

    /// A short option token whose `-` is followed by a multi-byte character must not panic:
    /// the glued-value branch used to slice `&token[..2]` straight through the character.
    #[test]
    fn non_ascii_short_token_is_rejected_not_sliced() {
        assert!(match_opt("-天气", WRAP_ENV).is_none());
        assert!(match_opt("-x天气", WRAP_ENV).is_none());
    }

    #[test]
    fn every_wrapper_and_path_spec_builds() {
        for w in [
            "sudo", "doas", "env", "nice", "timeout", "stdbuf", "xargs", "nohup", "setsid",
            "command", "builtin", "exec", "time",
        ] {
            assert!(wrapper_spec(w).is_some(), "{w}");
        }
        for p in [
            "rm", "shred", "touch", "truncate", "chmod", "grep", "cp", "mv", "ln", "rsync", "cat",
            "sort",
        ] {
            assert!(path_spec(p).is_some(), "{p}");
        }
        assert!(wrapper_spec("git").is_none());
        assert!(path_spec("git").is_none());
    }
}
