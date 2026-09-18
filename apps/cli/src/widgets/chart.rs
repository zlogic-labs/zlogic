//! Chart primitives for overlays: pure functions from values +
//! geometry + theme to styled `Line`s. No widget state — unit-testable without a
//! terminal. Series colors come from the theme's `Sem::Chart(i)` cycling palette.

use ratatui::text::{Line, Span};

use crate::glyph::{Glyph, IconTier};
use crate::theme::{Sem, ThemeState};

/// Sub-row resolution for unicode bars (sparkline family).
const EIGHTHS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Legend/table marker for series `i` — the caller styles it `Sem::Chart(i)`.
/// ASCII tier falls back to `#` (no U+25A0 on the wide-safe path).
pub fn series_marker(ascii: bool) -> &'static str {
    Glyph::ChartSeries.render(if ascii {
        IconTier::Ascii
    } else {
        IconTier::Unicode
    })
}

pub fn sparkline(values: &[f64], max_width: usize, icons: IconTier) -> String {
    if values.is_empty() || max_width == 0 {
        return String::new();
    }
    let start = values.len().saturating_sub(max_width);
    let slice = &values[start..];
    let min = slice.iter().copied().fold(f64::INFINITY, f64::min);
    let max = slice.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let span = (max - min).max(f64::EPSILON);
    slice
        .iter()
        .map(|value| {
            let index = (((value - min) / span) * (EIGHTHS.len() - 1) as f64).round() as usize;
            if icons == IconTier::Ascii {
                if index == 0 {
                    '.'
                } else {
                    '#'
                }
            } else {
                EIGHTHS[index.min(EIGHTHS.len() - 1)]
            }
        })
        .collect()
}

/// Vertical bar chart: one 1-col bar per value with a 1-col gap, `height` bar rows
/// plus one muted axis row labelling the first/last value index (1-based).
/// - Unicode: eighth-blocks (`▁▂▃▄▅▆▇█`) give sub-row resolution; a non-zero value
///   always shows at least one eighth.
/// - ASCII tier (`ascii = true`): full `#` rows only.
/// - More values than fit in `width` → keeps the LAST values (most recent days).
/// - Empty input or zero geometry → no lines. All-zero values → blank bars.
pub fn bar_chart(
    values: &[f64],
    width: u16,
    height: u16,
    theme: &ThemeState,
    ascii: bool,
) -> Vec<Line<'static>> {
    if values.is_empty() || width == 0 || height == 0 {
        return Vec::new();
    }
    // Bars are `bar gap bar gap … bar` → n bars need 2n-1 columns.
    let max_bars = usize::from(width).div_ceil(2);
    let start = values.len().saturating_sub(max_bars);
    let slice = &values[start..];
    let max = slice.iter().copied().fold(0.0_f64, f64::max);

    let levels_per_row = if ascii { 1 } else { EIGHTHS.len() };
    let total_levels = usize::from(height) * levels_per_row;
    let units: Vec<usize> = slice
        .iter()
        .map(|v| {
            if *v <= 0.0 || max <= 0.0 {
                0
            } else {
                (((v / max) * total_levels as f64).round() as usize).clamp(1, total_levels)
            }
        })
        .collect();

    let mut lines = Vec::with_capacity(usize::from(height) + 1);
    for row in (0..usize::from(height)).rev() {
        // `row` counts from the bottom; the topmost row renders first.
        let mut text = String::with_capacity(slice.len() * 2);
        for (i, u) in units.iter().enumerate() {
            if i > 0 {
                text.push(' ');
            }
            let cell = u.saturating_sub(row * levels_per_row).min(levels_per_row);
            text.push(if cell == 0 {
                ' '
            } else if ascii {
                '#'
            } else {
                EIGHTHS[cell - 1]
            });
        }
        lines.push(Line::from(Span::styled(text, theme.style(Sem::Chart(0)))));
    }
    lines.push(axis_line(
        start + 1,
        start + slice.len(),
        slice.len(),
        theme,
    ));
    lines
}

