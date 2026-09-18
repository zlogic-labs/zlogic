//! Token stream → flat command list.
//! Connectors (`&&` `||` `;` `|` `&`) all mean the same thing to a policy
//! engine — every command in the chain must independently pass — so they are
//! not preserved. Control-flow we cannot model precisely (`case`, function
//! definitions) becomes `Node::Opaque`, which the decision layer must map to
//! "ask"; the commands inside are still surfaced where possible.

use crate::lexer::{RedirKind, Token, Word, WordPart};

#[derive(Debug, Clone)]
pub struct Redirect {
    pub kind: RedirKind,
    pub fd: Option<u32>,
    pub target: Option<Word>,
}

#[derive(Debug, Clone, Default)]
pub struct SimpleCommand {
    pub assignments: Vec<(String, Word)>,
    pub words: Vec<Word>,
    pub redirects: Vec<Redirect>,
}

#[derive(Debug, Clone)]
pub enum Node {
    Cmd(SimpleCommand),
    /// `( ... )` — child process: cwd changes inside do not escape.
    Subshell(Vec<Node>),
    /// A construct we refuse to model — must resolve to ask.
    Opaque {
        reason: String,
    },
}

/// Reserved words that structure control flow but carry no operation of
/// their own; skipping them lets the commands inside if/while/etc. surface.
const RESERVED_SKIP: &[&str] = &[
    "if", "then", "else", "elif", "fi", "while", "until", "do", "done", "{", "}", "!", "time",
    "coproc",
];

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Normal,
    /// after `for` — next word is the loop variable
    ForVar,
    /// after the loop variable — `in` starts the (data) word list
    ForMaybeIn,
    /// inside `in ...` — words are data until the next separator
    ForList,
    /// inside `case ... esac` — skipped wholesale (already reported Opaque)
    CaseBody,
    /// after `function` — next word is the name
    FuncName,
}

pub fn parse(tokens: &[Token]) -> Result<Vec<Node>, String> {
    let mut i = 0usize;
    parse_list(tokens, &mut i, false)
}

fn flush(cur: &mut SimpleCommand, nodes: &mut Vec<Node>) {
    if !cur.words.is_empty() || !cur.assignments.is_empty() || !cur.redirects.is_empty() {
        nodes.push(Node::Cmd(std::mem::take(cur)));
    } else {
        *cur = SimpleCommand::default();
    }
}

fn parse_list(toks: &[Token], i: &mut usize, in_subshell: bool) -> Result<Vec<Node>, String> {
    let mut nodes = Vec::new();
    let mut cur = SimpleCommand::default();
    let mut pending_redir: Option<(RedirKind, Option<u32>)> = None;
    let mut mode = Mode::Normal;

    while *i < toks.len() {
        match &toks[*i] {
            Token::Semi | Token::Amp | Token::And | Token::Or | Token::Pipe => {
                *i += 1;
                if let Some((kind, fd)) = pending_redir.take() {
                    cur.redirects.push(Redirect {
                        kind,
                        fd,
                        target: None,
                    });
                }
                flush(&mut cur, &mut nodes);
                if matches!(mode, Mode::ForVar | Mode::ForMaybeIn | Mode::ForList) {
                    mode = Mode::Normal;
                }
            }
            Token::LParen => {
                if mode == Mode::CaseBody {
                    *i += 1;
                    continue;
                }
                // `name ( )` — function definition
                if cur.words.len() == 1
                    && cur.assignments.is_empty()
                    && matches!(toks.get(*i + 1), Some(Token::RParen))
                {
                    let name = cur.words[0].display();
                    cur = SimpleCommand::default();
                    *i += 2;
                    nodes.push(Node::Opaque {
                        reason: format!("function definition '{}'", name),
                    });
                    continue;
                }
                flush(&mut cur, &mut nodes);
                *i += 1;
                let inner = parse_list(toks, i, true)?;
                nodes.push(Node::Subshell(inner));
            }
            Token::RParen => {
                if mode == Mode::CaseBody {
                    *i += 1;
                    continue;
                }
                *i += 1;
                if in_subshell {
                    if let Some((kind, fd)) = pending_redir.take() {
                        cur.redirects.push(Redirect {
                            kind,
                            fd,
                            target: None,
                        });
                    }
                    flush(&mut cur, &mut nodes);
                    return Ok(nodes);
                }
                // stray ')' (e.g. an unskipped case pattern) — ignore
            }
            Token::Redir { kind, fd } => {
                *i += 1;
                if mode == Mode::CaseBody {
                    continue;
                }
                pending_redir = Some((*kind, *fd));
            }
            Token::Word(w) => {
                *i += 1;
                if mode == Mode::CaseBody {
                    if w.as_static().as_deref() == Some("esac") {
                        mode = Mode::Normal;
                    }
                    continue;
                }
                if let Some((kind, fd)) = pending_redir.take() {
                    cur.redirects.push(Redirect {
                        kind,
                        fd,
                        target: Some(w.clone()),
                    });
                    continue;
                }
                match mode {
                    Mode::FuncName => {
                        mode = Mode::Normal;
                        continue;
                    }
                    Mode::ForVar => {
                        mode = Mode::ForMaybeIn;
                        continue;
                    }
                    Mode::ForMaybeIn => {
                        if w.as_static().as_deref() == Some("in") {
                            mode = Mode::ForList;
                            continue;
                        }
                        mode = Mode::Normal; // `for x; do …` — fall through
                    }
                    Mode::ForList => continue,
                    Mode::Normal | Mode::CaseBody => {}
                }
                if cur.words.is_empty() {
                    if let Some(s) = w.as_static() {
                        if RESERVED_SKIP.contains(&s.as_str()) {
                            continue;
                        }
                        match s.as_str() {
                            "for" => {
                                mode = Mode::ForVar;
                                continue;
                            }
                            "case" => {
                                nodes.push(Node::Opaque {
                                    reason: "case statement (not modeled)".into(),
                                });
                                mode = Mode::CaseBody;
                                continue;
                            }
                            "function" => {
                                nodes.push(Node::Opaque {
                                    reason: "function definition".into(),
                                });
                                mode = Mode::FuncName;
                                continue;
                            }
                            _ => {}
                        }
                    }
                    if let Some((name, val)) = split_assignment(w) {
                        cur.assignments.push((name, val));
                        continue;
                    }
                }
                cur.words.push(w.clone());
            }
        }
    }
    if let Some((kind, fd)) = pending_redir.take() {
        cur.redirects.push(Redirect {
            kind,
            fd,
            target: None,
        });
    }
    flush(&mut cur, &mut nodes);
    if in_subshell {
        return Err("unterminated subshell".into());
    }
    Ok(nodes)
}

