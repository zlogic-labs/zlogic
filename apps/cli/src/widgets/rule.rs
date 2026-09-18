use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::glyph::{Glyph, IconTier};

pub fn rule(width: u16, style: Style, icons: IconTier) -> Line<'static> {
    let n = usize::from(width.min(80)).max(1);
    Line::from(Span::styled(
        Glyph::RuleHorizontal.render(icons).repeat(n),
        style,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    fn rule_len(width: u16) -> usize {
        let line = rule(width, Style::default(), IconTier::Unicode);
        line.spans[0].content.chars().count()
    }

    #[test]
    fn clamps_to_80_columns_max() {
        assert_eq!(rule_len(80), 80);
        assert_eq!(rule_len(81), 80);
        assert_eq!(rule_len(u16::MAX), 80);
    }

    #[test]
    fn zero_width_still_draws_one_segment() {
        assert_eq!(rule_len(0), 1);
        assert_eq!(rule_len(1), 1);
    }

    #[test]
    fn in_range_width_is_exact_and_styled() {
        let style = Style::default().fg(Color::Blue);
        let line = rule(40, style, IconTier::Unicode);
        assert_eq!(line.spans[0].content.chars().count(), 40);
        assert!(line.spans[0].content.chars().all(|c| c == '─'));
        assert_eq!(line.spans[0].style, style);
    }

    #[test]
    fn ascii_tier_uses_ascii_rule() {
        let line = rule(3, Style::default(), IconTier::Ascii);
        assert_eq!(line.spans[0].content, "---");
    }
}
