//! Command string → atomic operations, with a simulated cwd.
//! Cwd rules: `cd <literal>` is tracked; `cd $VAR` / `cd -` / `source` /
//! `popd` past the tracked stack poison the cwd — every later relative path
//! becomes Unresolved (the decision layer must ask). Subshells and `sh -c`
//! payloads run on a CLONED state: their cwd changes never escape, matching
//! process semantics.

use std::path::{Path, PathBuf};

use crate::lexer::{self, RedirKind, Word, WordPart};
use crate::ops::{Access, Op};
use crate::parser::{self, Node, SimpleCommand};
use crate::path::{Cwd, PathResolver, ResolvedPath, Zone, physical_resolve};
use crate::specs::{self, OperandModel, Opt, OptVal, PathSpec, Role, match_opt};

/// Result of peeling one wrapper: where the inner command starts, or a
/// fail-closed stop (the `Unknown` was already emitted).
enum Peel {
    At(usize),
    Bail,
}

/// Bounds `sh -c` / substitution recursion. Deeper nesting is not analyzed —
/// it degrades to Unknown, never to silence.
pub const MAX_DEPTH: usize = 8;

const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "fish"];

/// Env vars whose assignment can hijack what a later command actually
/// executes (loader injection, lookup-path swaps, shell rc override) —
/// a policy match on the command head is meaningless under them.
const DANGEROUS_ENV: &[&str] = &[
    "PATH",
    "IFS",
    "ENV",
    "BASH_ENV",
    "SHELL",
    "CDPATH",
    "PYTHONPATH",
    "PYTHONSTARTUP",
    "NODE_OPTIONS",
    "PERL5OPT",
    "PERL5LIB",
    "RUBYOPT",
    "GIT_SSH",
    "GIT_SSH_COMMAND",
    "GIT_ASKPASS",
    "SSH_ASKPASS",
];

fn check_env_name(name: &str, ops: &mut Vec<Op>) {
    if DANGEROUS_ENV.contains(&name) || name.starts_with("LD_") || name.starts_with("DYLD_") {
        ops.push(Op::Unknown {
            reason: format!("env assignment {name} can hijack what executes"),
            snippet: String::new(),
        });
    }
}
const INTERPS: &[&str] = &[
    "python", "python2", "python3", "node", "nodejs", "ruby", "perl", "php", "deno", "bun", "lua",
];

pub struct Analyzer {
    pub resolver: PathResolver,
}

#[derive(Clone)]
struct State {
    cwd: Cwd,
    dir_stack: Vec<Cwd>,
}

impl Analyzer {
    pub fn new(workspace: PathBuf, home: PathBuf) -> Self {
        Self {
            resolver: PathResolver::new(workspace, home),
        }
    }

    pub fn analyze(&self, input: &str, cwd: &Path) -> Vec<Op> {
        let mut ops = Vec::new();
        let mut st = State {
            cwd: Cwd::Known(physical_resolve(&if cwd.is_absolute() {
                cwd.to_path_buf()
            } else {
                self.resolver.workspace.join(cwd)
            })),
            dir_stack: Vec::new(),
        };
        self.analyze_into(input, &mut st, 0, &mut ops);
        ops
    }

    fn analyze_into(&self, input: &str, st: &mut State, depth: usize, ops: &mut Vec<Op>) {
        if depth > MAX_DEPTH {
            ops.push(Op::Unknown {
                reason: "recursion depth exceeded".into(),
                snippet: trunc(input),
            });
            return;
        }
        let toks = match lexer::lex(input) {
            Ok(t) => t,
            Err(e) => {
                ops.push(Op::Unknown {
                    reason: format!("lex error: {e}"),
                    snippet: trunc(input),
                });
                return;
            }
        };
        let nodes = match parser::parse(&toks) {
            Ok(n) => n,
            Err(e) => {
                ops.push(Op::Unknown {
                    reason: format!("parse error: {e}"),
                    snippet: trunc(input),
                });
                return;
            }
        };
        self.walk(&nodes, st, depth, ops);
    }

