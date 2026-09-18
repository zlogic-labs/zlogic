use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::rule;
use crate::glyph::IconTier;

#[derive(Clone, Copy)]
pub enum Align {
    Left,
    Right,
}

pub struct Column {
    pub header: String,
    pub min: u16,
    pub max: u16,
    pub priority: u8,
    pub align: Align,
}

pub fn render_table(
    columns: &[Column],
    rows: &[Vec<String>],
    width: u16,
    header_style: Style,
    body_style: Style,
    rule_style: Style,
    icons: IconTier,
) -> Vec<Line<'static>> {
    if columns.is_empty() {
        return Vec::new();
    }
    let widths = column_widths(columns, rows, width);
    let mut out = Vec::with_capacity(rows.len() + 2);
    out.push(render_row(
        columns,
        &columns.iter().map(|c| c.header.clone()).collect::<Vec<_>>(),
        &widths,
        header_style.add_modifier(Modifier::BOLD),
    ));
    out.push(rule::rule(width, rule_style, icons));
    for row in rows {
        out.push(render_row(columns, row, &widths, body_style));
    }
    out
}

fn column_widths(columns: &[Column], rows: &[Vec<String>], width: u16) -> Vec<u16> {
    let gap_total = columns.len().saturating_sub(1) as u16 * 2;
    let mut widths: Vec<u16> = columns
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let data_w = rows
                .iter()
                .filter_map(|r| r.get(i))
                .map(|s| UnicodeWidthStr::width(s.as_str()) as u16)
                .max()
                .unwrap_or(0);
            let header_w = UnicodeWidthStr::width(c.header.as_str()) as u16;
            data_w.max(header_w).clamp(c.min, c.max)
        })
        .collect();

    while widths.iter().sum::<u16>() + gap_total > width {
        let Some((idx, _)) = columns
            .iter()
            .enumerate()
            .filter(|(i, c)| widths[*i] > c.min)
            .min_by_key(|(_, c)| c.priority)
        else {
            break;
        };
        widths[idx] -= 1;
    }
    widths
}

fn render_row(columns: &[Column], cells: &[String], widths: &[u16], style: Style) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, col) in columns.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        let text = cells.get(i).map(String::as_str).unwrap_or("");
        spans.push(Span::styled(fit(text, widths[i], col.align), style));
    }
    Line::from(spans)
}

