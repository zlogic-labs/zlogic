//! PowerShell dialect — a deliberately CONSERVATIVE subset.
//! PS is a full programming language; we model the command-invocation
//! subset precisely and fail closed on the rest:
//! - aliases are canonicalized first (`rm`/`del`/`ri` → Remove-Item,
//!   `curl` → Invoke-WebRequest) — judging on the alias name would apply
//!   posix intuitions to cmdlets with entirely different parameters;
//! - cmdlet parameters bind by UNAMBIGUOUS PREFIX (`-Rec` = `-Recurse`,
//!   `-LiteralP` = `-LiteralPath`) — matching must be prefix-based or
//!   `-Pat x` slips past a `-Path` extractor;
//! - `$(...)` subexpressions, `(...)` expressions and `{...}` script blocks
//!   are recursed ON THE SAME STATE (one location per runspace — a
//!   `Set-Location` inside `$()` persists, unlike posix subshells);
//! - `Invoke-Expression` is eval — always opaque;
//! - keywords (if/foreach/while/…) emit nothing themselves; their
//!   conditions and bodies surface through the paren/block recursion;
//! - provider paths (`HKLM:` …) are not file paths → `Zone::Provider`,
//!   and `Set-Location` to one poisons the tracked cwd.

use std::path::PathBuf;

use super::{WinAnalyzer, WinState, analyze_ps_invocation};
use crate::ops::{Access, Op};
use crate::path::{ResolvedPath, Zone};
use crate::winpath::{AbsResult, WinResolver};

#[derive(Debug, Clone, PartialEq)]
enum Part {
    Lit(String),
    /// `$name` / `$env:X` / `${...}` — dynamic value
    Var(String),
    /// `$(...)` — subexpression, runs commands
    Sub(String),
    /// standalone `(...)` / `@(...)` — expression, may run commands
    Paren(String),
    /// `{...}` / `@{...}` — script block (executed by the receiving cmdlet)
    Block(String),
}

#[derive(Debug, Clone, Default)]
struct PWord {
    parts: Vec<Part>,
    glob: bool,
}

