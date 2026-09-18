//! Wrap-before-insert (caller owns wrapping). History is wrapped to physical lines
//! HERE, before `insert_before`, so the terminal's own reflow never fights ours. Once
//! written, scrollback is frozen — resize only redraws the viewport, never history.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Clone)]
pub struct HistSpan {
    pub text: String,
    pub style: Style,
}

impl HistSpan {
    pub fn raw(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: Style::default(),
        }
    }

    pub fn styled(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }
}

#[derive(Clone)]
pub struct HistLine {
    pub spans: Vec<HistSpan>,
    pub align: HistAlign,
}

#[derive(Clone, Copy)]
pub enum HistAlign {
    Left,
    CenterIn(usize),
}

impl HistLine {
    pub fn blank() -> Self {
        Self {
            spans: Vec::new(),
            align: HistAlign::Left,
        }
    }

    pub fn raw(text: impl Into<String>) -> Self {
        Self {
            spans: vec![HistSpan::raw(text)],
            align: HistAlign::Left,
        }
    }

    pub fn styled(text: impl Into<String>, style: Style) -> Self {
        Self {
            spans: vec![HistSpan::styled(text, style)],
            align: HistAlign::Left,
        }
    }

    pub fn spans(spans: Vec<HistSpan>) -> Self {
        Self {
            spans,
            align: HistAlign::Left,
        }
    }

    pub fn centered_in(spans: Vec<HistSpan>, width: usize) -> Self {
        Self {
            spans,
            align: HistAlign::CenterIn(width),
        }
    }

    pub fn from_line(line: Line<'static>) -> Self {
        Self {
            spans: line
                .spans
                .into_iter()
                .map(|span| HistSpan::styled(span.content.into_owned(), span.style))
                .collect(),
            align: HistAlign::Left,
        }
    }
}

/// Wrap `s` to at most `max` display columns per line (CJK = 2 via `unicode-width`).
/// Honors existing `\n`. Never returns empty (one empty line for empty input).
pub fn wrap_line(s: &str, max: usize) -> Vec<String> {
    if max == 0 {
        return vec![s.to_string()];
    }
    let mut out = Vec::new();
    for logical in s.split('\n') {
        let mut cur = String::new();
        let mut w = 0usize;
        for ch in logical.chars() {
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
            if w + cw > max && !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
                w = 0;
            }
            cur.push(ch);
            w += cw;
        }
        out.push(cur);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

pub fn wrap_hist_line(
    line: &HistLine,
    wrap_width: usize,
    align_width: usize,
) -> Vec<Line<'static>> {
    if wrap_width == 0 {
        return vec![to_ratatui_line(line.spans.clone())];
    }

    let spans = aligned_spans(line, align_width);
    let continuation_indent = match line.align {
        HistAlign::Left => leading_space_width(&spans),
        HistAlign::CenterIn(_) => 0,
    }
    .min(wrap_width.saturating_sub(1));
    let mut out = Vec::new();
    let mut cur_spans: Vec<HistSpan> = Vec::new();
    let mut cur_text = String::new();
    let mut cur_style = Style::default();
    let mut cur_w = 0usize;

    let flush_text = |cur_text: &mut String, cur_style: Style, cur_spans: &mut Vec<HistSpan>| {
        if !cur_text.is_empty() {
            cur_spans.push(HistSpan::styled(std::mem::take(cur_text), cur_style));
        }
    };

    let flush_line = |out: &mut Vec<Line<'static>>, cur_spans: &mut Vec<HistSpan>| {
        out.push(to_ratatui_line(std::mem::take(cur_spans)));
    };

    let start_continuation = |cur_spans: &mut Vec<HistSpan>, cur_w: &mut usize| {
        if continuation_indent > 0 {
            cur_spans.push(HistSpan::raw(" ".repeat(continuation_indent)));
        }
        *cur_w = continuation_indent;
    };

    for span in &spans {
        if cur_text.is_empty() {
            cur_style = span.style;
        }
        for ch in span.text.chars() {
            if ch == '\n' {
                flush_text(&mut cur_text, cur_style, &mut cur_spans);
                flush_line(&mut out, &mut cur_spans);
                start_continuation(&mut cur_spans, &mut cur_w);
                cur_style = span.style;
                continue;
            }

            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
            if cur_w + cw > wrap_width && (!cur_text.is_empty() || !cur_spans.is_empty()) {
                flush_text(&mut cur_text, cur_style, &mut cur_spans);
                flush_line(&mut out, &mut cur_spans);
                start_continuation(&mut cur_spans, &mut cur_w);
                cur_style = span.style;
            } else if cur_text.is_empty() {
                cur_style = span.style;
            }
            cur_text.push(ch);
            cur_w += cw;
        }
        flush_text(&mut cur_text, cur_style, &mut cur_spans);
    }

    flush_text(&mut cur_text, cur_style, &mut cur_spans);
    if out.is_empty() || !cur_spans.is_empty() {
        flush_line(&mut out, &mut cur_spans);
    }
    if out.is_empty() {
        out.push(Line::from(String::new()));
    }
    out
}