    fn walk(&self, nodes: &[Node], st: &mut State, depth: usize, ops: &mut Vec<Op>) {
        for n in nodes {
            match n {
                Node::Opaque { reason } => ops.push(Op::Unknown {
                    reason: reason.clone(),
                    snippet: String::new(),
                }),
                Node::Subshell(inner) => {
                    let mut sub = st.clone();
                    self.walk(inner, &mut sub, depth, ops);
                }
                Node::Cmd(c) => self.simple(c, st, depth, ops),
            }
        }
    }

    fn simple(&self, cmd: &SimpleCommand, st: &mut State, depth: usize, ops: &mut Vec<Op>) {
        // substitutions ANYWHERE in the command execute — surface them first
        for (name, v) in &cmd.assignments {
            check_env_name(name, ops);
            self.recurse_subs(v, st, depth, ops);
        }
        for w in &cmd.words {
            self.recurse_subs(w, st, depth, ops);
        }
        for r in &cmd.redirects {
            if let Some(t) = &r.target {
                self.recurse_subs(t, st, depth, ops);
            }
        }

        // redirect targets are path ops regardless of what the command is
        for r in &cmd.redirects {
            let access = match r.kind {
                RedirKind::In => Some(Access::Read),
                RedirKind::Out | RedirKind::Append => Some(Access::Write),
                // heredoc/herestring targets are data, dups have no path
                _ => None,
            };
            if let (Some(access), Some(t)) = (access, &r.target) {
                ops.push(Op::Path {
                    access,
                    path: self.resolve_word(t, st),
                });
            }
        }

        if cmd.words.is_empty() {
            return;
        }

        let words = &cmd.words;
        let mut idx = 0usize;
        let mut stdin_driven = false; // xargs: real argv arrives via stdin
        // env -C DIR / --chdir DIR runs the wrapped command in DIR; scoped to
        // that command, so it must NOT leak into later commands in the chain
        let mut env_chdir: Option<Cwd> = None;

        // peel wrapper heads down to the real command
        loop {
            let Some(hw) = words.get(idx) else {
                if stdin_driven {
                    ops.push(Op::Unknown {
                        reason: "xargs with stdin-supplied command".into(),
                        snippet: display_words(words),
                    });
                }
                return;
            };
            let Some(h) = hw.as_static() else {
                ops.push(Op::Unknown {
                    reason: "dynamic command head".into(),
                    snippet: display_words(&words[idx..]),
                });
                return;
            };
            let Some(spec) = specs::wrapper_spec(base_name(&h)) else {
                break;
            };
            match self.peel_wrapper(
                &spec,
                base_name(&h),
                words,
                idx,
                st,
                ops,
                &mut env_chdir,
                &mut stdin_driven,
            ) {
                Peel::At(next) => idx = next,
                Peel::Bail => return,
            }
        }

        // Apply an `env -C DIR` scoped chdir on a CLONED state so it resolves
        // this command's relative paths against DIR without leaking to later
        // commands in the chain (env's chdir dies with the wrapped process).
        let mut scoped;
        let st: &mut State = if let Some(newcwd) = env_chdir {
            scoped = st.clone();
            scoped.cwd = newcwd;
            &mut scoped
        } else {
            st
        };

        let Some(hw) = words.get(idx) else { return };
        let Some(h) = hw.as_static() else {
            ops.push(Op::Unknown {
                reason: "dynamic command head".into(),
                snippet: display_words(&words[idx..]),
            });
            return;
        };
        let base = base_name(&h).to_string();
        let args: &[Word] = &words[idx + 1..];

        // running a file by path (./x, /abs/x): authorization must bind to
        // the file's content, so it is a Script op, not just an Exec
        if h.contains('/') {
            ops.push(Op::Script {
                interpreter: "direct".into(),
                path: self.resolve_word(hw, st),
            });
        }

        match base.as_str() {
            "cd" | "pushd" => {
                if base == "pushd" {
                    st.dir_stack.push(st.cwd.clone());
                }
                self.apply_cd(args, st, ops);
                return;
            }
            "popd" => {
                st.cwd = st.dir_stack.pop().unwrap_or(Cwd::Unknown);
                if st.cwd == Cwd::Unknown {
                    ops.push(Op::Unknown {
                        reason: "popd past the tracked dir stack — cwd untracked from here".into(),
                        snippet: String::new(),
                    });
                }
                return;
            }
            _ => {}
        }

        ops.push(Op::Exec {
            head: base.clone(),
            argv: display_argv(&words[idx..]),
            dynamic_args: any_dynamic(args) || stdin_driven,
        });

        // shells & interpreters: the payload is the real behavior
        if SHELLS.contains(&base.as_str()) {
            self.shell_payload(&base, args, st, depth, ops);
            return;
        }
        if INTERPS.contains(&base.as_str()) {
            self.interp_payload(&base, args, st, ops);
            return;
        }
        if base == "source" || base == "." {
            if let Some(w) = first_positional(args) {
                ops.push(Op::Script {
                    interpreter: "source".into(),
                    path: self.resolve_word(w, st),
                });
            }
            // a sourced script runs in THIS shell and may chdir it
            st.cwd = Cwd::Unknown;
            return;
        }

        if !self.known_path_ops(&base, args, st, ops) {
            // unknown command: still surface any static arg that lands in
            // the sensitive floor (conservative, escalate-only)
            for w in positional(args) {
                if let Some(s) = w.as_static() {
                    let rp = self.resolver.resolve(&s, &st.cwd, w.glob);
                    if rp.zone == Zone::Sensitive {
                        ops.push(Op::Path {
                            access: Access::Read,
                            path: rp,
                        });
                    }
                }
            }
        }
    }

