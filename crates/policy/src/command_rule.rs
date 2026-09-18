//! User-tier command rules: token-level patterns matched per simple command.
//! A [`CommandRule`] is an explicit, standing authorization written by the
//! USER (the trusted party in this crate's threat model — see lib.rs). When
//! every simple command in an input matches an allow rule, the decision is
//! terminal: op-level analysis and its uncertainty floors are skipped. This
//! is what lets a user pre-approve commands the fail-closed analyzer can
//! never clear on its own (`sudo …`, `bash -c $CMD`) — the alternative is
//! the user disabling permissions entirely, which is strictly worse.
//! Matching is deliberately NOT raw-string matching:
//! - The input is lexed and parsed with the same POSIX lexer/parser the
//!   analyzer uses, and rules match each [`SimpleCommand`] independently.
//!   A wildcard can therefore never swallow a connector — `cat **` does not
//!   match `cat x; rm -rf /` (two commands; each needs its own match).
//! - `*` matches exactly one word, `**` matches any run of remaining words
//!   (only the bare unquoted tokens; `a*` is a literal). Everything else is
//!   matched literally on the word's display form, so an exact pattern like
//!   `bash -c $CMD` matches that command regardless of quoting.
//! - What a wildcard may consume is class-gated, strictest by default:
//!   static words always; words containing `$VAR` expansions or unquoted
//!   globs only when `require_static_args: false`; words containing command
//!   or process substitution (`$(…)`, backticks, `<(…)`) only when
//!   `allow_substitutions: true` — a substitution is another execution
//!   hiding inside the matched command, so it never rides in on a bare `*`.
//! - Redirections and leading `NAME=val` assignments are part of the match:
//!   a pattern without them does not match a command that has them
//!   (`cat *` does not match `cat x > /etc/passwd`).
//! Deny/ask rules are tightening, so their wildcards match ANY word
//! unconditionally — uncertainty must never weaken a restriction.
//! POSIX dialect only: cmd.exe / PowerShell inputs never match command
//! rules and fall through to op-level evaluation unchanged.

use crate::lexer::{self, Word, WordPart};
use crate::parser::{self, Node, Redirect, SimpleCommand};
use crate::policy::Effect;

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandRule {
    pub id: String,
    /// Exactly one simple command. `*` = one word, `**` = the rest.
    pub pattern: String,
    pub effect: Effect,
    /// Default true: wildcards only match fully-static words. Set false to
    /// let them also match words containing `$VAR` / unquoted globs.
    #[serde(default = "default_true")]
    pub require_static_args: bool,
    /// Default false: wildcards never match words containing `$(…)` /
    /// backticks / `<(…)`. Applies only to allow rules (deny/ask always match).
    #[serde(default)]
    pub allow_substitutions: bool,
}

const fn default_true() -> bool {
    true
}

/// Parse a rule pattern. Errors unless it is exactly one simple command with
/// at least one word (no connectors, no subshells, no control flow).
pub(crate) fn compile_pattern(pattern: &str) -> Result<SimpleCommand, String> {
    let toks = lexer::lex(pattern).map_err(|e| format!("invalid pattern: {e}"))?;
    let nodes = parser::parse(&toks).map_err(|e| format!("invalid pattern: {e}"))?;
    match <[Node; 1]>::try_from(nodes) {
        Ok([Node::Cmd(cmd)]) if !cmd.words.is_empty() => Ok(cmd),
        _ => Err("pattern must be a single simple command".into()),
    }
}

/// Parse an input into its simple commands (subshells flattened). `None`
/// when the input does not parse or contains an opaque construct — those
/// must fall through to op-level evaluation, never match.
pub(crate) fn parse_input(command: &str) -> Option<Vec<SimpleCommand>> {
    let toks = lexer::lex(command).ok()?;
    let nodes = parser::parse(&toks).ok()?;
    let mut out = Vec::new();
    collect(&nodes, &mut out).then_some(out)
}