fn leading_space_width(spans: &[HistSpan]) -> usize {
    let mut width = 0;
    for span in spans {
        for ch in span.text.chars() {
            if ch == ' ' {
                width += 1;
            } else {
                return width;
            }
        }
    }
    width
}

fn aligned_spans(line: &HistLine, width: usize) -> Vec<HistSpan> {
    let HistAlign::CenterIn(block_width) = line.align else {
        return line.spans.clone();
    };
    if block_width >= width || line.spans.is_empty() {
        return line.spans.clone();
    }
    let pad = (width - block_width) / 2;
    let mut spans = Vec::with_capacity(line.spans.len() + 1);
    spans.push(HistSpan::raw(" ".repeat(pad)));
    spans.extend(line.spans.clone());
    spans
}

pub fn spans_width(spans: &[HistSpan]) -> usize {
    spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.text.as_str()))
        .sum()
}

fn to_ratatui_line(spans: Vec<HistSpan>) -> Line<'static> {
    if spans.is_empty() {
        return Line::from(String::new());
    }
    Line::from(
        spans
            .into_iter()
            .map(|s| Span::styled(s.text, s.style))
            .collect::<Vec<_>>(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Modifier};

    fn line_text(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn line_width(line: &Line<'static>) -> usize {
        UnicodeWidthStr::width(line_text(line).as_str())
    }

    // ---- wrap_line ------------------------------------------------------

    #[test]
    fn wrap_line_plain_ascii_at_cap() {
        let s = "a".repeat(250);
        let out = wrap_line(&s, 120);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].len(), 120);
        assert_eq!(out[1].len(), 120);
        assert_eq!(out[2].len(), 10);
        assert_eq!(out.concat(), s, "no characters lost");
        for l in &out {
            assert!(UnicodeWidthStr::width(l.as_str()) <= 120);
        }
    }

    #[test]
    fn wrap_line_honors_existing_newlines() {
        let out = wrap_line("ab\ncd", 10);
        assert_eq!(out, vec!["ab".to_string(), "cd".to_string()]);
        // Trailing newline yields a trailing empty physical line.
        assert_eq!(wrap_line("ab\n", 10), vec!["ab".to_string(), String::new()]);
    }

    #[test]
    fn wrap_line_empty_input_yields_one_empty_line() {
        assert_eq!(wrap_line("", 40), vec![String::new()]);
    }

    #[test]
    fn wrap_line_zero_width_returns_input_unwrapped() {
        assert_eq!(
            wrap_line("hello\nworld", 0),
            vec!["hello\nworld".to_string()]
        );
    }

    #[test]
    fn wrap_line_cjk_never_splits_mid_glyph_and_fits() {
        // 12 double-width chars at max=10 → 5 chars (10 cols) per line.
        let s = "汉".repeat(12);
        let out = wrap_line(&s, 10);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].chars().count(), 5);
        assert_eq!(out[1].chars().count(), 5);
        assert_eq!(out[2].chars().count(), 2);
        assert_eq!(out.concat(), s);
        for l in &out {
            assert!(
                UnicodeWidthStr::width(l.as_str()) <= 10,
                "line overflows: {l:?}"
            );
        }
    }

    #[test]
    fn wrap_line_cjk_odd_width_leaves_slack_not_split() {
        // max=3 fits exactly one double-width char plus 1 col slack — a second
        // char would need 4 cols, so each line carries one glyph, never half.
        let out = wrap_line("你好世", 3);
        assert_eq!(out, vec!["你", "好", "世"]);
        for l in &out {
            assert!(UnicodeWidthStr::width(l.as_str()) <= 3);
        }
    }

    #[test]
    fn wrap_line_mixed_cjk_ascii_fits() {
        let s = "ab你cd好ef世gh界ij"; // widths alternate 1 and 2
        let out = wrap_line(s, 5);
        assert_eq!(out.concat(), s);
        for l in &out {
            assert!(
                UnicodeWidthStr::width(l.as_str()) <= 5,
                "line overflows: {l:?}"
            );
        }
    }

    #[test]
    fn wrap_line_pathological_token_hard_breaks() {
        let s = "x".repeat(1000);
        let out = wrap_line(&s, 7);
        assert_eq!(out.len(), 1000usize.div_ceil(7));
        assert_eq!(out.concat(), s);
        for l in &out {
            assert!(l.len() <= 7);
        }
    }

    #[test]
    fn wrap_line_one_col_cjk_overflows_by_glyph_not_forever() {
        // A double-width glyph cannot fit in 1 column; the documented floor is
        // one glyph per line (line width 2 > 1), never a panic or infinite loop.
        let out = wrap_line("你好", 1);
        assert_eq!(out, vec!["你", "好"]);
    }

    // ---- wrap_hist_line --------------------------------------------------

    #[test]
    fn wrap_hist_line_fits_width_and_preserves_text() {
        let line = HistLine::raw("a".repeat(30));
        let out = wrap_hist_line(&line, 12, 12);
        assert_eq!(out.len(), 3);
        let all: String = out.iter().map(line_text).collect();
        assert_eq!(all, "a".repeat(30));
        for l in &out {
            assert!(line_width(l) <= 12);
        }
    }

    #[test]
    fn wrap_hist_line_zero_width_is_single_unwrapped_line() {
        let line = HistLine::raw("hello world, quite a long line indeed");
        let out = wrap_hist_line(&line, 0, 80);
        assert_eq!(out.len(), 1);
        assert_eq!(line_text(&out[0]), "hello world, quite a long line indeed");
    }

    #[test]
    fn wrap_hist_line_blank_yields_one_empty_line() {
        let out = wrap_hist_line(&HistLine::blank(), 40, 40);
        assert_eq!(out.len(), 1);
        assert_eq!(line_text(&out[0]), "");
    }

    #[test]
    fn wrap_hist_line_preserves_span_styles_across_wraps() {
        let red = Style::default().fg(Color::Red);
        let blue = Style::default()
            .fg(Color::Blue)
            .add_modifier(Modifier::BOLD);
        let line = HistLine::spans(vec![
            HistSpan::styled("aaaa", red),
            HistSpan::styled("bbbb", blue),
        ]);
        let out = wrap_hist_line(&line, 3, 3);
        // "aaaa"+"bbbb" at 3 cols → "aaa" / "a"+"bb" / "bb"
        assert_eq!(out.len(), 3);
        assert_eq!(line_text(&out[0]), "aaa");
        assert_eq!(line_text(&out[1]), "abb");
        assert_eq!(line_text(&out[2]), "bb");
        // Every 'a' span stays red; every 'b' span stays blue, on both sides
        // of each wrap point.
        for l in &out {
            for span in &l.spans {
                let expect = if span.content.contains('a') {
                    red
                } else {
                    blue
                };
                assert_eq!(span.style, expect, "span {:?} lost its style", span.content);
            }
        }
        // The middle line carries both styles as separate spans.
        assert_eq!(out[1].spans.len(), 2);
    }

    #[test]
    fn wrap_hist_line_cjk_spans_fit() {
        let line = HistLine::spans(vec![HistSpan::raw("汉字宽度测试"), HistSpan::raw("abc")]);
        let out = wrap_hist_line(&line, 5, 5);
        let all: String = out.iter().map(line_text).collect();
        assert_eq!(all, "汉字宽度测试abc");
        for l in &out {
            assert!(line_width(l) <= 5, "line overflows: {:?}", line_text(l));
        }
    }

    #[test]
    fn wrap_hist_line_embedded_newline_breaks_line() {
        let line = HistLine::raw("ab\ncd");
        let out = wrap_hist_line(&line, 40, 40);
        assert_eq!(out.len(), 2);
        assert_eq!(line_text(&out[0]), "ab");
        assert_eq!(line_text(&out[1]), "cd");
    }

    #[test]
    fn wrap_hist_line_one_col_cjk_no_panic_no_loop() {
        let line = HistLine::raw("你好世界");
        let out = wrap_hist_line(&line, 1, 1);
        assert_eq!(out.len(), 4, "one double-width glyph per line");
        let all: String = out.iter().map(line_text).collect();
        assert_eq!(all, "你好世界");
    }

    #[test]
    fn wrapped_indented_lines_keep_the_left_padding() {
        let line = HistLine::raw("  123456789");
        let out = wrap_hist_line(&line, 6, 6);
        assert_eq!(line_text(&out[0]), "  1234");
        assert_eq!(line_text(&out[1]), "  5678");
        assert_eq!(line_text(&out[2]), "  9");
    }

    // ---- HistAlign::CenterIn ---------------------------------------------

    #[test]
    fn center_in_pads_content_narrower_than_box() {
        // Block of declared width 10 centered in an available width of 20:
        // pad = (20 - 10) / 2 = 5 leading spaces.
        let line = HistLine::centered_in(vec![HistSpan::raw("hi")], 10);
        let out = wrap_hist_line(&line, 40, 20);
        assert_eq!(out.len(), 1);
        assert_eq!(line_text(&out[0]), format!("{}hi", " ".repeat(5)));
    }

    #[test]
    fn center_in_odd_remainder_floors_pad() {
        // (21 - 10) / 2 = 5 (integer division).
        let line = HistLine::centered_in(vec![HistSpan::raw("hi")], 10);
        let out = wrap_hist_line(&line, 40, 21);
        assert_eq!(line_text(&out[0]), format!("{}hi", " ".repeat(5)));
    }

    #[test]
    fn center_in_box_wider_than_terminal_gets_no_pad() {
        // Declared block width >= available width → no centering pad.
        let line = HistLine::centered_in(vec![HistSpan::raw("wide")], 100);
        let out = wrap_hist_line(&line, 40, 80);
        assert_eq!(line_text(&out[0]), "wide");
    }

    #[test]
    fn center_in_empty_spans_get_no_pad() {
        let line = HistLine::centered_in(Vec::new(), 10);
        let out = wrap_hist_line(&line, 40, 20);
        assert_eq!(out.len(), 1);
        assert_eq!(line_text(&out[0]), "");
    }

    // ---- spans_width -------------------------------------------------------

    #[test]
    fn spans_width_sums_display_columns() {
        let spans = vec![
            HistSpan::raw("ab"),
            HistSpan::raw("汉字"),
            HistSpan::raw(""),
        ];
        assert_eq!(spans_width(&spans), 2 + 4);
        assert_eq!(spans_width(&[]), 0);
    }
}
