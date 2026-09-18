//! cmd.exe dialect.
//! Differences from POSIX that this module encodes (each burned somebody):
//! - `^` is the escape char; `\` is just the path separator;
//! - `&` is a SEQUENCE separator (posix `;`), not background;
//! - `,` `;` `=` are argument separators like whitespace;
//! - only double quotes exist; `%VAR%` expands even INSIDE quotes;
//! - `(...)` groups do NOT scope state — `(cd D:\x) & dir` lists D:\x, so
//!   groups are analyzed on the SAME state (unlike posix subshells);
//! - `cd` without `/d` on another drive changes THAT drive's cwd without
//!   switching; bare `D:` switches drives;
//! - `for` (esp. `/f ... in ('command')`) executes commands — not modeled,
//!   fails closed;
//! - delayed expansion `!VAR!` is assumed OFF (the executor must not pass
//!   /V:ON); `cmd /v` payloads fail closed.

use std::path::PathBuf;

use super::{WinAnalyzer, WinState, analyze_ps_invocation};
use crate::ops::{Access, Op};
use crate::path::{ResolvedPath, Zone};
use crate::winpath::WinResolver;

#[derive(Debug, Clone, PartialEq)]
enum Part {
    Lit(String),
    /// `%NAME%` / `%1` / `%~dp0`
    Var(String),
}

#[derive(Debug, Clone, Default, PartialEq)]
struct CWord {
    parts: Vec<Part>,
    glob: bool,
    quoted: bool,
}

impl CWord {
    fn as_static(&self) -> Option<String> {
        let mut s = String::new();
        for p in &self.parts {
            match p {
                Part::Lit(l) => s.push_str(l),
                Part::Var(_) => return None,
            }
        }
        Some(s)
    }

    fn is_dynamic(&self) -> bool {
        self.as_static().is_none()
    }

    fn display(&self) -> String {
        let mut s = String::new();
        for p in &self.parts {
            match p {
                Part::Lit(l) => s.push_str(l),
                Part::Var(v) => {
                    s.push('%');
                    s.push_str(v);
                    s.push('%');
                }
            }
        }
        s
    }

    fn push_lit(&mut self, c: char) {
        if let Some(Part::Lit(l)) = self.parts.last_mut() {
            l.push(c);
        } else {
            self.parts.push(Part::Lit(c.to_string()));
        }
    }