fn collect(nodes: &[Node], out: &mut Vec<SimpleCommand>) -> bool {
    for n in nodes {
        match n {
            Node::Cmd(c) => out.push(c.clone()),
            Node::Subshell(inner) => {
                if !collect(inner, out) {
                    return false;
                }
            }
            Node::Opaque { .. } => return false,
        }
    }
    true
}

pub(crate) fn rule_matches(rule: &CommandRule, cmd: &SimpleCommand) -> bool {
    let Ok(pat) = compile_pattern(&rule.pattern) else {
        return false; // validate() rejects these; never match at runtime
    };
    match_assignments(&pat.assignments, &cmd.assignments)
        && match_redirects(&pat.redirects, &cmd.redirects, rule)
        && match_words(&pat.words, &cmd.words, rule)
}

fn match_assignments(pat: &[(String, Word)], inp: &[(String, Word)]) -> bool {
    pat.len() == inp.len()
        && pat
            .iter()
            .zip(inp)
            .all(|((pn, pv), (inn, iv))| pn == inn && pv.display() == iv.display())
}

fn match_redirects(pat: &[Redirect], inp: &[Redirect], rule: &CommandRule) -> bool {
    pat.len() == inp.len()
        && pat.iter().zip(inp).all(|(p, i)| {
            p.kind == i.kind
                && p.fd == i.fd
                && match (&p.target, &i.target) {
                    (None, None) => true,
                    (Some(pw), Some(iw)) => match_one_word(pw, iw, rule),
                    _ => false,
                }
        })
}

enum Wildcard {
    One,
    Rest,
}

fn wildcard_kind(w: &Word) -> Option<Wildcard> {
    if !w.glob {
        return None; // quoted '*' is a literal
    }
    match w.as_static().as_deref() {
        Some("*") => Some(Wildcard::One),
        Some("**") => Some(Wildcard::Rest),
        _ => None,
    }
}

/// May a wildcard in this rule consume this input word? Class-gated for
/// allow rules; tightening rules (deny/ask) always match.
fn wildcard_can_match(w: &Word, rule: &CommandRule) -> bool {
    if rule.effect != Effect::Allow {
        return true;
    }
    let has_substitution = w
        .parts
        .iter()
        .any(|p| matches!(p, WordPart::CmdSub(_) | WordPart::ProcSub { .. }));
    if has_substitution {
        return rule.allow_substitutions;
    }
    if w.is_dynamic() || w.glob {
        return !rule.require_static_args;
    }
    true
}

fn match_one_word(pat: &Word, inp: &Word, rule: &CommandRule) -> bool {
    match wildcard_kind(pat) {
        Some(_) => wildcard_can_match(inp, rule),
        None => pat.display() == inp.display(),
    }
}