    fn apply_cd(&self, args: &[Word], st: &mut State, ops: &mut Vec<Op>) {
        match first_positional(args) {
            None => {
                let home = self.resolver.home.clone();
                let zone = self.resolver.classify(&home);
                st.cwd = Cwd::Known(home.clone());
                ops.push(Op::CwdChange {
                    path: ResolvedPath {
                        requested: "~".into(),
                        resolved: Some(home),
                        zone,
                    },
                });
            }
            Some(w) => match w.as_static() {
                Some(s) if s == "-" => {
                    st.cwd = Cwd::Unknown;
                    ops.push(Op::Unknown {
                        reason: "cd - (previous dir untracked) — cwd unknown from here".into(),
                        snippet: "cd -".into(),
                    });
                }
                Some(s) => {
                    let rp = self.resolver.resolve(&s, &st.cwd, w.glob);
                    st.cwd = match &rp.resolved {
                        Some(p) => Cwd::Known(p.clone()),
                        None => Cwd::Unknown,
                    };
                    ops.push(Op::CwdChange { path: rp });
                }
                None => {
                    st.cwd = Cwd::Unknown;
                    ops.push(Op::Unknown {
                        reason: "cd to dynamic path — relative paths untracked from here".into(),
                        snippet: w.display(),
                    });
                }
            },
        }
    }

    /// `sh -c '<payload>'` recurses (cloned state: a child shell inherits the
    /// cwd but its changes don't propagate back); `sh file.sh` is a Script;
    /// bare `sh` reads stdin — opaque.
    fn shell_payload(
        &self,
        base: &str,
        args: &[Word],
        st: &State,
        depth: usize,
        ops: &mut Vec<Op>,
    ) {
        let mut expects_payload = false;
        let mut payload: Option<&Word> = None;
        let mut script: Option<&Word> = None;
        let mut i = 0usize;
        while i < args.len() {
            let w = &args[i];
            if let Some(s) = w.as_static() {
                if !expects_payload && s.starts_with('-') && s.len() > 1 {
                    // value-taking options: skip the option AND its value, or
                    // the value gets mistaken for the `-c` payload / script
                    if s == "-o" || s == "+o" || s == "--rcfile" || s == "--init-file" {
                        i += 2;
                        continue;
                    }
                    // `-c` is only ever a SHORT flag (`-c`, `-lc`); a long
                    // option that merely contains 'c' (`--norc`, `--restricted`)
                    // must not flip us into payload mode and drop the real `-c`.
                    if !s.starts_with("--") && s.contains('c') {
                        expects_payload = true;
                    }
                    i += 1;
                    continue;
                }
            }
            if expects_payload {
                payload = Some(w);
            } else {
                script = Some(w);
            }
            break;
        }
        if let Some(p) = payload {
            match p.as_static() {
                Some(src) => {
                    let mut sub = st.clone();
                    self.analyze_into(&src, &mut sub, depth + 1, ops);
                }
                None => ops.push(Op::Unknown {
                    reason: format!("{base} -c with dynamic payload"),
                    snippet: p.display(),
                }),
            }
        } else if let Some(s) = script {
            ops.push(Op::Script {
                interpreter: base.to_string(),
                path: self.resolve_word(s, st),
            });
        } else {
            ops.push(Op::Unknown {
                reason: format!("{base} reading commands from stdin"),
                snippet: String::new(),
            });
        }
    }

