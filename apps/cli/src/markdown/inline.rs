//! Inline markdown → styled `Span`s (bold / italic / code / strike / links).
//! Design notes:
//! - Single left-to-right scan over a `Vec<char>` (char indexing keeps CJK and
//!   other multibyte text safe); literal text accumulates in a buffer that is
//!   flushed before each styled span.
//! - Emphasis is delimiter-run based: `*`/`_` runs of 1 → ITALIC, 2 → BOLD,
//!   3 → bold+italic **deliberately degraded to BOLD** (terminals rarely render
//!   the combination distinctly, and it keeps the parser one-pass). Runs of 4+
//!   render literally.
//! - Unclosed markers never eat text — if no valid closer is found the run is
//!   emitted literally. `\*` (any escaped ASCII punctuation) emits the char.
//! - Underscore emphasis is word-boundary gated so `snake_case_names` stays
//!   literal: an opening `_` must not follow an alphanumeric char, a closing
//!   `_` must not precede one.
//! - Inner content of emphasis/links is parsed recursively with the outer
//!   style as the base, so `*a **b** c*` nests. Code spans are opaque: their
//!   content is never re-parsed. Backtick runs match by exact length, so
//!   ``` ``a`b`` ``` works.
//! - Links keep it simple (no OSC 8): `[text](url)` → text UNDERLINED +
//!   `Sem::Info`, followed by ` (url)` in `Sem::Muted`.

use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

use crate::theme::{Sem, ThemeState};

/// Parse a text run into styled spans with a default base style.
pub fn parse_inline(text: &str, theme: &ThemeState) -> Vec<Span<'static>> {
    parse_inline_styled(text, Style::default(), theme)
}

/// Parse a text run; every produced span starts from `base` (e.g. the muted
/// blockquote style or a heading's bold+accent style) and patches on top.
pub fn parse_inline_styled(text: &str, base: Style, theme: &ThemeState) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    parse_into(&chars, base, theme, &mut out);
    out
}

/// Render inline markdown to plain text (markers dropped, links become
/// `text (url)`). Used where the sink cannot carry styles, e.g. table cells.
pub fn strip_inline(text: &str, theme: &ThemeState) -> String {
    parse_inline(text, theme)
        .iter()
        .map(|s| s.content.as_ref())
        .collect()
}

fn parse_into(chars: &[char], base: Style, theme: &ThemeState, out: &mut Vec<Span<'static>>) {
    let mut buf = String::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' if i + 1 < chars.len() && chars[i + 1].is_ascii_punctuation() => {
                buf.push(chars[i + 1]);
                i += 2;
            }
            '`' => {
                let n = run_len(chars, i, '`');
                match find_code_close(chars, i + n, n) {
                    Some(close) => {
                        flush(&mut buf, base, out);
                        let content: String = chars[i + n..close].iter().collect();
                        out.push(Span::styled(
                            content,
                            base.patch(theme.style(Sem::AccentSoft)),
                        ));
                        i = close + n;
                    }
                    None => {
                        buf.extend(std::iter::repeat_n('`', n));
                        i += n;
                    }
                }
            }
            '*' | '_' | '~' => match try_emphasis(chars, i, base, theme, out, &mut buf) {
                Some(next) => i = next,
                None => {
                    let n = run_len(chars, i, chars[i]);
                    buf.extend(std::iter::repeat_n(chars[i], n));
                    i += n;
                }
            },
            '[' => match try_link(chars, i, base, theme, out, &mut buf) {
                Some(next) => i = next,
                None => {
                    buf.push('[');
                    i += 1;
                }
            },
            c => {
                buf.push(c);
                i += 1;
            }
        }
    }
    flush(&mut buf, base, out);
}

fn flush(buf: &mut String, base: Style, out: &mut Vec<Span<'static>>) {
    if !buf.is_empty() {
        out.push(Span::styled(std::mem::take(buf), base));
    }
}

fn run_len(chars: &[char], from: usize, ch: char) -> usize {
    chars[from..].iter().take_while(|c| **c == ch).count()
}

/// Length of delimiter consumed + the modifier it maps to. `None` = not an
/// emphasis delimiter (e.g. a single `~`, or a 4+ run of `*`).
fn emphasis_delim(ch: char, n: usize) -> Option<(usize, Modifier)> {
    match ch {
        '~' if n >= 2 => Some((2, Modifier::CROSSED_OUT)),
        '*' | '_' => match n {
            1 => Some((1, Modifier::ITALIC)),
            2 => Some((2, Modifier::BOLD)),
            // ***bold italic*** degrades to bold (documented choice).
            3 => Some((3, Modifier::BOLD)),
            _ => None,
        },
        _ => None,
    }
}