fn match_words(pat: &[Word], inp: &[Word], rule: &CommandRule) -> bool {
    let Some(first) = pat.first() else {
        return inp.is_empty();
    };
    match wildcard_kind(first) {
        Some(Wildcard::Rest) => {
            if match_words(&pat[1..], inp, rule) {
                return true; // `**` consumed zero words
            }
            inp.first().is_some_and(|w| wildcard_can_match(w, rule))
                && match_words(pat, &inp[1..], rule)
        }
        Some(Wildcard::One) => {
            inp.first().is_some_and(|w| wildcard_can_match(w, rule))
                && match_words(&pat[1..], &inp[1..], rule)
        }
        None => {
            inp.first().is_some_and(|w| first.display() == w.display())
                && match_words(&pat[1..], &inp[1..], rule)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(pattern: &str, effect: Effect) -> CommandRule {
        CommandRule {
            id: "t".into(),
            pattern: pattern.into(),
            effect,
            require_static_args: true,
            allow_substitutions: false,
        }
    }

    fn matches(pattern: &str, input: &str) -> bool {
        matches_rule(&rule(pattern, Effect::Allow), input)
    }

    fn matches_rule(r: &CommandRule, input: &str) -> bool {
        let cmds = parse_input(input).unwrap();
        assert_eq!(cmds.len(), 1, "helper expects a single command");
        rule_matches(r, &cmds[0])
    }

    #[test]
    fn exact_and_wildcards() {
        assert!(matches("cat /xx.txt", "cat /xx.txt"));
        assert!(!matches("cat /xx.txt", "cat /yy.txt"));
        assert!(matches("cat *", "cat a.txt"));
        assert!(!matches("cat *", "cat a.txt b.txt")); // * is exactly one word
        assert!(matches("sudo **", "sudo rm -rf /tmp/x"));
        assert!(matches("sudo **", "sudo")); // ** matches zero words
        assert!(matches("git * origin **", "git push origin main --tags"));
        assert!(!matches("npm run *", "npm install"));
    }

    #[test]
    fn quoting_is_normalized_for_literals() {
        assert!(matches("bash -c $CMD", "bash -c \"$CMD\""));
        assert!(matches("cat 'a b.txt'", "cat \"a b.txt\""));
    }

    #[test]
    fn quoted_star_in_pattern_is_literal() {
        assert!(matches("grep '*' file.txt", "grep '*' file.txt"));
        assert!(!matches("grep '*' file.txt", "grep anything file.txt"));
    }

    #[test]
    fn wildcard_word_classes_are_gated() {
        // static-only by default
        assert!(!matches("cat *", "cat $FILE"));
        assert!(!matches("cat *", "cat *.log")); // unquoted glob is not static
        assert!(!matches("cat *", "cat $(rm -rf x)"));
        assert!(!matches("cat **", "cat `rm -rf x`"));
        assert!(!matches("diff * *", "diff <(sort a) b"));

        let mut r = rule("cat *", Effect::Allow);
        r.require_static_args = false;
        assert!(matches_rule(&r, "cat $FILE"));
        assert!(matches_rule(&r, "cat *.log"));
        assert!(!matches_rule(&r, "cat $(rm -rf x)")); // subs need their own gate

        let mut r = rule("cat *", Effect::Allow);
        r.allow_substitutions = true;
        assert!(matches_rule(&r, "cat $(git rev-parse HEAD)"));
        assert!(!matches_rule(&r, "cat $FILE")); // vars still gated separately
    }

    #[test]
    fn tightening_rules_match_everything() {
        let deny = rule("git push --force **", Effect::Deny);
        assert!(matches_rule(&deny, "git push --force origin $BRANCH"));
        assert!(matches_rule(&deny, "git push --force $(target)"));
        let ask = rule("rm *", Effect::Ask);
        assert!(matches_rule(&ask, "rm $TARGET"));
    }

    #[test]
    fn redirects_and_assignments_must_be_declared() {
        assert!(!matches("cat *", "cat x > /etc/passwd"));
        assert!(matches("cat * > out.log", "cat x > out.log"));
        assert!(!matches("cat * > out.log", "cat x >> out.log")); // kind differs
        assert!(!matches("make **", "FOO=1 make all"));
        assert!(matches("FOO=1 make **", "FOO=1 make all"));
        assert!(!matches("FOO=1 make **", "FOO=2 make all"));
    }

    #[test]
    fn connectors_are_never_swallowed() {
        // two commands: the helper's single-command assert would trip, so go direct
        let cmds = parse_input("cat x; rm -rf /").unwrap();
        assert_eq!(cmds.len(), 2);
        let r = rule("cat **", Effect::Allow);
        assert!(rule_matches(&r, &cmds[0]));
        assert!(!rule_matches(&r, &cmds[1]));
    }

    #[test]
    fn opaque_and_unparseable_inputs_never_match() {
        assert!(parse_input("case $x in a) rm foo;; esac").is_none());
        assert!(parse_input("cat 'unterminated").is_none());
    }

    #[test]
    fn pattern_compilation_rules() {
        assert!(compile_pattern("sudo **").is_ok());
        assert!(compile_pattern("a && b").is_err());
        assert!(compile_pattern("(a)").is_err());
        assert!(compile_pattern("").is_err());
        assert!(compile_pattern("FOO=1").is_err()); // needs at least one word
    }
}
