//! POSIX-ish shell lexer.
//! Deliberately NOT a full bash grammar. The contract is fail-closed: every
//! construct is either modeled precisely, or surfaces as a dynamic word /
//! `Err` so the caller degrades to "ask the user" — never silently passes.

/// One segment of a word. A word whose parts are all `Lit` is statically
/// knowable; anything else means the runtime value cannot be predicted.
#[derive(Debug, Clone, PartialEq)]
pub enum WordPart {
    Lit(String),
    /// `$NAME` / `${...}` / `$((...))` — value unknowable statically.
    /// Note: a `${x:-$(cmd)}` default runs a command we do not extract; the
    /// word still counts as dynamic, so the decision layer must ask.
    Var(String),
    /// `$( ... )` or `` ` ... ` `` — inner source text, analyzed recursively.
    CmdSub(String),
    /// `<(...)` / `>(...)` — inner source text, analyzed recursively.
    ProcSub {
        write: bool,
        body: String,
    },
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Word {
    pub parts: Vec<WordPart>,
    /// An unquoted glob char (`*` `?` `[`) appeared — expansion unknowable.
    pub glob: bool,
}

impl Word {
    /// The word's exact value, if it is fully literal.
    pub fn as_static(&self) -> Option<String> {
        let mut s = String::new();
        for p in &self.parts {
            match p {
                WordPart::Lit(l) => s.push_str(l),
                _ => return None,
            }
        }
        Some(s)
    }

    pub fn is_dynamic(&self) -> bool {
        self.as_static().is_none()
    }

    /// Human-readable reconstruction (for display / audit, not execution).
    pub fn display(&self) -> String {
        let mut s = String::new();
        for p in &self.parts {
            match p {
                WordPart::Lit(l) => s.push_str(l),
                WordPart::Var(v) => {
                    s.push('$');
                    s.push_str(v);
                }
                WordPart::CmdSub(b) => {
                    s.push_str("$(");
                    s.push_str(b);
                    s.push(')');
                }
                WordPart::ProcSub { write, body } => {
                    s.push(if *write { '>' } else { '<' });
                    s.push('(');
                    s.push_str(body);
                    s.push(')');
                }
            }
        }
        s
    }

    fn push_lit(&mut self, c: char) {
        if let Some(WordPart::Lit(l)) = self.parts.last_mut() {
            l.push(c);
        } else {
            self.parts.push(WordPart::Lit(c.to_string()));
        }
    }

    fn push_lit_str(&mut self, s: &str) {
        for c in s.chars() {
            self.push_lit(c);
        }
    }