fn fit(s: &str, width: u16, align: Align) -> String {
    let width = usize::from(width);
    let clipped = truncate(s, width);
    let pad = width.saturating_sub(UnicodeWidthStr::width(clipped.as_str()));
    match align {
        Align::Left => format!("{clipped}{}", " ".repeat(pad)),
        Align::Right => format!("{}{clipped}", " ".repeat(pad)),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if UnicodeWidthStr::width(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut w = 0usize;
    let ell = "…";
    let ell_w = UnicodeWidthStr::width(ell);
    for ch in s.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw + ell_w > max {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push_str(ell);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(header: &str, min: u16, max: u16, priority: u8, align: Align) -> Column {
        Column {
            header: header.to_string(),
            min,
            max,
            priority,
            align,
        }
    }

    fn line_text(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn rows(data: &[&[&str]]) -> Vec<Vec<String>> {
        data.iter()
            .map(|r| r.iter().map(|s| s.to_string()).collect())
            .collect()
    }

    #[test]
    fn empty_columns_render_nothing() {
        let out = render_table(
            &[],
            &rows(&[&["x"]]),
            40,
            Style::default(),
            Style::default(),
            Style::default(),
            IconTier::Unicode,
        );
        assert!(out.is_empty());
    }

    #[test]
    fn layout_is_header_rule_then_rows() {
        let cols = [
            col("A", 1, 10, 0, Align::Left),
            col("B", 1, 10, 1, Align::Left),
        ];
        let out = render_table(
            &cols,
            &rows(&[&["aa", "b"], &["c", "dd"]]),
            40,
            Style::default(),
            Style::default(),
            Style::default(),
            IconTier::Unicode,
        );
        assert_eq!(out.len(), 4);
        assert_eq!(line_text(&out[0]), "A   B ");
        assert!(line_text(&out[1]).starts_with('─'), "rule row");
        assert_eq!(line_text(&out[2]), "aa  b ");
        assert_eq!(line_text(&out[3]), "c   dd");
    }

    #[test]
    fn header_cells_are_bold() {
        let cols = [col("A", 1, 10, 0, Align::Left)];
        let out = render_table(
            &cols,
            &rows(&[&["x"]]),
            40,
            Style::default(),
            Style::default(),
            Style::default(),
            IconTier::Unicode,
        );
        let header_span = &out[0].spans[0];
        assert!(header_span.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn right_alignment_pads_on_the_left() {
        let cols = [
            col("Name", 1, 10, 0, Align::Left),
            col("Num", 1, 10, 1, Align::Right),
        ];
        let out = render_table(
            &cols,
            &rows(&[&["a", "5"]]),
            40,
            Style::default(),
            Style::default(),
            Style::default(),
            IconTier::Unicode,
        );
        // Column widths come from the headers (4 and 3).
        assert_eq!(line_text(&out[2]), "a       5");
    }

    #[test]
    fn max_caps_column_and_truncates_with_ellipsis() {
        let cols = [col("A", 1, 4, 0, Align::Left)];
        let out = render_table(
            &cols,
            &rows(&[&["abcdef"]]),
            40,
            Style::default(),
            Style::default(),
            Style::default(),
            IconTier::Unicode,
        );
        assert_eq!(line_text(&out[2]), "abc…");
    }

    #[test]
    fn cjk_cell_truncates_on_glyph_boundary() {
        // half glyph.
        let cols = [col("A", 1, 5, 0, Align::Left)];
        let out = render_table(
            &cols,
            &rows(&[&["你好世界"]]),
            40,
            Style::default(),
            Style::default(),
            Style::default(),
            IconTier::Unicode,
        );
        assert_eq!(line_text(&out[2]), "你好…");
        assert_eq!(UnicodeWidthStr::width(line_text(&out[2]).as_str()), 5);
    }

    #[test]
    fn cjk_cell_that_fits_pads_to_column_width() {
        let cols = [
            col("A", 1, 10, 0, Align::Left),
            col("B", 1, 10, 1, Align::Left),
        ];
        let out = render_table(
            &cols,
            &rows(&[&["你好", "x"], &["yyyyy", "z"]]),
            40,
            Style::default(),
            Style::default(),
            Style::default(),
            IconTier::Unicode,
        );
        assert_eq!(line_text(&out[2]), "你好   x");
        assert_eq!(line_text(&out[3]), "yyyyy  z");
        // Both body rows occupy identical display width.
        assert_eq!(
            UnicodeWidthStr::width(line_text(&out[2]).as_str()),
            UnicodeWidthStr::width(line_text(&out[3]).as_str()),
        );
    }

    #[test]
    fn narrow_width_shrinks_lowest_priority_column_first() {
        let cols = [
            col("A", 2, 20, 0, Align::Left), // priority 0 → sacrificed first
            col("B", 2, 20, 1, Align::Left),
        ];
        // Natural widths 10 + 10 + gap 2 = 22; table width 16 → A shrinks to 4.
        let out = render_table(
            &cols,
            &rows(&[&["aaaaaaaaaa", "bbbbbbbbbb"]]),
            16,
            Style::default(),
            Style::default(),
            Style::default(),
            IconTier::Unicode,
        );
        assert_eq!(line_text(&out[2]), "aaa…  bbbbbbbbbb");
        assert_eq!(UnicodeWidthStr::width(line_text(&out[2]).as_str()), 16);
    }

    #[test]
    fn width_smaller_than_min_sum_does_not_hang() {
        let cols = [
            col("A", 2, 20, 0, Align::Left),
            col("B", 2, 20, 1, Align::Left),
        ];
        // min 2 + min 2 + gap 2 = 6 > width 3: nothing left to shrink → columns
        // stay at their mins (deliberate overflow, but no infinite loop).
        let out = render_table(
            &cols,
            &rows(&[&["aaaa", "bbbb"]]),
            3,
            Style::default(),
            Style::default(),
            Style::default(),
            IconTier::Unicode,
        );
        assert_eq!(line_text(&out[2]), "a…  b…");
    }

    #[test]
    fn missing_cells_render_as_blank_padding() {
        let cols = [
            col("A", 1, 10, 0, Align::Left),
            col("B", 1, 10, 1, Align::Left),
        ];
        let out = render_table(
            &cols,
            &rows(&[&["only-a"]]),
            40,
            Style::default(),
            Style::default(),
            Style::default(),
            IconTier::Unicode,
        );
        assert_eq!(line_text(&out[2]), "only-a  ".to_string() + " ");
    }
}