impl PWord {
    fn as_static(&self) -> Option<String> {
        let mut s = String::new();
        for p in &self.parts {
            match p {
                Part::Lit(l) => s.push_str(l),
                _ => return None,
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
                    s.push('$');
                    s.push_str(v);
                }
                Part::Sub(b) => {
                    s.push_str("$(");
                    s.push_str(b);
                    s.push(')');
                }
                Part::Paren(b) => {
                    s.push('(');
                    s.push_str(b);
                    s.push(')');
                }
                Part::Block(b) => {
                    s.push('{');
                    s.push_str(b);
                    s.push('}');
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

#[derive(Debug)]
enum Tok {
    Word(PWord),
    Sep,
    /// `&` — call operator (PS has no background `&`)
    CallOp,
    Redir,
}

fn scan_balanced(cs: &[char], i: &mut usize, open: char, close: char) -> Result<String, String> {
    let mut depth = 1usize;
    let mut out = String::new();
    let mut sq = false;
    let mut dq = false;
    while *i < cs.len() {
        let c = cs[*i];
        *i += 1;
        if sq {
            if c == '\'' {
                sq = false;
            }
            out.push(c);
            continue;
        }
        if dq {
            if c == '"' {
                dq = false;
            } else if c == '`' {
                out.push(c);
                if let Some(&n) = cs.get(*i) {
                    *i += 1;
                    out.push(n);
                }
                continue;
            }
            out.push(c);
            continue;
        }
        match c {
            '\'' => {
                sq = true;
                out.push(c);
            }
            '"' => {
                dq = true;
                out.push(c);
            }
            '`' => {
                out.push(c);
                if let Some(&n) = cs.get(*i) {
                    *i += 1;
                    out.push(n);
                }
            }
            c if c == open => {
                depth += 1;
                out.push(c);
            }
            c if c == close => {
                depth -= 1;
                if depth == 0 {
                    return Ok(out);
                }
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    Err(format!("unbalanced {open}{close} in powershell source"))
}

fn lex(input: &str) -> Result<Vec<Tok>, String> {
    let cs: Vec<char> = input.chars().collect();
    let mut i = 0usize;
    let mut toks = Vec::new();
    while i < cs.len() {
        let c = cs[i];
        match c {
            ' ' | '\t' | '\r' => i += 1,
            '\n' | ';' => {
                i += 1;
                toks.push(Tok::Sep);
            }
            '#' => {
                while i < cs.len() && cs[i] != '\n' {
                    i += 1;
                }
            }
            '<' => {
                // `<#` block comment; a bare `<` is a PS syntax error anyway
                if cs.get(i + 1) == Some(&'#') {
                    i += 2;
                    while i < cs.len() && !(cs[i] == '#' && cs.get(i + 1) == Some(&'>')) {
                        i += 1;
                    }
                    i = (i + 2).min(cs.len());
                } else {
                    i += 1;
                    toks.push(Tok::Sep);
                }
            }
            '|' => {
                i += 1;
                if cs.get(i) == Some(&'|') {
                    i += 1;
                }
                toks.push(Tok::Sep);
            }
            '&' => {
                if cs.get(i + 1) == Some(&'&') {
                    i += 2;
                    toks.push(Tok::Sep);
                } else {
                    i += 1;
                    toks.push(Tok::CallOp);
                }
            }
            '>' => {
                i += 1;
                if cs.get(i) == Some(&'>') {
                    i += 1;
                }
                if cs.get(i) == Some(&'&') {
                    // 2>&1 — stream merge, no path
                    i += 1;
                    while cs.get(i).is_some_and(|c| c.is_ascii_digit()) {
                        i += 1;
                    }
                } else {
                    toks.push(Tok::Redir);
                }
            }
            d if (d.is_ascii_digit() || d == '*') && cs.get(i + 1) == Some(&'>') => {
                i += 1; // stream number / *, the '>' branch handles the rest
            }
            '(' => {
                i += 1;
                let body = scan_balanced(&cs, &mut i, '(', ')')?;
                toks.push(Tok::Word(PWord {
                    parts: vec![Part::Paren(body)],
                    glob: false,
                }));
            }
            '{' => {
                i += 1;
                let body = scan_balanced(&cs, &mut i, '{', '}')?;
                toks.push(Tok::Word(PWord {
                    parts: vec![Part::Block(body)],
                    glob: false,
                }));
            }
            '@' if matches!(cs.get(i + 1), Some('(') | Some('{')) => {
                let open = cs[i + 1];
                let close = if open == '(' { ')' } else { '}' };
                i += 2;
                let body = scan_balanced(&cs, &mut i, open, close)?;
                let part = if open == '(' {
                    Part::Paren(body)
                } else {
                    Part::Block(body)
                };
                toks.push(Tok::Word(PWord {
                    parts: vec![part],
                    glob: false,
                }));
            }
            ')' | '}' | ',' => {
                i += 1; // stray closers / array separators — argument split
            }
            _ => {
                let w = lex_word(&cs, &mut i)?;
                if !w.parts.is_empty() {
                    toks.push(Tok::Word(w));
                }
            }
        }
    }
    Ok(toks)
}

fn lex_word(cs: &[char], i: &mut usize) -> Result<PWord, String> {
    let mut w = PWord::default();
    while *i < cs.len() {
        let c = cs[*i];
        match c {
            ' ' | '\t' | '\r' | '\n' | ';' | '|' | '&' | '<' | '>' | '#' | '(' | ')' | '{'
            | '}' | ',' => break,
            '\'' => {
                *i += 1;
                let mut s = String::new();
                loop {
                    match cs.get(*i) {
                        None => return Err("unterminated single quote".into()),
                        Some('\'') => {
                            *i += 1;
                            if cs.get(*i) == Some(&'\'') {
                                s.push('\'');
                                *i += 1;
                                continue;
                            }
                            break;
                        }
                        Some(&ch) => {
                            s.push(ch);
                            *i += 1;
                        }
                    }
                }
                for ch in s.chars() {
                    w.push_lit(ch);
                }
                w.ensure_part();
            }
            '"' => {
                *i += 1;
                lex_dq(cs, i, &mut w)?;
            }
            '`' => {
                *i += 1;
                if let Some(&n) = cs.get(*i) {
                    *i += 1;
                    if n != '\n' {
                        w.push_lit(n);
                    }
                }
            }
            '$' => {
                *i += 1;
                lex_dollar(cs, i, &mut w)?;
            }
            '*' | '?' | '[' => {
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
    Ok(w)
}

fn lex_dq(cs: &[char], i: &mut usize, w: &mut PWord) -> Result<(), String> {
    loop {
        match cs.get(*i) {
            None => return Err("unterminated double quote".into()),
            Some('"') => {
                *i += 1;
                if cs.get(*i) == Some(&'"') {
                    w.push_lit('"');
                    *i += 1;
                    continue;
                }
                break;
            }
            Some('`') => {
                *i += 1;
                if let Some(&n) = cs.get(*i) {
                    *i += 1;
                    w.push_lit(n);
                }
            }
            Some('$') => {
                *i += 1;
                lex_dollar(cs, i, w)?;
            }
            Some(&ch) => {
                w.push_lit(ch);
                *i += 1;
            }
        }
    }
    w.ensure_part();
    Ok(())
}

fn lex_dollar(cs: &[char], i: &mut usize, w: &mut PWord) -> Result<(), String> {
    match cs.get(*i) {
        Some('(') => {
            *i += 1;
            let body = scan_balanced(cs, i, '(', ')')?;
            w.parts.push(Part::Sub(body));
        }
        Some('{') => {
            *i += 1;
            let body = scan_balanced(cs, i, '{', '}')?;
            w.parts.push(Part::Var(body));
        }
        Some(&c) if c.is_ascii_alphanumeric() || c == '_' => {
            let mut name = String::new();
            while cs
                .get(*i)
                .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == ':')
            {
                name.push(cs[*i]);
                *i += 1;
            }
            w.parts.push(Part::Var(name));
        }
        Some(&c) if matches!(c, '?' | '^' | '$') => {
            *i += 1;
            w.parts.push(Part::Var(c.to_string()));
        }
        _ => w.push_lit('$'),
    }
    Ok(())
}

const KEYWORDS: &[&str] = &[
    "if", "elseif", "else", "while", "for", "foreach", "switch", "do", "until", "try", "catch",
    "finally", "return", "throw", "param", "begin", "process", "end", "break", "continue", "trap",
    "using", "exit", "in",
];

const ALIASES: &[(&str, &str)] = &[
    ("rm", "remove-item"),
    ("ri", "remove-item"),
    ("del", "remove-item"),
    ("erase", "remove-item"),
    ("rd", "remove-item"),
    ("rmdir", "remove-item"),
    ("ls", "get-childitem"),
    ("dir", "get-childitem"),
    ("gci", "get-childitem"),
    ("cat", "get-content"),
    ("gc", "get-content"),
    ("type", "get-content"),
    ("cp", "copy-item"),
    ("copy", "copy-item"),
    ("cpi", "copy-item"),
    ("mv", "move-item"),
    ("move", "move-item"),
    ("mi", "move-item"),
    ("ren", "rename-item"),
    ("rni", "rename-item"),
    ("cd", "set-location"),
    ("chdir", "set-location"),
    ("sl", "set-location"),
    ("pushd", "push-location"),
    ("popd", "pop-location"),
    ("md", "new-item"),
    ("mkdir", "new-item"),
    ("ni", "new-item"),
    ("zlogic", "write-output"),
    ("write", "write-output"),
    ("iex", "invoke-expression"),
    // Windows PowerShell 5 aliases curl/wget to Invoke-WebRequest
    ("curl", "invoke-webrequest"),
    ("wget", "invoke-webrequest"),
    ("iwr", "invoke-webrequest"),
    ("irm", "invoke-restmethod"),
    ("start", "start-process"),
    ("saps", "start-process"),
    ("%", "foreach-object"),
    ("?", "where-object"),
    ("gi", "get-item"),
    ("ii", "invoke-item"),
    ("tee", "tee-object"),
];

fn canonical(name: &str) -> String {
    let n = name.to_lowercase();
    for (a, c) in ALIASES {
        if *a == n {
            return (*c).to_string();
        }
    }
    n
}

#[derive(Clone, Copy)]
enum Role {
    Read,
    Write,
    Delete,
    ReadDelete,
    Script,
}

struct Spec {
    named: &'static [(&'static str, Role)],
    positional: &'static [Role],
}

/// Common valued parameters that must not be mistaken for positionals.
const COMMON_VALUED: &[&str] = &[
    "erroraction",
    "warningaction",
    "errorvariable",
    "warningvariable",
    "outvariable",
    "informationaction",
    "informationvariable",
    "pipelinevariable",
    "outbuffer",
    "credential",
    "encoding",
    "filter",
    "include",
    "exclude",
    "depth",
    "first",
    "last",
    "skip",
    "itemtype",
    "stream",
    "delimiter",
];

fn cmdlet_spec(canon: &str) -> Option<Spec> {
    use Role::*;
    Some(match canon {
        "remove-item" => Spec {
            named: &[("path", Delete), ("literalpath", Delete)],
            positional: &[Delete],
        },
        "get-content" | "get-item" | "get-childitem" | "test-path" | "import-csv" => Spec {
            named: &[("path", Read), ("literalpath", Read)],
            positional: &[Read],
        },
        "set-content" | "add-content" => Spec {
            named: &[("path", Write), ("literalpath", Write)],
            positional: &[Write],
        },
        "out-file" | "export-csv" => Spec {
            named: &[("filepath", Write), ("path", Write), ("literalpath", Write)],
            positional: &[Write],
        },
        "tee-object" => Spec {
            named: &[("filepath", Write)],
            positional: &[Write],
        },
        "copy-item" => Spec {
            named: &[
                ("path", Read),
                ("literalpath", Read),
                ("destination", Write),
            ],
            positional: &[Read, Write],
        },
        "move-item" => Spec {
            named: &[
                ("path", ReadDelete),
                ("literalpath", ReadDelete),
                ("destination", Write),
            ],
            positional: &[ReadDelete, Write],
        },
        "rename-item" => Spec {
            named: &[
                ("path", ReadDelete),
                ("literalpath", ReadDelete),
                ("newname", Write),
            ],
            positional: &[ReadDelete, Write],
        },
        "new-item" => Spec {
            named: &[("path", Write)],
            positional: &[Write],
        },
        "unblock-file" => Spec {
            named: &[("path", Write)],
            positional: &[Write],
        },
        "invoke-item" => Spec {
            named: &[("path", Script), ("literalpath", Script)],
            positional: &[Script],
        },
        "start-process" => Spec {
            named: &[("filepath", Script)],
            positional: &[Script],
        },
        "invoke-webrequest" | "invoke-restmethod" => Spec {
            named: &[("outfile", Write)],
            positional: &[],
        },
        _ => return None,
    })
}

pub(crate) fn analyze(
    a: &WinAnalyzer,
    input: &str,
    st: &mut WinState,
    depth: usize,
    ops: &mut Vec<Op>,
) {
    let toks = match lex(input) {
        Ok(t) => t,
        Err(e) => {
            ops.push(Op::Unknown {
                reason: format!("powershell lex error: {e}"),
                snippet: input.chars().take(80).collect(),
            });
            return;
        }
    };
    let mut stmt: Vec<Tok> = Vec::new();
    for t in toks {
        if matches!(t, Tok::Sep) {
            handle_statement(a, &stmt, st, depth, ops);
            stmt.clear();
        } else {
            stmt.push(t);
        }
    }
    handle_statement(a, &stmt, st, depth, ops);
}

fn resolve_word(a: &WinAnalyzer, w: &PWord, st: &WinState) -> ResolvedPath {
    match w.as_static() {
        Some(s) => a.resolver.resolve(&s, &st.cwd, w.glob),
        None => a.resolver.dynamic(&w.display()),
    }
}

fn handle_statement(
    a: &WinAnalyzer,
    stmt: &[Tok],
    st: &mut WinState,
    depth: usize,
    ops: &mut Vec<Op>,
) {
    if stmt.is_empty() {
        return;
    }
    // every subexpression / expression / block runs commands — surface them
    // on the SAME state (one location per runspace)
    for t in stmt {
        if let Tok::Word(w) = t {
            for p in &w.parts {
                match p {
                    Part::Sub(b) | Part::Paren(b) | Part::Block(b) => {
                        a.run_ps(b, st, depth + 1, ops)
                    }
                    _ => {}
                }
            }
        }
    }

    let mut words: Vec<&PWord> = Vec::new();
    let mut pending_redir = false;
    let mut saw_call_op = false;
    for t in stmt {
        match t {
            Tok::CallOp => {
                if words.is_empty() {
                    saw_call_op = true;
                }
            }
            Tok::Redir => pending_redir = true,
            Tok::Word(w) => {
                if pending_redir {
                    pending_redir = false;
                    let is_null = w.display() == "$null"
                        || w.as_static().as_deref().is_some_and(WinResolver::is_nul);
                    if !is_null {
                        ops.push(Op::Path {
                            access: Access::Write,
                            path: resolve_word(a, w, st),
                        });
                    }
                } else {
                    words.push(w);
                }
            }
            Tok::Sep => {}
        }
    }
    let Some(head) = words.first() else { return };
    let Some(h0) = head.as_static() else {
        if saw_call_op {
            ops.push(Op::Unknown {
                reason: "call operator (&) with dynamic target".into(),
                snippet: head.display(),
            });
        } else if !head
            .parts
            .iter()
            .all(|p| matches!(p, Part::Paren(_) | Part::Sub(_) | Part::Block(_)))
        {
            ops.push(Op::Unknown {
                reason: "dynamic command head".into(),
                snippet: head.display(),
            });
        }
        // pure expression statements were already recursed above
        return;
    };
    let hl = h0.to_lowercase();
    let args = &words[1..];

    if hl == "." {
        // dot-sourcing runs in THIS scope and may Set-Location
        if let Some(w) = args.first() {
            ops.push(Op::Script {
                interpreter: "ps-dot-source".into(),
                path: resolve_word(a, w, st),
            });
        }
        st.cwd.poison();
        return;
    }
    if KEYWORDS.contains(&hl.as_str()) {
        return; // conditions/bodies surfaced via the part recursion
    }
    if matches!(hl.as_str(), "function" | "filter" | "class" | "workflow") {
        ops.push(Op::Unknown {
            reason: format!("{hl} definition (deferred execution)"),
            snippet: args.first().map(|w| w.display()).unwrap_or_default(),
        });
        return;
    }

    let canon = canonical(&hl);
    match canon.as_str() {
        "invoke-expression" => {
            ops.push(Op::Unknown {
                reason: "Invoke-Expression (eval) — not analyzable".into(),
                snippet: args.first().map(|w| w.display()).unwrap_or_default(),
            });
            return;
        }
        "foreach-object" | "where-object" | "write-output" | "select-object" | "sort-object" => {
            return; // no path semantics; blocks already recursed
        }
        "set-location" | "push-location" => {
            if canon == "push-location" {
                st.stack.push(st.cwd.current.clone());
            }
            apply_cd(a, args, st, ops);
            return;
        }
        "pop-location" => {
            match st.stack.pop().flatten() {
                Some(abs) => st.cwd.set_current(abs),
                None => {
                    st.cwd.poison();
                    ops.push(Op::Unknown {
                        reason: "Pop-Location past the tracked stack — cwd untracked".into(),
                        snippet: String::new(),
                    });
                }
            }
            return;
        }
        "powershell" | "pwsh" => {
            ops.push(Op::Exec {
                head: canon.clone(),
                argv: words.iter().map(|w| w.display()).collect(),
                dynamic_args: args.iter().any(|w| w.is_dynamic() || w.glob),
            });
            let pairs: Vec<(Option<String>, String)> =
                args.iter().map(|w| (w.as_static(), w.display())).collect();
            analyze_ps_invocation(a, &pairs, st, depth, ops);
            return;
        }
        "cmd" => {
            ops.push(Op::Exec {
                head: "cmd".into(),
                argv: words.iter().map(|w| w.display()).collect(),
                dynamic_args: args.iter().any(|w| w.is_dynamic() || w.glob),
            });
            // find /c payload, joined
            if let Some(pos) = args
                .iter()
                .position(|w| w.as_static().is_some_and(|s| s.eq_ignore_ascii_case("/c")))
            {
                let payload = &args[pos + 1..];
                if let Some(joined) = payload
                    .iter()
                    .map(|w| w.as_static())
                    .collect::<Option<Vec<_>>>()
                    .map(|v| v.join(" "))
                {
                    let mut sub = st.clone(); // child process
                    a.run_cmd(&joined, &mut sub, depth + 1, ops);
                } else {
                    ops.push(Op::Unknown {
                        reason: "cmd /c with dynamic payload".into(),
                        snippet: payload
                            .iter()
                            .map(|w| w.display())
                            .collect::<Vec<_>>()
                            .join(" "),
                    });
                }
            } else {
                ops.push(Op::Unknown {
                    reason: "cmd reading commands from stdin".into(),
                    snippet: String::new(),
                });
            }
            return;
        }
        _ => {}
    }

    // script files / external binaries
    let base = canon
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(&canon)
        .to_string();
    let (exec_head, is_script) = match base.rsplit_once('.') {
        Some((stem, "exe" | "com")) => (stem.to_string(), false),
        Some((_, "ps1" | "bat" | "cmd")) => (base.clone(), true),
        _ => (base.clone(), false),
    };
    if is_script || h0.contains('\\') || h0.contains('/') {
        ops.push(Op::Script {
            interpreter: "powershell".into(),
            path: a.resolver.resolve(&h0, &st.cwd, false),
        });
    }

    ops.push(Op::Exec {
        head: exec_head,
        argv: words.iter().map(|w| w.display()).collect(),
        dynamic_args: args.iter().any(|w| w.is_dynamic() || w.glob),
    });

    if let Some(spec) = cmdlet_spec(&canon) {
        bind_params(a, &spec, args, st, ops);
    } else {
        for w in args {
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

fn apply_cd(a: &WinAnalyzer, args: &[&PWord], st: &mut WinState, ops: &mut Vec<Op>) {
    // target: -Path/-LiteralPath value or first positional
    let mut target: Option<&PWord> = None;
    let mut i = 0usize;
    while i < args.len() {
        if let Some(s) = args[i].as_static() {
            if let Some(name) = s.strip_prefix('-') {
                let nl = name.to_lowercase();
                if "path".starts_with(&nl) || "literalpath".starts_with(&nl) {
                    target = args.get(i + 1).copied();
                    break;
                }
                if COMMON_VALUED.iter().any(|v| v.starts_with(&nl)) {
                    i += 2;
                    continue;
                }
                i += 1;
                continue;
            }
        }
        target = Some(args[i]);
        break;
    }
    let Some(t) = target else { return };
    let Some(s) = t.as_static() else {
        st.cwd.poison();
        ops.push(Op::Unknown {
            reason: "Set-Location to dynamic path — cwd untracked from here".into(),
            snippet: t.display(),
        });
        return;
    };
    match a.resolver.to_abs(&s, &st.cwd) {
        AbsResult::Abs(abs) => {
            let zone = a.resolver.classify(&abs);
            ops.push(Op::CwdChange {
                path: ResolvedPath {
                    requested: s,
                    resolved: Some(PathBuf::from(abs.display())),
                    zone,
                },
            });
            st.cwd.set_current(abs); // PS cd always switches drives
        }
        AbsResult::Provider => {
            st.cwd.poison();
            ops.push(Op::Unknown {
                reason: "Set-Location to a provider drive — cwd untracked from here".into(),
                snippet: s,
            });
        }
        AbsResult::Unresolvable => {
            st.cwd.poison();
            ops.push(Op::Unknown {
                reason: "Set-Location target unresolvable — cwd untracked from here".into(),
                snippet: s,
            });
        }
    }
}

fn bind_params(a: &WinAnalyzer, spec: &Spec, args: &[&PWord], st: &WinState, ops: &mut Vec<Op>) {
    let emit = |role: Role, rp: ResolvedPath, ops: &mut Vec<Op>| match role {
        Role::Read => ops.push(Op::Path {
            access: Access::Read,
            path: rp,
        }),
        Role::Write => ops.push(Op::Path {
            access: Access::Write,
            path: rp,
        }),
        Role::Delete => ops.push(Op::Path {
            access: Access::Delete,
            path: rp,
        }),
        Role::ReadDelete => {
            ops.push(Op::Path {
                access: Access::Read,
                path: rp.clone(),
            });
            ops.push(Op::Path {
                access: Access::Delete,
                path: rp,
            });
        }
        Role::Script => ops.push(Op::Script {
            interpreter: "powershell".into(),
            path: rp,
        }),
    };
    let mut pos_idx = 0usize;
    let mut i = 0usize;
    while i < args.len() {
        let w = args[i];
        if let Some(s) = w.as_static() {
            if s.len() > 1 && s.starts_with('-') && s.chars().nth(1).unwrap().is_ascii_alphabetic()
            {
                let full = s[1..].to_lowercase();
                let (name, inline) = match full.split_once(':') {
                    Some((n, v)) => (n.to_string(), Some(v.to_string())),
                    None => (full, None),
                };
                // parameters bind by prefix: -Rec == -Recurse, -LiteralP == -LiteralPath
                if let Some((_, role)) = spec.named.iter().find(|(p, _)| p.starts_with(&name)) {
                    if let Some(v) = inline {
                        let rp = a.resolver.resolve(&v, &st.cwd, false);
                        emit(*role, rp, ops);
                        i += 1;
                    } else if let Some(vw) = args.get(i + 1) {
                        emit(*role, resolve_word(a, vw, st), ops);
                        i += 2;
                    } else {
                        i += 1;
                    }
                    continue;
                }
                if COMMON_VALUED.iter().any(|p| p.starts_with(&name)) {
                    i += if inline.is_some() { 1 } else { 2 };
                    continue;
                }
                i += 1; // unknown parameter → switch
                continue;
            }
        }
        if let Some(role) = spec.positional.get(pos_idx) {
            emit(*role, resolve_word(a, w, st), ops);
            pos_idx += 1;
        }
        i += 1;
    }
}