    /// `''` / `""` are real (empty) arguments — keep a part so the word
    /// is not dropped as "nothing lexed".
    fn ensure_part(&mut self) {
        if self.parts.is_empty() {
            self.parts.push(WordPart::Lit(String::new()));
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirKind {
    In,
    Out,
    Append,
    Heredoc,
    HereString,
    DupIn,
    DupOut,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Word(Word),
    And,
    Or,
    Pipe,
    Semi,
    Amp,
    LParen,
    RParen,
    /// The target is the next `Word` token in the stream.
    Redir {
        kind: RedirKind,
        fd: Option<u32>,
    },
}

pub fn lex(input: &str) -> Result<Vec<Token>, String> {
    let mut lx = Lexer {
        chars: input.chars().collect(),
        i: 0,
        tokens: Vec::new(),
        pending_heredocs: Vec::new(),
    };
    lx.run()?;
    Ok(lx.tokens)
}

struct Lexer {
    chars: Vec<char>,
    i: usize,
    tokens: Vec<Token>,
    /// Heredoc delimiters seen on the current line; their bodies are skipped
    /// (they are stdin data, not commands) when the newline arrives.
    pending_heredocs: Vec<String>,
}

impl Lexer {
    fn peek(&self, n: usize) -> Option<char> {
        self.chars.get(self.i + n).copied()
    }

    fn skip_blank(&mut self) {
        while matches!(self.peek(0), Some(' ') | Some('\t')) {
            self.i += 1;
        }
    }

    fn run(&mut self) -> Result<(), String> {
        while let Some(c) = self.peek(0) {
            match c {
                ' ' | '\t' | '\r' => self.i += 1,
                '\n' => {
                    self.i += 1;
                    self.consume_heredoc_bodies();
                    self.tokens.push(Token::Semi);
                }
                '#' => {
                    while let Some(c) = self.peek(0) {
                        if c == '\n' {
                            break;
                        }
                        self.i += 1;
                    }
                }
                ';' => {
                    self.i += 1;
                    if self.peek(0) == Some(';') {
                        self.i += 1;
                    }
                    self.tokens.push(Token::Semi);
                }
                '&' => {
                    if self.peek(1) == Some('&') {
                        self.i += 2;
                        self.tokens.push(Token::And);
                    } else if self.peek(1) == Some('>') {
                        // &> / &>> redirect both streams
                        self.i += 2;
                        let kind = if self.peek(0) == Some('>') {
                            self.i += 1;
                            RedirKind::Append
                        } else {
                            RedirKind::Out
                        };
                        self.tokens.push(Token::Redir { kind, fd: None });
                    } else {
                        self.i += 1;
                        self.tokens.push(Token::Amp);
                    }
                }
                '|' => {
                    if self.peek(1) == Some('|') {
                        self.i += 2;
                        self.tokens.push(Token::Or);
                    } else {
                        self.i += 1;
                        self.tokens.push(Token::Pipe);
                    }
                }
                '(' => {
                    self.i += 1;
                    self.tokens.push(Token::LParen);
                }
                ')' => {
                    self.i += 1;
                    self.tokens.push(Token::RParen);
                }
                '<' | '>' => self.lex_redirect(None)?,
                c if c.is_ascii_digit() && self.fd_redirect_ahead() => {
                    let mut n = 0u32;
                    while let Some(d) = self.peek(0).and_then(|c| c.to_digit(10)) {
                        n = n.saturating_mul(10).saturating_add(d);
                        self.i += 1;
                    }
                    self.lex_redirect(Some(n))?;
                }
                _ => {
                    let w = self.lex_word()?;
                    if !w.parts.is_empty() {
                        self.tokens.push(Token::Word(w));
                    }
                }
            }
        }
        Ok(())
    }

    /// True when a token-initial digit run is immediately followed by a
    /// redirect operator (`2>`), i.e. the digits are an fd, not a word.
    fn fd_redirect_ahead(&self) -> bool {
        let mut j = self.i;
        while self.chars.get(j).is_some_and(|c| c.is_ascii_digit()) {
            j += 1;
        }
        matches!(self.chars.get(j), Some('<') | Some('>'))
    }

    fn lex_redirect(&mut self, fd: Option<u32>) -> Result<(), String> {
        let c = self.peek(0).expect("caller checked");
        if c == '<' {
            if self.peek(1) == Some('(') {
                // process substitution is a WORD, not an operator
                self.i += 2;
                let body = self.consume_balanced_parens()?;
                self.tokens.push(Token::Word(Word {
                    parts: vec![WordPart::ProcSub { write: false, body }],
                    glob: false,
                }));
                return Ok(());
            }
            if self.peek(1) == Some('<') {
                if self.peek(2) == Some('<') {
                    self.i += 3;
                    self.tokens.push(Token::Redir {
                        kind: RedirKind::HereString,
                        fd,
                    });
                    return Ok(());
                }
                self.i += 2;
                if self.peek(0) == Some('-') {
                    self.i += 1;
                }
                self.tokens.push(Token::Redir {
                    kind: RedirKind::Heredoc,
                    fd,
                });
                // the delimiter word must follow; record it so the newline
                // handler can skip the body
                self.skip_blank();
                let w = self.lex_word()?;
                if w.parts.is_empty() {
                    return Err("heredoc without delimiter".into());
                }
                let delim = w.as_static().unwrap_or_else(|| w.display());
                self.pending_heredocs.push(delim);
                self.tokens.push(Token::Word(w));
                return Ok(());
            }
            if self.peek(1) == Some('&') {
                self.i += 2;
                self.tokens.push(Token::Redir {
                    kind: RedirKind::DupIn,
                    fd,
                });
                return Ok(());
            }
            self.i += 1;
            self.tokens.push(Token::Redir {
                kind: RedirKind::In,
                fd,
            });
        } else {
            if self.peek(1) == Some('(') {
                self.i += 2;
                let body = self.consume_balanced_parens()?;
                self.tokens.push(Token::Word(Word {
                    parts: vec![WordPart::ProcSub { write: true, body }],
                    glob: false,
                }));
                return Ok(());
            }
            if self.peek(1) == Some('>') {
                self.i += 2;
                self.tokens.push(Token::Redir {
                    kind: RedirKind::Append,
                    fd,
                });
                return Ok(());
            }
            if self.peek(1) == Some('&') {
                self.i += 2;
                self.tokens.push(Token::Redir {
                    kind: RedirKind::DupOut,
                    fd,
                });
                return Ok(());
            }
            self.i += 1;
            self.tokens.push(Token::Redir {
                kind: RedirKind::Out,
                fd,
            });
        }
        Ok(())
    }

    fn consume_heredoc_bodies(&mut self) {
        let delims: Vec<String> = self.pending_heredocs.drain(..).collect();
        for delim in delims {
            loop {
                if self.i >= self.chars.len() {
                    return;
                }
                let start = self.i;
                while self.i < self.chars.len() && self.chars[self.i] != '\n' {
                    self.i += 1;
                }
                let line: String = self.chars[start..self.i].iter().collect();
                if self.i < self.chars.len() {
                    self.i += 1; // the newline
                }
                if line.trim() == delim {
                    break;
                }
            }
        }
    }

    fn lex_word(&mut self) -> Result<Word, String> {
        let mut w = Word::default();
        while let Some(c) = self.peek(0) {
            match c {
                ' ' | '\t' | '\r' | '\n' | ';' | '&' | '|' | '(' | ')' | '<' | '>' => break,
                '\'' => {
                    self.i += 1;
                    let mut s = String::new();
                    loop {
                        match self.peek(0) {
                            None => return Err("unterminated single quote".into()),
                            Some('\'') => {
                                self.i += 1;
                                break;
                            }
                            Some(ch) => {
                                s.push(ch);
                                self.i += 1;
                            }
                        }
                    }
                    w.push_lit_str(&s);
                    w.ensure_part();
                }
                '"' => {
                    self.i += 1;
                    self.lex_double_quoted(&mut w)?;
                }
                '\\' => {
                    self.i += 1;
                    match self.peek(0) {
                        None => w.push_lit('\\'),
                        Some('\n') => self.i += 1, // line continuation
                        Some(ch) => {
                            w.push_lit(ch);
                            self.i += 1;
                        }
                    }
                }
                '$' => {
                    self.i += 1;
                    self.lex_dollar(&mut w)?;
                }
                '`' => {
                    self.i += 1;
                    let body = self.consume_backtick()?;
                    w.parts.push(WordPart::CmdSub(body));
                }
                // `{` covers brace expansion (`{a,b}.txt`) — like a glob, the
                // expansion result is not this literal string
                '*' | '?' | '[' | '{' => {
                    w.glob = true;
                    w.push_lit(c);
                    self.i += 1;
                }
                _ => {
                    w.push_lit(c);
                    self.i += 1;
                }
            }
        }
        Ok(w)
    }

    fn lex_double_quoted(&mut self, w: &mut Word) -> Result<(), String> {
        loop {
            match self.peek(0) {
                None => return Err("unterminated double quote".into()),
                Some('"') => {
                    self.i += 1;
                    break;
                }
                Some('\\') => {
                    self.i += 1;
                    match self.peek(0) {
                        None => return Err("unterminated double quote".into()),
                        Some('\n') => self.i += 1,
                        Some(ch) => {
                            if !matches!(ch, '$' | '`' | '"' | '\\') {
                                w.push_lit('\\');
                            }
                            w.push_lit(ch);
                            self.i += 1;
                        }
                    }
                }
                Some('$') => {
                    self.i += 1;
                    self.lex_dollar(w)?;
                }
                Some('`') => {
                    self.i += 1;
                    let body = self.consume_backtick()?;
                    w.parts.push(WordPart::CmdSub(body));
                }
                Some(ch) => {
                    w.push_lit(ch);
                    self.i += 1;
                }
            }
        }
        w.ensure_part();
        Ok(())
    }

    fn lex_dollar(&mut self, w: &mut Word) -> Result<(), String> {
        match self.peek(0) {
            Some('(') => {
                if self.peek(1) == Some('(') {
                    // $((arithmetic)) — dynamic value
                    self.i += 2;
                    let body = self.consume_balanced_parens()?;
                    if self.peek(0) == Some(')') {
                        self.i += 1;
                    }
                    w.parts.push(WordPart::Var(format!("(({}))", body)));
                } else {
                    self.i += 1;
                    let body = self.consume_balanced_parens()?;
                    w.parts.push(WordPart::CmdSub(body));
                }
            }
            Some('{') => {
                self.i += 1;
                let mut depth = 1usize;
                let mut body = String::new();
                loop {
                    match self.peek(0) {
                        None => return Err("unterminated ${".into()),
                        Some(c) => {
                            self.i += 1;
                            match c {
                                '{' => {
                                    depth += 1;
                                    body.push(c);
                                }
                                '}' => {
                                    depth -= 1;
                                    if depth == 0 {
                                        break;
                                    }
                                    body.push(c);
                                }
                                _ => body.push(c),
                            }
                        }
                    }
                }
                w.parts.push(WordPart::Var(body));
            }
            Some(c) if c.is_ascii_alphabetic() || c == '_' => {
                let mut name = String::new();
                while let Some(c) = self.peek(0) {
                    if c.is_ascii_alphanumeric() || c == '_' {
                        name.push(c);
                        self.i += 1;
                    } else {
                        break;
                    }
                }
                w.parts.push(WordPart::Var(name));
            }
            Some(c)
                if c.is_ascii_digit() || matches!(c, '?' | '$' | '!' | '#' | '@' | '*' | '-') =>
            {
                self.i += 1;
                w.parts.push(WordPart::Var(c.to_string()));
            }
            _ => w.push_lit('$'),
        }
        Ok(())
    }

    /// Consume up to the `)` matching an already-consumed `(`, quote-aware.
    fn consume_balanced_parens(&mut self) -> Result<String, String> {
        let mut depth = 1usize;
        let mut out = String::new();
        let mut sq = false;
        let mut dq = false;
        while let Some(c) = self.peek(0) {
            self.i += 1;
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
                    out.push(c);
                } else if c == '\\' {
                    out.push(c);
                    if let Some(n) = self.peek(0) {
                        self.i += 1;
                        out.push(n);
                    }
                } else {
                    out.push(c);
                }
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
                '\\' => {
                    out.push(c);
                    if let Some(n) = self.peek(0) {
                        self.i += 1;
                        out.push(n);
                    }
                }
                '(' => {
                    depth += 1;
                    out.push(c);
                }
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(out);
                    }
                    out.push(c);
                }
                _ => out.push(c),
            }
        }
        Err("unbalanced parentheses in substitution".into())
    }