    /// Non-shell interpreters: a script FILE becomes a Script op (content-
    /// hash authorization); inline code (`-c`/`-e`) is another language we
    /// don't parse — opaque.
    fn interp_payload(&self, base: &str, args: &[Word], st: &State, ops: &mut Vec<Op>) {
        let mut i = 0usize;
        if matches!(base, "deno" | "bun")
            && args.first().and_then(|w| w.as_static()).as_deref() == Some("run")
        {
            i = 1;
        }
        while i < args.len() {
            let w = &args[i];
            if let Some(s) = w.as_static() {
                if matches!(s.as_str(), "-c" | "-e" | "--eval" | "-p") {
                    ops.push(Op::Unknown {
                        reason: format!("inline {base} code (not analyzed)"),
                        snippet: args.get(i + 1).map(|w| w.display()).unwrap_or_default(),
                    });
                    return;
                }
                if s == "-m" {
                    ops.push(Op::Unknown {
                        reason: "module execution (-m), no file path to bind to".into(),
                        snippet: display_words(&args[i..]),
                    });
                    return;
                }
                if s.starts_with('-') && s.len() > 1 {
                    i += 1;
                    continue;
                }
            }
            ops.push(Op::Script {
                interpreter: base.to_string(),
                path: self.resolve_word(w, st),
            });
            return;
        }
        ops.push(Op::Unknown {
            reason: format!("{base} reading code from stdin"),
            snippet: String::new(),
        });
    }

    /// Path semantics for common heads. Returns false when the head is not
    /// modeled (caller then runs the sensitive-floor fallback). Deliberately
    /// NOT exhaustive — anything beyond this table is the decision layer's
    /// problem, not silent trust.
    fn known_path_ops(&self, base: &str, args: &[Word], st: &State, ops: &mut Vec<Op>) -> bool {
        // Bespoke tools that don't fit the positional-role model — kept out of
        // the spec table on purpose (documented in this module):
        match base {
            // sed: `-i`/`--in-place` flips the target from read to write; the
            // suffix can be glued (`-i.bak`), so match by prefix
            "sed" => {
                let inplace = args.iter().any(|w| {
                    w.as_static()
                        .is_some_and(|s| s == "--in-place" || (s.starts_with("-i") && s.len() >= 2))
                });
                let access = if inplace { Access::Write } else { Access::Read };
                let mut files = Vec::new();
                let mut script_from_option = false;
                let mut end = false;
                let mut i = 0usize;
                while i < args.len() {
                    let w = &args[i];
                    let Some(s) = w.as_static() else {
                        files.push(w);
                        i += 1;
                        continue;
                    };
                    if end {
                        files.push(w);
                        i += 1;
                        continue;
                    }
                    if s == "--" {
                        end = true;
                        i += 1;
                        continue;
                    }
                    if matches!(s.as_str(), "-e" | "--expression" | "-f" | "--file") {
                        script_from_option = true;
                        i += 2; // the following word is script text / a script file
                        continue;
                    }
                    if s.starts_with("-e") && s.len() > 2
                        || s.starts_with("-f") && s.len() > 2
                        || s.starts_with("--expression=")
                        || s.starts_with("--file=")
                    {
                        script_from_option = true;
                        i += 1;
                        continue;
                    }
                    if s.starts_with('-') && s.len() > 1 {
                        i += 1;
                        continue;
                    }
                    files.push(w);
                    i += 1;
                }
                let skip = usize::from(!script_from_option);
                for w in files.iter().skip(skip) {
                    ops.push(Op::Path {
                        access,
                        path: self.resolve_word(w, st),
                    });
                }
                return true;
            }
            // dd: paths arrive as `if=`/`of=` operands, not positionals
            "dd" => {
                for w in args {
                    if let Some(s) = w.as_static() {
                        if let Some(f) = s.strip_prefix("if=") {
                            ops.push(Op::Path {
                                access: Access::Read,
                                path: self.resolver.resolve(f, &st.cwd, w.glob),
                            });
                        } else if let Some(f) = s.strip_prefix("of=") {
                            ops.push(Op::Path {
                                access: Access::Write,
                                path: self.resolver.resolve(f, &st.cwd, w.glob),
                            });
                        }
                    } else {
                        let display = w.display();
                        if let Some(f) = display.strip_prefix("if=") {
                            ops.push(Op::Path {
                                access: Access::Read,
                                path: self.resolver.dynamic(f),
                            });
                        } else if let Some(f) = display.strip_prefix("of=") {
                            ops.push(Op::Path {
                                access: Access::Write,
                                path: self.resolver.dynamic(f),
                            });
                        }
                    }
                }
                return true;
            }
            _ => {}
        }

        // everything else goes through the declarative spec table
        match specs::path_spec(base) {
            Some(spec) => {
                self.apply_path_spec(&spec, args, st, ops);
                true
            }
            None => false,
        }
    }