    fn ensure_part(&mut self) {
        if self.parts.is_empty() {
            self.parts.push(Part::Lit(String::new()));
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum RKind {
    In,
    Out,
    Append,
    Dup,
}

#[derive(Debug)]
enum Tok {
    Word(CWord),
    Sep,
    Redir(RKind),
}

fn fd_ahead(cs: &[char], mut j: usize) -> bool {
    while cs.get(j).is_some_and(|c| c.is_ascii_digit()) {
        j += 1;
    }
    matches!(cs.get(j), Some('<') | Some('>'))
}

fn lex(input: &str) -> Vec<Tok> {
    let cs: Vec<char> = input.chars().collect();
    let mut i = 0usize;
    let mut toks = Vec::new();
    while i < cs.len() {
        let c = cs[i];
        match c {
            ' ' | '\t' | '\r' | ',' | ';' => i += 1,
            '\n' | '(' | ')' => {
                i += 1;
                toks.push(Tok::Sep);
            }
            '&' | '|' => {
                i += 1;
                if cs.get(i) == Some(&c) {
                    i += 1;
                }
                toks.push(Tok::Sep);
            }
            '<' => {
                i += 1;
                toks.push(Tok::Redir(RKind::In));
            }
            '>' => {
                i += 1;
                if cs.get(i) == Some(&'>') {
                    i += 1;
                    toks.push(Tok::Redir(RKind::Append));
                } else if cs.get(i) == Some(&'&') {
                    i += 1;
                    while cs.get(i).is_some_and(|c| c.is_ascii_digit()) {
                        i += 1;
                    }
                    toks.push(Tok::Redir(RKind::Dup));
                } else {
                    toks.push(Tok::Redir(RKind::Out));
                }
            }
            d if d.is_ascii_digit() && fd_ahead(&cs, i) => {
                // fd digits before a redirect — the number itself is unused
                while cs.get(i).is_some_and(|c| c.is_ascii_digit()) {
                    i += 1;
                }
            }
            _ => {
                let w = lex_word(&cs, &mut i);
                if !w.parts.is_empty() {
                    toks.push(Tok::Word(w));
                }
            }
        }
    }
    toks
}

fn lex_word(cs: &[char], i: &mut usize) -> CWord {
    let mut w = CWord::default();
    let mut inq = false;
    while *i < cs.len() {
        let c = cs[*i];
        if !inq
            && matches!(
                c,
                ' ' | '\t' | '\r' | '\n' | ',' | ';' | '&' | '|' | '<' | '>' | '(' | ')'
            )
        {
            break;
        }
        match c {
            '"' => {
                inq = !inq;
                *i += 1;
                w.quoted = true;
                w.ensure_part();
            }
            '^' if !inq => {
                *i += 1;
                if let Some(&n) = cs.get(*i) {
                    *i += 1;
                    if n != '\n' {
                        w.push_lit(n);
                    }
                }
            }
            // %VAR% expands even inside quotes
            '%' => {
                *i += 1;
                match cs.get(*i) {
                    Some('%') => {
                        w.push_lit('%');
                        *i += 1;
                    }
                    // batch parameters: %1 / %* / %~dp0
                    Some(&n) if n.is_ascii_digit() || n == '*' => {
                        w.parts.push(Part::Var(n.to_string()));
                        *i += 1;
                    }
                    Some('~') => {
                        let mut name = String::from("~");
                        *i += 1;
                        while cs.get(*i).is_some_and(|c| c.is_ascii_alphanumeric()) {
                            name.push(cs[*i]);
                            *i += 1;
                        }
                        w.parts.push(Part::Var(name));
                    }
                    _ => {
                        // %NAME% — unterminated % stays literal (cmd behavior)
                        let start = *i;
                        let mut j = *i;
                        while j < cs.len() && !matches!(cs[j], '%' | '\n' | '&' | '|' | '<' | '>') {
                            j += 1;
                        }
                        if cs.get(j) == Some(&'%') && j > start {
                            let name: String = cs[start..j].iter().collect();
                            w.parts.push(Part::Var(name));
                            *i = j + 1;
                        } else {
                            w.push_lit('%');
                        }
                    }
                }
            }
            // wildcards are expanded by the CALLEE (del &c.), quoted or not
            '*' | '?' => {
                w.glob = true;
                w.push_lit(c);
                *i += 1;
            }
            _ => {
                w.push_lit(c);
                *i += 1;
            }
        }
    }
    w
}

pub(crate) fn analyze(
    a: &WinAnalyzer,
    input: &str,
    st: &mut WinState,
    depth: usize,
    ops: &mut Vec<Op>,
) {
    let toks = lex(input);
    let mut words: Vec<CWord> = Vec::new();
    let mut redirs: Vec<(RKind, CWord)> = Vec::new();
    let mut pending: Option<RKind> = None;
    for t in toks {
        match t {
            Tok::Sep => {
                pending = None;
                finish(a, &mut words, &mut redirs, st, depth, ops);
            }
            Tok::Redir(k) => pending = Some(k),
            Tok::Word(w) => {
                if let Some(k) = pending.take() {
                    if k != RKind::Dup {
                        redirs.push((k, w));
                    }
                } else {
                    words.push(w);
                }
            }
        }
    }
    finish(a, &mut words, &mut redirs, st, depth, ops);
}

fn finish(
    a: &WinAnalyzer,
    words: &mut Vec<CWord>,
    redirs: &mut Vec<(RKind, CWord)>,
    st: &mut WinState,
    depth: usize,
    ops: &mut Vec<Op>,
) {
    for (k, t) in redirs.drain(..) {
        let access = match k {
            RKind::In => Access::Read,
            RKind::Out | RKind::Append => Access::Write,
            RKind::Dup => continue,
        };
        if t.as_static().as_deref().is_some_and(WinResolver::is_nul) {
            continue;
        }
        ops.push(Op::Path {
            access,
            path: resolve_word(a, &t, st),
        });
    }
    if words.is_empty() {
        return;
    }
    let taken = std::mem::take(words);
    handle_command(a, &taken, st, depth, ops);
}

fn resolve_word(a: &WinAnalyzer, w: &CWord, st: &WinState) -> ResolvedPath {
    match w.as_static() {
        Some(s) => a.resolver.resolve(&s, &st.cwd, w.glob),
        None => a.resolver.dynamic(&w.display()),
    }
}

fn positional(args: &[CWord]) -> Vec<&CWord> {
    args.iter()
        .filter(|w| !w.as_static().is_some_and(|s| s.starts_with('/')))
        .collect()
}

fn any_dynamic(args: &[CWord]) -> bool {
    args.iter().any(|w| w.is_dynamic() || w.glob)
}

fn displays(words: &[CWord]) -> Vec<String> {
    words.iter().map(|w| w.display()).collect()
}

fn handle_command(
    a: &WinAnalyzer,
    words: &[CWord],
    st: &mut WinState,
    depth: usize,
    ops: &mut Vec<Op>,
) {
    let Some(h) = words[0].as_static() else {
        ops.push(Op::Unknown {
            reason: "dynamic command head".into(),
            snippet: displays(words).join(" "),
        });
        return;
    };
    let hl = h.to_lowercase();

    // bare `D:` switches to that drive's tracked cwd
    if hl.len() == 2 && hl.ends_with(':') && hl.chars().next().unwrap().is_ascii_alphabetic() {
        let d = hl.chars().next().unwrap().to_ascii_uppercase();
        match st.cwd.drive_cwd(d) {
            Some(abs) => {
                let zone = a.resolver.classify(&abs);
                ops.push(Op::CwdChange {
                    path: ResolvedPath {
                        requested: h.clone(),
                        resolved: Some(PathBuf::from(abs.display())),
                        zone,
                    },
                });
                st.cwd.set_current(abs);
            }
            None => {
                st.cwd.current = None;
                ops.push(Op::Unknown {
                    reason: format!("switch to untracked drive {d}: — cwd unknown from here"),
                    snippet: h.clone(),
                });
            }
        }
        return;
    }

    let base_full = h.rsplit(['\\', '/']).next().unwrap_or(&h).to_lowercase();
    let (base, script_ext) = match base_full.rsplit_once('.') {
        Some((stem, "exe" | "com")) => (stem.to_string(), false),
        Some((_, "bat" | "cmd" | "ps1")) => (base_full.clone(), true),
        _ => (base_full.clone(), false),
    };

    // running a script / a file by path — bind authorization to content
    if script_ext || h.contains('\\') || h.contains('/') {
        ops.push(Op::Script {
            interpreter: "cmd".into(),
            path: a.resolver.resolve(&h, &st.cwd, false),
        });
    }

    let args = &words[1..];
    match base.as_str() {
        "rem" => return,
        "cd" | "chdir" => {
            apply_cd(a, args, st, ops, false);
            return;
        }
        "pushd" => {
            st.stack.push(st.cwd.current.clone());
            apply_cd(a, args, st, ops, true);
            return;
        }
        "popd" => {
            match st.stack.pop().flatten() {
                Some(abs) => st.cwd.set_current(abs),
                None => {
                    st.cwd.poison();
                    ops.push(Op::Unknown {
                        reason: "popd past the tracked dir stack — cwd untracked from here".into(),
                        snippet: String::new(),
                    });
                }
            }
            return;
        }
        "if" => {
            handle_if(a, args, st, depth, ops);
            return;
        }
        "for" => {
            ops.push(Op::Unknown {
                reason: "cmd for statement (not modeled — `for /f in ('…')` executes commands)"
                    .into(),
                snippet: displays(words).join(" "),
            });
            return;
        }
        "call" => {
            if !args.is_empty() {
                handle_command(a, args, st, depth, ops);
            }
            return;
        }
        "start" => {
            let mut rest: Vec<CWord> = args
                .iter()
                .filter(|w| !w.as_static().is_some_and(|s| s.starts_with('/')))
                .cloned()
                .collect();
            // The first quoted argument is always START's window title,
            // including the conventional empty title in `start "" cmd`.
            if rest.first().is_some_and(|w| w.quoted) {
                rest.remove(0);
            }
            if !rest.is_empty() {
                handle_command(a, &rest, st, depth, ops);
            }
            return;
        }
        "cmd" => {
            ops.push(Op::Exec {
                head: "cmd".into(),
                argv: displays(words),
                dynamic_args: any_dynamic(args),
            });
            handle_cmd_exe(a, args, st, depth, ops);
            return;
        }
        "powershell" | "pwsh" => {
            ops.push(Op::Exec {
                head: base.clone(),
                argv: displays(words),
                dynamic_args: any_dynamic(args),
            });
            let pairs: Vec<(Option<String>, String)> =
                args.iter().map(|w| (w.as_static(), w.display())).collect();
            analyze_ps_invocation(a, &pairs, st, depth, ops);
            return;
        }
        _ => {}
    }

    ops.push(Op::Exec {
        head: base.clone(),
        argv: displays(words),
        dynamic_args: any_dynamic(args),
    });

    if !known_path_ops(a, &base, args, st, ops) {
        for w in positional(args) {
            let rp = resolve_word(a, w, st);
            if rp.zone == Zone::Sensitive {
                ops.push(Op::Path {
                    access: Access::Read,
                    path: rp,
                });
            }
        }
    }
}

fn apply_cd(a: &WinAnalyzer, args: &[CWord], st: &mut WinState, ops: &mut Vec<Op>, switch: bool) {
    let slash_d = args
        .iter()
        .any(|w| w.as_static().is_some_and(|s| s.eq_ignore_ascii_case("/d")));
    let Some(target) = positional(args).into_iter().next().cloned() else {
        return; // `cd` alone prints the cwd
    };
    let Some(s) = target.as_static() else {
        st.cwd.poison();
        ops.push(Op::Unknown {
            reason: "cd to dynamic path — cwd untracked from here".into(),
            snippet: target.display(),
        });
        return;
    };
    match a.resolver.to_abs(&s, &st.cwd) {
        crate::winpath::AbsResult::Abs(abs) => {
            let zone = a.resolver.classify(&abs);
            ops.push(Op::CwdChange {
                path: ResolvedPath {
                    requested: s.clone(),
                    resolved: Some(PathBuf::from(abs.display())),
                    zone,
                },
            });
            let cur_drive = st.cwd.current.as_ref().and_then(|c| c.drive());
            match abs.drive() {
                // `cd D:\x` without /d changes D's cwd but does NOT switch
                Some(d) if !switch && !slash_d && cur_drive.is_some() && cur_drive != Some(d) => {
                    st.cwd.set_drive(d, abs);
                }
                _ => st.cwd.set_current(abs),
            }
        }
        _ => {
            st.cwd.poison();
            ops.push(Op::Unknown {
                reason: "cd target unresolvable — cwd untracked from here".into(),
                snippet: s,
            });
        }
    }
}

/// `if [not] exist <path> <command>` and friends. The `a==b` comparison form
/// is not recoverable after `=`-as-separator lexing — fails closed.
fn handle_if(a: &WinAnalyzer, args: &[CWord], st: &mut WinState, depth: usize, ops: &mut Vec<Op>) {
    let mut i = 0usize;
    if args
        .get(i)
        .and_then(|w| w.as_static())
        .is_some_and(|s| s.eq_ignore_ascii_case("/i"))
    {
        i += 1;
    }
    if args
        .get(i)
        .and_then(|w| w.as_static())
        .is_some_and(|s| s.eq_ignore_ascii_case("not"))
    {
        i += 1;
    }
    match args
        .get(i)
        .and_then(|w| w.as_static())
        .map(|s| s.to_lowercase())
        .as_deref()
    {
        Some("exist") => {
            if let Some(p) = args.get(i + 1) {
                ops.push(Op::Path {
                    access: Access::Read,
                    path: resolve_word(a, p, st),
                });
            }
            if args.len() > i + 2 {
                handle_command(a, &args[i + 2..], st, depth, ops);
            }
        }
        Some("errorlevel") | Some("defined") | Some("cmdextversion") => {
            if args.len() > i + 2 {
                handle_command(a, &args[i + 2..], st, depth, ops);
            }
        }
        _ => ops.push(Op::Unknown {
            reason: "cmd if comparison (not modeled)".into(),
            snippet: displays(args).join(" "),
        }),
    }
}

fn handle_cmd_exe(a: &WinAnalyzer, args: &[CWord], st: &WinState, depth: usize, ops: &mut Vec<Op>) {
    let mut i = 0usize;
    while i < args.len() {
        let Some(s) = args[i].as_static() else { break };
        let sl = s.to_lowercase();
        if sl.starts_with("/v") {
            ops.push(Op::Unknown {
                reason: "cmd /v delayed expansion — !VAR! semantics not modeled".into(),
                snippet: displays(args).join(" "),
            });
            return;
        }
        if sl == "/c" || sl == "/k" {
            let payload = &args[i + 1..];
            if payload.is_empty() {
                break;
            }
            if payload.iter().all(|w| !w.is_dynamic()) {
                let joined = payload
                    .iter()
                    .map(|w| w.as_static().unwrap())
                    .collect::<Vec<_>>()
                    .join(" ");
                let mut sub = st.clone(); // child process
                a.run_cmd(&joined, &mut sub, depth + 1, ops);
            } else {
                ops.push(Op::Unknown {
                    reason: "cmd /c with dynamic payload".into(),
                    snippet: displays(payload).join(" "),
                });
            }
            return;
        }
        if sl.starts_with('/') {
            i += 1;
            continue;
        }
        break;
    }
    ops.push(Op::Unknown {
        reason: "cmd reading commands from stdin".into(),
        snippet: String::new(),
    });
}

fn known_path_ops(
    a: &WinAnalyzer,
    base: &str,
    args: &[CWord],
    st: &WinState,
    ops: &mut Vec<Op>,
) -> bool {
    let pos = positional(args);
    let push = |access: Access, w: &CWord, ops: &mut Vec<Op>| {
        ops.push(Op::Path {
            access,
            path: resolve_word(a, w, st),
        });
    };
    match base {
        "del" | "erase" | "rd" | "rmdir" => {
            for w in &pos {
                push(Access::Delete, w, ops);
            }
            true
        }
        "type" | "more" | "fc" | "find" | "findstr" => {
            // find/findstr: first positional is the pattern
            let skip = usize::from(matches!(base, "find" | "findstr"));
            for w in pos.iter().skip(skip) {
                push(Access::Read, w, ops);
            }
            true
        }
        "copy" => {
            if let Some((dst, srcs)) = pos.split_last() {
                for s in srcs {
                    push(Access::Read, s, ops);
                }
                if !srcs.is_empty() {
                    push(Access::Write, dst, ops);
                }
            }
            true
        }
        "xcopy" | "robocopy" => {
            if let Some(src) = pos.first() {
                push(Access::Read, src, ops);
            }
            if let Some(dst) = pos.get(1) {
                push(Access::Write, dst, ops);
            }
            true
        }
        "move" | "ren" | "rename" => {
            if let Some(src) = pos.first() {
                push(Access::Read, src, ops);
                push(Access::Delete, src, ops);
            }
            if let Some(dst) = pos.get(1) {
                push(Access::Write, dst, ops);
            }
            true
        }
        "md" | "mkdir" => {
            for w in &pos {
                push(Access::Write, w, ops);
            }
            true
        }
        "mklink" => {
            if let Some(link) = pos.first() {
                push(Access::Write, link, ops);
            }
            if let Some(target) = pos.get(1) {
                push(Access::Read, target, ops);
            }
            true
        }
        "attrib" | "icacls" | "takeown" => {
            for w in &pos {
                push(Access::Write, w, ops);
            }
            true
        }
        _ => false,
    }
}