fn try_emphasis(
    chars: &[char],
    i: usize,
    base: Style,
    theme: &ThemeState,
    out: &mut Vec<Span<'static>>,
    buf: &mut String,
) -> Option<usize> {
    let ch = chars[i];
    let n = run_len(chars, i, ch);
    let (delim, modifier) = emphasis_delim(ch, n)?;
    let open_end = i + delim;
    // Opener: content must start immediately (no whitespace, not more of the
    // same delimiter char).
    if open_end >= chars.len() || chars[open_end].is_whitespace() || chars[open_end] == ch {
        return None;
    }
    // Underscore never opens inside a word (snake_case stays literal).
    if ch == '_' && i > 0 && chars[i - 1].is_alphanumeric() {
        return None;
    }
    // Scan for a valid closer.
    let mut j = open_end;
    while j < chars.len() {
        if chars[j] == '\\' {
            j += 2; // escaped char can't start a closer
            continue;
        }
        if chars[j] != ch {
            j += 1;
            continue;
        }
        let run = run_len(chars, j, ch);
        let closes = run >= delim
            // an italic `*` must not close on a `**` run — lets bold nest inside
            && (delim > 1 || run == 1)
            && j > open_end // non-empty inner
            && !chars[j - 1].is_whitespace()
            // underscore never closes into a word
            && (ch != '_' || j + delim >= chars.len() || !chars[j + delim].is_alphanumeric());
        if closes {
            flush(buf, base, out);
            parse_into(&chars[open_end..j], base.add_modifier(modifier), theme, out);
            return Some(j + delim);
        }
        j += run;
    }
    None
}

/// Backtick-run closer of exactly `n` backticks.
fn find_code_close(chars: &[char], from: usize, n: usize) -> Option<usize> {
    let mut j = from;
    while j < chars.len() {
        if chars[j] == '`' {
            let run = run_len(chars, j, '`');
            if run == n {
                return Some(j);
            }
            j += run;
        } else {
            j += 1;
        }
    }
    None
}