    /// Every `$(...)` / `<(...)` in a word runs commands — analyze them on a
    /// cloned state (substitutions are subshells).
    fn recurse_subs(&self, w: &Word, st: &State, depth: usize, ops: &mut Vec<Op>) {
        for p in &w.parts {
            match p {
                WordPart::CmdSub(body) | WordPart::ProcSub { body, .. } => {
                    let mut sub = st.clone();
                    self.analyze_into(body, &mut sub, depth + 1, ops);
                }
                _ => {}
            }
        }
    }

    fn resolve_word(&self, w: &Word, st: &State) -> ResolvedPath {
        match w.as_static() {
            Some(s) => self.resolver.resolve(&s, &st.cwd, w.glob),
            None => self.resolver.dynamic(&w.display()),
        }
    }

    /// Resolve a directory word into a `Cwd` (for `env -C DIR`): a static path
    /// becomes `Known`, a dynamic one becomes `Unknown` (relative paths then
    /// ask). Mirrors `apply_cd`'s tracking.
    fn cwd_of(&self, w: &Word, st: &State) -> Cwd {
        match w.as_static() {
            Some(s) => self.cwd_from_str(&s, st, w.glob),
            None => Cwd::Unknown,
        }
    }

    fn cwd_from_str(&self, s: &str, st: &State, glob: bool) -> Cwd {
        match self.resolver.resolve(s, &st.cwd, glob).resolved {
            Some(p) => Cwd::Known(p),
            None => Cwd::Unknown,
        }
    }

    fn resolve_optval(&self, v: &OptVal, st: &State) -> Option<ResolvedPath> {
        match v {
            OptVal::None => None,
            OptVal::Word(w) => Some(self.resolve_word(w, st)),
            OptVal::Inline(s) => Some(self.resolver.resolve(s, &st.cwd, false)),
        }
    }

    fn cwd_from_optval(&self, v: &OptVal, st: &State) -> Cwd {
        match v {
            OptVal::None => Cwd::Unknown,
            OptVal::Word(w) => self.cwd_of(w, st),
            OptVal::Inline(s) => self.cwd_from_str(s, st, false),
        }
    }