/// `NAME=value` at command start. The value keeps its dynamic parts.
fn split_assignment(w: &Word) -> Option<(String, Word)> {
    let WordPart::Lit(first) = w.parts.first()? else {
        return None;
    };
    let eq = first.find('=')?;
    let name = &first[..eq];
    let head_ok = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    if name.is_empty() || !head_ok || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    let mut val = Word {
        parts: Vec::new(),
        glob: w.glob,
    };
    let rest = &first[eq + 1..];
    if !rest.is_empty() {
        val.parts.push(WordPart::Lit(rest.to_string()));
    }
    val.parts.extend(w.parts.iter().skip(1).cloned());
    Some((name.to_string(), val))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;

    fn parse_str(s: &str) -> Vec<Node> {
        parse(&lex(s).unwrap()).unwrap()
    }

    fn cmd_heads(nodes: &[Node]) -> Vec<String> {
        let mut out = Vec::new();
        for n in nodes {
            match n {
                Node::Cmd(c) => {
                    if let Some(w) = c.words.first() {
                        out.push(w.display());
                    }
                }
                Node::Subshell(inner) => out.extend(cmd_heads(inner)),
                Node::Opaque { .. } => out.push("<opaque>".into()),
            }
        }
        out
    }

    #[test]
    fn connectors_split_commands() {
        let n = parse_str("a && b || c; d | e");
        assert_eq!(cmd_heads(&n), vec!["a", "b", "c", "d", "e"]);
    }

    #[test]
    fn assignments_prefix() {
        let n = parse_str("FOO=1 BAR=$X cmd arg");
        let Node::Cmd(c) = &n[0] else { panic!() };
        assert_eq!(c.assignments.len(), 2);
        assert_eq!(c.assignments[0].0, "FOO");
        assert!(c.assignments[1].1.is_dynamic());
        assert_eq!(c.words[0].display(), "cmd");
    }

    #[test]
    fn subshell_nested() {
        let n = parse_str("(cd /tmp; ls) && pwd");
        assert!(matches!(&n[0], Node::Subshell(inner) if inner.len() == 2));
        assert_eq!(cmd_heads(&n), vec!["cd", "ls", "pwd"]);
    }

    #[test]
    fn function_definition_is_opaque_but_body_surfaces() {
        let n = parse_str("f() { rm x; }; f");
        assert!(matches!(&n[0], Node::Opaque { .. }));
        assert!(cmd_heads(&n).contains(&"rm".to_string()));
    }

    #[test]
    fn for_header_words_are_not_commands() {
        let n = parse_str("for f in a.txt b.txt; do rm $f; done");
        let heads = cmd_heads(&n);
        assert_eq!(heads, vec!["rm"]);
    }

    #[test]
    fn if_blocks_surface_inner_commands() {
        let n = parse_str("if test -f x; then rm x; else touch x; fi");
        assert_eq!(cmd_heads(&n), vec!["test", "rm", "touch"]);
    }

    #[test]
    fn case_is_opaque() {
        let n = parse_str("case $x in a) rm foo;; esac; zlogic done");
        assert!(matches!(&n[0], Node::Opaque { .. }));
        // body is skipped wholesale; the command after esac still parses
        assert_eq!(cmd_heads(&n), vec!["<opaque>", "zlogic"]);
    }

    #[test]
    fn redirect_target_not_argv() {
        let n = parse_str("zlogic hi > out.log");
        let Node::Cmd(c) = &n[0] else { panic!() };
        assert_eq!(c.words.len(), 2);
        assert_eq!(c.redirects.len(), 1);
        assert_eq!(c.redirects[0].target.as_ref().unwrap().display(), "out.log");
    }
}