/// Muted axis row: first index left-aligned under the first bar, last index
/// right-aligned under the last bar (first/last is enough).
fn axis_line(first: usize, last: usize, bars: usize, theme: &ThemeState) -> Line<'static> {
    let chart_w = bars * 2 - 1;
    let left = first.to_string();
    let right = last.to_string();
    let mut text = left.clone();
    if bars > 1 && left.len() + right.len() <= chart_w.saturating_sub(1) {
        text.push_str(&" ".repeat(chart_w - left.len() - right.len()));
        text.push_str(&right);
    }
    Line::from(Span::styled(text, theme.style(Sem::Muted)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{themes, ColorTier, ThemeState};

    fn theme() -> ThemeState {
        ThemeState {
            theme: themes::dark::theme(),
            tier: ColorTier::Rich,
        }
    }

    fn text_of(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn empty_input_or_zero_geometry_renders_nothing() {
        let th = theme();
        assert!(bar_chart(&[], 20, 4, &th, false).is_empty());
        assert!(bar_chart(&[1.0], 0, 4, &th, false).is_empty());
        assert!(bar_chart(&[1.0], 20, 0, &th, false).is_empty());
    }

    #[test]
    fn scaling_maps_max_to_full_block_and_keeps_proportions() {
        let th = theme();
        let lines = bar_chart(&[1.0, 2.0, 4.0], 20, 1, &th, false);
        assert_eq!(lines.len(), 2, "one bar row + one axis row");
        assert_eq!(text_of(&lines[0]), "▂ ▄ █");
        assert_eq!(text_of(&lines[1]), "1   3");
    }

    #[test]
    fn single_value_fills_the_full_height() {
        let th = theme();
        let lines = bar_chart(&[3.5], 20, 3, &th, false);
        assert_eq!(lines.len(), 4);
        for row in &lines[..3] {
            assert_eq!(text_of(row), "█");
        }
        assert_eq!(text_of(&lines[3]), "1");
    }

    #[test]
    fn nonzero_value_never_rounds_to_an_empty_bar() {
        let th = theme();
        let lines = bar_chart(&[0.001, 100.0], 20, 2, &th, false);
        let bottom = text_of(&lines[1]);
        assert_eq!(
            bottom.chars().next(),
            Some('▁'),
            "tiny value shows one eighth"
        );
    }

    #[test]
    fn ascii_tier_uses_full_hash_rows_only() {
        let th = theme();
        let lines = bar_chart(&[1.0, 2.0], 20, 2, &th, true);
        assert_eq!(lines.len(), 3);
        assert_eq!(text_of(&lines[0]), "  #");
        assert_eq!(text_of(&lines[1]), "# #");
        assert_eq!(text_of(&lines[2]), "1 2");
        for line in &lines[..2] {
            assert!(text_of(line).chars().all(|c| c == '#' || c == ' '));
        }
    }

    #[test]
    fn narrow_width_keeps_the_most_recent_values() {
        let th = theme();
        // width 5 → at most 3 bars; of 1..=14 only 12/13/14 survive.
        let values: Vec<f64> = (1..=14).map(|v| v as f64).collect();
        let lines = bar_chart(&values, 5, 1, &th, false);
        assert_eq!(text_of(&lines[0]).chars().count(), 5);
        assert_eq!(text_of(&lines[1]), "12 14");
    }

    #[test]
    fn all_zero_values_render_blank_bars_without_panicking() {
        let th = theme();
        let lines = bar_chart(&[0.0, 0.0, 0.0], 20, 2, &th, false);
        assert_eq!(lines.len(), 3);
        assert!(text_of(&lines[0]).trim().is_empty());
        assert!(text_of(&lines[1]).trim().is_empty());
    }

    #[test]
    fn series_marker_has_ascii_fallback() {
        assert_eq!(series_marker(false), "■");
        assert_eq!(series_marker(true), "#");
    }

    #[test]
    fn sparkline_respects_icon_tier_and_width() {
        let values = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(sparkline(&values, 3, IconTier::Unicode), "▁▅█");
        assert_eq!(sparkline(&values, 3, IconTier::Ascii), ".##");
    }
}
