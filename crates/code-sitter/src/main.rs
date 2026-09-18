//! CLI for manual testing:
//!   code-sitter <pattern> [path] [--include "*.ts"] [--case-sensitive]
//!            [--max N] [--context N] [--files-only] [--hidden]
//!            [--no-annotate] [--include-ignored]
//! Output: `[kind] path:line: (in fn Qualified.Name) text`
//!   - `[def]`/`[call]`/`[comment]`/`[string]` tag when classified (plain code: no tag)
//!   - context lines (--context N) print as `path-line- text`
//!   - --files-only prints just the matching file paths

use std::path::PathBuf;
use std::process::ExitCode;

use code_sitter::{Options, annotate, search};

fn main() -> ExitCode {
    // Integration mode for an existing grep: read `path\tline[\tcol]` on stdin
    // (col = 0-based byte column, optional), print `symkind\tqualified\thitkind`
    // per line (all fields empty = no annotation). One process handles the
    // whole batch; files are parsed once each.
    if std::env::args().nth(1).as_deref() == Some("--annotate") {
        return annotate_mode();
    }
    // Diagnostic mode: print every distinct tree-sitter node kind a file parses
    // into (sorted). For wiring `def_of` arms of a new language.
    if std::env::args().nth(1).as_deref() == Some("--kinds") {
        let Some(path) = std::env::args().nth(2) else {
            eprintln!("usage: code-sitter --kinds <path>");
            return ExitCode::from(2);
        };
        let probe = PathBuf::from(&path);
        return match code_sitter::node_kinds(&probe) {
            Some(kinds) => {
                for kind in kinds {
                    println!("{kind}");
                }
                ExitCode::SUCCESS
            }
            None => {
                eprintln!("cannot parse {path}: unhandled language, unreadable, or too large");
                ExitCode::FAILURE
            }
        };
    }

    let mut args = std::env::args().skip(1);
    let mut pattern: Option<String> = None;
    let mut root: Option<PathBuf> = None;
    let mut opts = Options::default();

    while let Some(a) = args.next() {
        match a.as_str() {
            "--include" => opts.include = args.next(),
            "--max" => {
                opts.max_total = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(opts.max_total)
            }
            "--context" => {
                opts.context_lines = args.next().and_then(|v| v.parse().ok()).unwrap_or(0)
            }
            "--case-sensitive" => opts.case_sensitive = true,
            "--files-only" => opts.files_only = true,
            "--hidden" => opts.include_hidden = true,
            "--no-annotate" => opts.annotate = false,
            "--include-ignored" => opts.include_ignored = true,
            "-h" | "--help" => {
                eprintln!(
                    "usage: code-sitter <pattern> [path] [--include GLOB] [--case-sensitive] \
                     [--max N] [--context N] [--files-only] [--hidden] [--no-annotate] [--include-ignored]\n\
                     \u{20}      code-sitter --annotate   (stdin: path\\tline[\\tcol] → stdout: symkind\\tqualified\\thitkind)"
                );
                return ExitCode::SUCCESS;
            }
            _ if pattern.is_none() => pattern = Some(a),
            _ if root.is_none() => root = Some(PathBuf::from(a)),
            _ => {
                eprintln!("unexpected arg: {a}");
                return ExitCode::from(2);
            }
        }
    }

    let Some(pattern) = pattern else {
        eprintln!("error: pattern required");
        return ExitCode::from(2);
    };
    let root = root.unwrap_or_else(|| PathBuf::from("."));

    match search(&root, &pattern, &opts) {
        Ok(res) => {
            if res.hits.is_empty() {
                eprintln!("no matches for /{pattern}/ under {}", root.display());
                return ExitCode::SUCCESS;
            }
            if opts.files_only {
                for h in &res.hits {
                    println!("{}", h.path.display());
                }
            } else {
                let mut first = true;
                for h in &res.hits {
                    if opts.context_lines > 0 && !first {
                        println!("--");
                    }
                    first = false;
                    for c in &h.context_before {
                        println!("{}-{}- {}", h.path.display(), c.line, c.text);
                    }
                    let kind_tag = h
                        .hit_kind
                        .and_then(|k| k.label())
                        .map(|l| format!("[{l}] "))
                        .unwrap_or_default();
                    let sym = h
                        .symbol
                        .as_ref()
                        .map(|s| format!(" (in {} {})", s.kind, s.qualified))
                        .unwrap_or_default();
                    println!(
                        "{kind_tag}{}:{}:{} {}",
                        h.path.display(),
                        h.line,
                        sym,
                        h.text
                    );
                    for c in &h.context_after {
                        println!("{}-{}- {}", h.path.display(), c.line, c.text);
                    }
                }
            }
            eprintln!("\n{} hit(s)", res.hits.len());
            if res.truncated {
                eprintln!(
                    "(truncated at {} — narrow the search to see more)",
                    opts.max_total
                );
            }
            if res.skipped_files > 0 {
                eprintln!("(skipped {} unreadable file(s))", res.skipped_files);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// stdin: one `path\tline[\tcol]` per line (col = 0-based byte column, optional;
/// defaults to 0). stdout: one `symkind\tqualified\thitkind` per line (same
/// order; all-empty line when no annotation / unknown language).
fn annotate_mode() -> ExitCode {
    use std::io::{BufRead, Write};

    let stdin = std::io::stdin();
    let mut pairs: Vec<code_sitter::Pair> = Vec::new();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.is_empty() {
            continue;
        }
        let mut it = line.splitn(3, '\t');
        let path = it.next().unwrap_or_default();
        let lnum: u64 = it.next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
        let col: u32 = it.next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
        pairs.push((PathBuf::from(path), lnum, col));
    }

    let anns = annotate(&pairs);
    let out = std::io::stdout();
    let mut w = out.lock();
    for a in anns {
        let (sk, q) = a
            .symbol
            .map(|s| (s.kind, s.qualified))
            .unwrap_or_else(|| (String::new(), String::new()));
        let hk = a.hit_kind.and_then(|k| k.label()).unwrap_or("");
        if sk.is_empty() && hk.is_empty() {
            let _ = writeln!(w);
        } else {
            let _ = writeln!(w, "{sk}\t{q}\t{hk}");
        }
    }
    ExitCode::SUCCESS
}