    fn consume_backtick(&mut self) -> Result<String, String> {
        let mut out = String::new();
        loop {
            match self.peek(0) {
                None => return Err("unterminated backtick".into()),
                Some('`') => {
                    self.i += 1;
                    return Ok(out);
                }
                Some('\\') => {
                    self.i += 1;
                    if let Some(n) = self.peek(0) {
                        if !matches!(n, '`' | '\\' | '$') {
                            out.push('\\');
                        }
                        out.push(n);
                        self.i += 1;
                    }
                }
                Some(c) => {
                    out.push(c);
                    self.i += 1;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(tokens: &[Token]) -> Vec<String> {
        tokens
            .iter()
            .filter_map(|t| match t {
                Token::Word(w) => Some(w.display()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn quotes_and_escapes() {
        let t = lex(r#"zlogic 'a b' "c $X" d\ e"#).unwrap();
        let w = words(&t);
        assert_eq!(w, vec!["zlogic", "a b", "c $X", "d e"]);
        // "c $X" is dynamic, 'a b' is static
        let Token::Word(quoted) = &t[1] else { panic!() };
        assert_eq!(quoted.as_static().as_deref(), Some("a b"));
        let Token::Word(dyn_w) = &t[2] else { panic!() };
        assert!(dyn_w.is_dynamic());
    }

    #[test]
    fn operators_split() {
        let t = lex("a && b | c; d & e").unwrap();
        let ops: Vec<&Token> = t.iter().filter(|t| !matches!(t, Token::Word(_))).collect();
        assert!(matches!(ops[0], Token::And));
        assert!(matches!(ops[1], Token::Pipe));
        assert!(matches!(ops[2], Token::Semi));
        assert!(matches!(ops[3], Token::Amp));
    }

    #[test]
    fn fd_redirects() {
        let t = lex("cmd 2>err.log >out.log").unwrap();
        assert!(t.contains(&Token::Redir {
            kind: RedirKind::Out,
            fd: Some(2)
        }));
        assert!(t.contains(&Token::Redir {
            kind: RedirKind::Out,
            fd: None
        }));
        // `zlogic 2 >x` — standalone digit stays a word
        let t = lex("zlogic 2 >x").unwrap();
        assert_eq!(words(&t), vec!["zlogic", "2", "x"]);
    }

    #[test]
    fn dup_is_not_a_path() {
        let t = lex("cmd 2>&1").unwrap();
        assert!(t.contains(&Token::Redir {
            kind: RedirKind::DupOut,
            fd: Some(2)
        }));
    }

    #[test]
    fn command_substitution() {
        let t = lex("zlogic $(ls -l | wc) `date`").unwrap();
        let Token::Word(w1) = &t[1] else { panic!() };
        assert_eq!(w1.parts, vec![WordPart::CmdSub("ls -l | wc".into())]);
        let Token::Word(w2) = &t[2] else { panic!() };
        assert_eq!(w2.parts, vec![WordPart::CmdSub("date".into())]);
    }

    #[test]
    fn nested_cmdsub_with_quotes() {
        let t = lex(r#"zlogic "$(zlogic ")")""#).unwrap();
        let Token::Word(w) = &t[1] else { panic!() };
        assert_eq!(w.parts, vec![WordPart::CmdSub(r#"zlogic ")""#.into())]);
    }

    #[test]
    fn heredoc_body_skipped() {
        let t = lex("cat <<EOF\nrm -rf /\nEOF\nzlogic hi").unwrap();
        let w = words(&t);
        // heredoc body never becomes tokens
        assert_eq!(w, vec!["cat", "EOF", "zlogic", "hi"]);
    }

    #[test]
    fn glob_flag() {
        let t = lex("rm *.log '*.keep'").unwrap();
        let Token::Word(g) = &t[1] else { panic!() };
        assert!(g.glob);
        let Token::Word(q) = &t[2] else { panic!() };
        assert!(!q.glob); // quoted glob chars do not expand
    }

    #[test]
    fn process_substitution() {
        let t = lex("diff <(sort a) b").unwrap();
        let Token::Word(w) = &t[1] else { panic!() };
        assert_eq!(
            w.parts,
            vec![WordPart::ProcSub {
                write: false,
                body: "sort a".into()
            }]
        );
    }

    #[test]
    fn unterminated_quote_fails_closed() {
        assert!(lex("zlogic 'oops").is_err());
        assert!(lex("zlogic $(true").is_err());
    }
}