    /// Peel ONE wrapper (`sudo`/`env`/`timeout`/…) using its [`specs::WrapperSpec`].
    /// Fail-closed invariant: a dynamic token or an option not in the spec makes
    /// us `Bail` (emit `Unknown`) rather than guess which token is the wrapped
    /// command — the whole reason wrappers get strict treatment (the fail-closed rule).
    #[allow(clippy::too_many_arguments)]
    fn peel_wrapper(
        &self,
        spec: &specs::WrapperSpec,
        wname: &str,
        words: &[Word],
        start: usize,
        st: &State,
        ops: &mut Vec<Op>,
        env_chdir: &mut Option<Cwd>,
        stdin_driven: &mut bool,
    ) -> Peel {
        if spec.self_exec {
            ops.push(Op::Exec {
                head: wname.to_string(),
                argv: display_argv(&words[start..]),
                dynamic_args: any_dynamic(&words[start + 1..]),
            });
        }
        if spec.stdin_driven {
            *stdin_driven = true;
        }
        let mut i = start + 1;
        let mut leading_left = spec.leading_positionals;
        while i < words.len() {
            let w = &words[i];
            // env's leading NAME=val (value may be dynamic — check the word, not
            // its static form)
            if spec.assignments && is_env_assign(w) {
                if let Some(WordPart::Lit(l)) = w.parts.first() {
                    if let Some(eq) = l.find('=') {
                        check_env_name(&l[..eq], ops);
                    }
                }
                i += 1;
                continue;
            }
            let Some(s) = w.as_static() else {
                // dynamic token where an option / duration / command could be —
                // we can't tell, so refuse to guess
                ops.push(Op::Unknown {
                    reason: format!(
                        "dynamic token in {wname} arguments — refusing to guess the command"
                    ),
                    snippet: display_words(&words[i..]),
                });
                return Peel::Bail;
            };
            if s == "--" {
                i += 1;
                break;
            }
            if s.starts_with('-') && s.len() > 1 {
                let Some((name, kind, inline)) = match_opt(&s, spec.opts) else {
                    ops.push(Op::Unknown {
                        reason: format!(
                            "unknown option {s} to {wname} — refusing to guess the command head"
                        ),
                        snippet: display_words(&words[i..]),
                    });
                    return Peel::Bail;
                };
                if spec.bail_opts.contains(&name) {
                    ops.push(Op::Unknown {
                        reason: format!("{name} changes {wname} argument parsing (not modeled)"),
                        snippet: display_words(&words[i..]),
                    });
                    return Peel::Bail;
                }
                match kind {
                    Opt::Flag => i += 1,
                    Opt::Value | Opt::ValueRead => {
                        let (val, consumed) = match inline {
                            Some(t) => (OptVal::Inline(t), 1),
                            None => match words.get(i + 1) {
                                Some(v) => (OptVal::Word(v), 2),
                                None => (OptVal::None, 1),
                            },
                        };
                        if spec.chdir_opts.contains(&name) {
                            *env_chdir = Some(self.cwd_from_optval(&val, st));
                        }
                        if matches!(kind, Opt::ValueRead) {
                            if let Some(rp) = self.resolve_optval(&val, st) {
                                ops.push(Op::Path {
                                    access: Access::Read,
                                    path: rp,
                                });
                            }
                        }
                        i += consumed;
                    }
                }
                continue;
            }
            // a bare positional
            if leading_left > 0 {
                leading_left -= 1;
                i += 1;
                continue;
            }
            break; // the command head
        }
        Peel::At(i)
    }