fn try_link(
    chars: &[char],
    i: usize,
    base: Style,
    theme: &ThemeState,
    out: &mut Vec<Span<'static>>,
    buf: &mut String,
) -> Option<usize> {
    // chars[i] == '['
    let mut j = i + 1;
    let text_end = loop {
        if j >= chars.len() {
            return None;
        }
        match chars[j] {
            '\\' => j += 2,
            ']' => break j,
            _ => j += 1,
        }
    };
    if text_end + 1 >= chars.len() || chars[text_end + 1] != '(' {
        return None;
    }
    let url_start = text_end + 2;
    let url_end = (url_start..chars.len()).find(|&k| chars[k] == ')')?;
    flush(buf, base, out);
    let link_style = base
        .patch(theme.style(Sem::Info))
        .add_modifier(Modifier::UNDERLINED);
    parse_into(&chars[i + 1..text_end], link_style, theme, out);
    let url: String = chars[url_start..url_end].iter().collect();
    out.push(Span::styled(
        format!(" ({url})"),
        base.patch(theme.style(Sem::Muted)),
    ));
    Some(url_end + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{by_id, ColorTier};

    fn ts() -> ThemeState {
        ThemeState {
            theme: by_id("dark").expect("dark theme exists"),
            tier: ColorTier::Rich,
        }
    }

    fn text_of(spans: &[Span<'_>]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn span_with<'a>(spans: &'a [Span<'a>], content: &str) -> &'a Span<'a> {
        spans
            .iter()
            .find(|s| s.content.as_ref() == content)
            .unwrap_or_else(|| panic!("no span with content {content:?} in {spans:?}"))
    }

    #[test]
    fn bold_double_asterisk() {
        let t = ts();
        let spans = parse_inline("a **b** c", &t);
        assert_eq!(text_of(&spans), "a b c");
        assert!(span_with(&spans, "b")
            .style
            .add_modifier
            .contains(Modifier::BOLD));
        assert!(!span_with(&spans, "a ")
            .style
            .add_modifier
            .contains(Modifier::BOLD));
    }

    #[test]
    fn bold_double_underscore() {
        let t = ts();
        let spans = parse_inline("__b__", &t);
        assert_eq!(text_of(&spans), "b");
        assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn italic_single_asterisk() {
        let t = ts();
        let spans = parse_inline("*i*", &t);
        assert_eq!(text_of(&spans), "i");
        assert!(spans[0].style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn italic_underscore_at_word_boundary() {
        let t = ts();
        let spans = parse_inline("_i_", &t);
        assert_eq!(text_of(&spans), "i");
        assert!(spans[0].style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn underscore_inside_word_is_literal() {
        let t = ts();
        let spans = parse_inline("snake_case_names", &t);
        assert_eq!(text_of(&spans), "snake_case_names");
        for s in &spans {
            assert!(!s.style.add_modifier.contains(Modifier::ITALIC), "{s:?}");
        }
    }

    #[test]
    fn code_span_uses_accent_soft_no_modifier() {
        let t = ts();
        let spans = parse_inline("run `cargo test` now", &t);
        let code = span_with(&spans, "cargo test");
        assert_eq!(code.style.fg, Some(t.color(Sem::AccentSoft)));
        assert!(!code.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn double_backtick_code_keeps_inner_backtick() {
        let t = ts();
        let spans = parse_inline("``a`b``", &t);
        assert_eq!(text_of(&spans), "a`b");
        assert_eq!(spans[0].style.fg, Some(t.color(Sem::AccentSoft)));
    }

    #[test]
    fn code_span_content_is_opaque() {
        let t = ts();
        let spans = parse_inline("`**x**`", &t);
        assert_eq!(text_of(&spans), "**x**");
        assert!(!spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn strikethrough() {
        let t = ts();
        let spans = parse_inline("~~gone~~", &t);
        assert_eq!(text_of(&spans), "gone");
        assert!(spans[0].style.add_modifier.contains(Modifier::CROSSED_OUT));
    }

    #[test]
    fn single_tilde_is_literal() {
        let t = ts();
        assert_eq!(text_of(&parse_inline("~/.config/app", &t)), "~/.config/app");
    }

    #[test]
    fn link_text_and_url() {
        let t = ts();
        let spans = parse_inline("[docs](https://e.com)", &t);
        assert_eq!(text_of(&spans), "docs (https://e.com)");
        let text = span_with(&spans, "docs");
        assert!(text.style.add_modifier.contains(Modifier::UNDERLINED));
        assert_eq!(text.style.fg, Some(t.color(Sem::Info)));
        let url = span_with(&spans, " (https://e.com)");
        assert_eq!(url.style.fg, Some(t.color(Sem::Muted)));
    }

    #[test]
    fn incomplete_link_is_literal() {
        let t = ts();
        assert_eq!(text_of(&parse_inline("[docs](oops", &t)), "[docs](oops");
        assert_eq!(text_of(&parse_inline("[docs] no url", &t)), "[docs] no url");
    }

    #[test]
    fn unclosed_markers_render_literally() {
        let t = ts();
        assert_eq!(text_of(&parse_inline("**a", &t)), "**a");
        assert_eq!(text_of(&parse_inline("*a", &t)), "*a");
        assert_eq!(text_of(&parse_inline("~~a", &t)), "~~a");
        assert_eq!(text_of(&parse_inline("`a", &t)), "`a");
    }

    #[test]
    fn marker_at_line_end_is_literal() {
        let t = ts();
        let spans = parse_inline("dangling **", &t);
        assert_eq!(text_of(&spans), "dangling **");
        for s in &spans {
            assert!(!s.style.add_modifier.contains(Modifier::BOLD));
        }
    }

    #[test]
    fn escaped_asterisk_survives_literally() {
        let t = ts();
        let spans = parse_inline(r"\*not italic\*", &t);
        assert_eq!(text_of(&spans), "*not italic*");
        for s in &spans {
            assert!(!s.style.add_modifier.contains(Modifier::ITALIC));
        }
    }

    #[test]
    fn triple_asterisk_degrades_to_bold() {
        let t = ts();
        let spans = parse_inline("***x***", &t);
        assert_eq!(text_of(&spans), "x");
        assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn bold_nests_inside_italic() {
        let t = ts();
        let spans = parse_inline("*a **b** c*", &t);
        assert_eq!(text_of(&spans), "a b c");
        for s in &spans {
            assert!(s.style.add_modifier.contains(Modifier::ITALIC), "{s:?}");
        }
        assert!(span_with(&spans, "b")
            .style
            .add_modifier
            .contains(Modifier::BOLD));
        assert!(!span_with(&spans, "a ")
            .style
            .add_modifier
            .contains(Modifier::BOLD));
    }

    #[test]
    fn spaced_asterisks_are_not_emphasis() {
        let t = ts();
        assert_eq!(text_of(&parse_inline("2 * 3 * 4", &t)), "2 * 3 * 4");
    }

    #[test]
    fn cjk_with_inline_styles() {
        let t = ts();
        let spans = parse_inline("**加粗**，然后是`代码`片段", &t);
        assert_eq!(text_of(&spans), "加粗，然后是代码片段");
        assert!(span_with(&spans, "加粗")
            .style
            .add_modifier
            .contains(Modifier::BOLD));
        assert_eq!(
            span_with(&spans, "代码").style.fg,
            Some(t.color(Sem::AccentSoft))
        );
    }

    #[test]
    fn base_style_is_preserved_under_emphasis() {
        let t = ts();
        let base = t.style(Sem::Muted);
        let spans = parse_inline_styled("**b**", base, &t);
        assert_eq!(spans[0].style.fg, base.fg);
        assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn strip_inline_drops_markers() {
        let t = ts();
        assert_eq!(
            strip_inline("**b** `c` [t](u)", &t),
            "b c t (u)".to_string()
        );
    }
}
