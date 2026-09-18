use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::glyph::{Glyph, IconTier};

pub fn rail_lines(lines: &[Line<'static>], style: Style, icons: IconTier) -> Vec<Line<'static>> {
    let rail = Glyph::Rail.render(icons).to_string();
    lines
        .iter()
        .cloned()
        .map(|line| {
            let mut spans = vec![Span::styled(rail.clone(), style), Span::raw(" ")];
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    fn text(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn prefixes_every_line_with_styled_rail_and_space() {
        let style = Style::default().fg(Color::Magenta);
        let input = vec![Line::from("first"), Line::from("second")];
        let out = rail_lines(&input, style, IconTier::Unicode);
        assert_eq!(out.len(), 2);
        for (line, body) in out.iter().zip(["first", "second"]) {
            assert_eq!(line.spans[0].content.as_ref(), "▎");
            assert_eq!(line.spans[0].style, style, "rail span carries the style");
            assert_eq!(line.spans[1].content.as_ref(), " ");
            assert_eq!(text(line), format!("▎ {body}"));
        }
    }

    #[test]
    fn ascii_tier_uses_pipe_rail() {
        let out = rail_lines(&[Line::from("x")], Style::default(), IconTier::Ascii);
        assert_eq!(text(&out[0]), "| x");
    }

    #[test]
    fn empty_input_yields_empty_output() {
        let out = rail_lines(&[], Style::default(), IconTier::Unicode);
        assert!(out.is_empty());
    }

    #[test]
    fn preserves_original_spans_after_prefix() {
        let body_style = Style::default().fg(Color::Green);
        let input = vec![Line::from(vec![
            Span::styled("a", body_style),
            Span::raw("b"),
        ])];
        let out = rail_lines(&input, Style::default(), IconTier::Unicode);
        assert_eq!(out[0].spans.len(), 4); // rail + space + a + b
        assert_eq!(out[0].spans[2].style, body_style);
        assert_eq!(text(&out[0]), "▎ ab");
    }
}