    /// Apply a [`specs::PathSpec`]: scan options (consuming values per the
    /// spec's option table), then map positionals to path roles. Covers the
    /// regular tools; `sed`/`dd` stay bespoke (they don't fit this model).
    fn apply_path_spec(&self, spec: &PathSpec, args: &[Word], st: &State, ops: &mut Vec<Op>) {
        let mut positionals: Vec<&Word> = Vec::new();
        let mut target: Option<ResolvedPath> = None;
        let mut supplied_first_operand = false;
        let mut end = false;
        let mut i = 0usize;
        while i < args.len() {
            let w = &args[i];
            if end {
                positionals.push(w);
                i += 1;
                continue;
            }
            let Some(s) = w.as_static() else {
                // dynamic word: a standalone positional (a valued option's value
                // was already consumed via the next-token path). Resolves to
                // Unresolved, which is the safe direction.
                positionals.push(w);
                i += 1;
                continue;
            };
            if s == "--" {
                end = true;
                i += 1;
                continue;
            }
            if s.starts_with('-') && s.len() > 1 {
                match match_opt(&s, spec.opts) {
                    Some((name, kind, inline)) => match kind {
                        Opt::Flag => i += 1,
                        Opt::Value | Opt::ValueRead => {
                            let (val, consumed) = match inline {
                                Some(t) => (OptVal::Inline(t), 1),
                                None => match args.get(i + 1) {
                                    Some(v) => (OptVal::Word(v), 2),
                                    None => (OptVal::None, 1),
                                },
                            };
                            if spec.target_dir_opts.contains(&name) {
                                target = self.resolve_optval(&val, st);
                            }
                            if first_operand_opts(&spec.operands).contains(&name) {
                                supplied_first_operand = true;
                            }
                            if matches!(kind, Opt::ValueRead) {
                                if let Some(rp) = self.resolve_optval(&val, st) {
                                    ops.push(Op::Path {
                                        access: Access::Read,
                                        path: rp,
                                    });
                                }
                            }
                            i += consumed;
                        }
                    },
                    None => {
                        if spec.strict_unknown_opts {
                            ops.push(Op::Unknown {
                                reason: format!(
                                    "unknown option {s} to path command — continuing conservatively"
                                ),
                                snippet: display_words(args),
                            });
                        }
                        i += 1;
                    }
                }
                continue;
            }
            positionals.push(w);
            i += 1;
        }

        // rsync --delete* / --del: also removes extraneous files in the dest
        let delete_dest = spec.delete_family
            && args.iter().any(|w| {
                w.as_static()
                    .is_some_and(|s| s == "--del" || s.starts_with("--delete"))
            });

        let push_role = |role: Role, w: &Word, ops: &mut Vec<Op>| {
            let rp = self.resolve_word(w, st);
            emit_role(role, rp, ops);
        };

        // -t DIR mode flips LastDest tools: DIR is the dest, all positionals
        // are sources
        if let (Some(dst), OperandModel::LastDest { src, dst: dst_role }) = (&target, spec.operands)
        {
            for w in &positionals {
                push_role(src, w, ops);
            }
            if delete_dest {
                emit_role(Role::Delete, dst.clone(), ops);
            }
            emit_role(dst_role, dst.clone(), ops);
            return;
        }

        match spec.operands {
            OperandModel::AllTargets(r) => {
                for w in &positionals {
                    push_role(r, w, ops);
                }
            }
            OperandModel::PatternThenFiles { .. } => {
                let skip = usize::from(!supplied_first_operand);
                for w in positionals.iter().skip(skip) {
                    push_role(Role::Read, w, ops);
                }
            }
            OperandModel::ModeThenTargets { target, .. } => {
                let skip = usize::from(!supplied_first_operand);
                for w in positionals.iter().skip(skip) {
                    push_role(target, w, ops);
                }
            }
            OperandModel::LastDest { src, dst: dst_role } => {
                if let Some((dst, srcs)) = positionals.split_last() {
                    for w in srcs {
                        push_role(src, w, ops);
                    }
                    if !srcs.is_empty() {
                        if delete_dest {
                            push_role(Role::Delete, dst, ops);
                        }
                        push_role(dst_role, dst, ops);
                    }
                }
            }
        }
    }
}

fn first_operand_opts(model: &OperandModel) -> &'static [&'static str] {
    match model {
        OperandModel::PatternThenFiles { pattern_opts } => pattern_opts,
        OperandModel::ModeThenTargets { mode_opts, .. } => mode_opts,
        _ => &[],
    }
}

/// Emit the `Op::Path`(s) a [`Role`] implies for an already-resolved path.
fn emit_role(role: Role, rp: ResolvedPath, ops: &mut Vec<Op>) {
    match role {
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
    }
}

fn positional(args: &[Word]) -> Vec<&Word> {
    let mut out = Vec::new();
    let mut no_more_flags = false;
    for w in args {
        if !no_more_flags {
            if let Some(s) = w.as_static() {
                if s == "--" {
                    no_more_flags = true;
                    continue;
                }
                if s.starts_with('-') && s.len() > 1 {
                    continue;
                }
            }
        }
        out.push(w);
    }
    out
}

fn first_positional(args: &[Word]) -> Option<&Word> {
    positional(args).into_iter().next()
}

fn any_dynamic(words: &[Word]) -> bool {
    words.iter().any(|w| w.is_dynamic() || w.glob)
}

fn display_words(words: &[Word]) -> String {
    words
        .iter()
        .map(|w| w.display())
        .collect::<Vec<_>>()
        .join(" ")
}

fn display_argv(words: &[Word]) -> Vec<String> {
    words.iter().map(|w| w.display()).collect()
}

fn base_name(h: &str) -> &str {
    h.rsplit('/').next().unwrap_or(h)
}

fn is_env_assign(w: &Word) -> bool {
    let Some(WordPart::Lit(l)) = w.parts.first() else {
        return false;
    };
    let Some(eq) = l.find('=') else {
        return false;
    };
    let name = &l[..eq];
    !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn trunc(s: &str) -> String {
    s.chars().take(80).collect()
}
