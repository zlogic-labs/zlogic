use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::glyph::{Glyph, IconTier};
use crate::theme::{Sem, ThemeState};
use crate::widgets::rail;

/// Above this many lines the diff body is truncated to head + tail.
const MAX_LINES: usize = 44;
/// Lines kept on each side of the truncation marker.
const KEEP: usize = 20;

pub fn render_diff(src: &str, theme: &ThemeState, icons: IconTier) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (i, raw) in limit_lines(src.lines().collect(), icons)
        .into_iter()
        .enumerate()
    {
        if i == 0 && !raw.starts_with("@@") && !raw.starts_with('+') && !raw.starts_with('-') {
            lines.push(Line::from(vec![Span::styled(
                raw,
                Style::default()
                    .fg(theme.color(Sem::AccentSoft))
                    .add_modifier(Modifier::BOLD),
            )]));
            continue;
        }
        lines.push(render_diff_line(&raw, i, theme));
    }
    rail::rail_lines(
        &lines,
        Style::default().fg(theme.color(Sem::AccentSoft)),
        icons,
    )
}

fn render_diff_line(raw: &str, idx: usize, theme: &ThemeState) -> Line<'static> {
    if raw.starts_with("@@") {
        return Line::from(Span::styled(
            raw.to_string(),
            Style::default()
                .fg(theme.color(Sem::Info))
                .add_modifier(Modifier::BOLD),
        ));
    }
    let style = if raw.starts_with('+') {
        Style::default().fg(theme.color(Sem::Success))
    } else if raw.starts_with('-') {
        Style::default().fg(theme.color(Sem::Error))
    } else {
        theme.style(Sem::Muted)
    };
    Line::from(vec![
        Span::styled(format!("{idx:>4} "), theme.style(Sem::Muted)),
        Span::styled(raw.to_string(), style),
    ])
}

fn limit_lines(lines: Vec<&str>, icons: IconTier) -> Vec<String> {
    if lines.len() <= MAX_LINES {
        return lines.into_iter().map(str::to_string).collect();
    }
    let omitted = lines.len() - KEEP * 2;
    let mut out: Vec<String> = lines[..KEEP].iter().map(|s| s.to_string()).collect();
    out.push(truncation_marker(omitted, icons));
    out.extend(lines[lines.len() - KEEP..].iter().map(|s| s.to_string()));
    out
}

/// Language-neutral truncation marker (ellipses + omitted line count). The
/// markdown pipeline has no `I18n` handle (callers pass only theme + icons),
/// so the marker deliberately avoids words in any language.
fn truncation_marker(omitted: usize, icons: IconTier) -> String {
    let ell = Glyph::Truncation.render(icons);
    format!("{ell} {omitted} {ell}")
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

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn diff_src(body_lines: usize) -> String {
        let mut out = vec!["diff --git a/f b/f".to_string()];
        for i in 0..body_lines {
            out.push(format!("+line {i}"));
        }
        out.join("\n")
    }

    #[test]
    fn short_diff_is_not_truncated() {
        let lines = render_diff(&diff_src(10), &ts(), IconTier::Unicode);
        assert_eq!(lines.len(), 11); // header + 10 body lines
        let joined: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(!joined.contains('⋯'), "{joined}");
    }

    #[test]
    fn long_diff_truncates_to_head_marker_tail() {
        // 1 header + 99 body = 100 source lines → 20 + marker + 20 output rows
        let lines = render_diff(&diff_src(99), &ts(), IconTier::Unicode);
        assert_eq!(lines.len(), KEEP * 2 + 1);
        let marker = line_text(&lines[KEEP]);
        assert!(marker.contains("⋯⋯ 60 ⋯⋯"), "{marker}");
        // head keeps the file header, tail keeps the last body line
        assert!(line_text(&lines[0]).contains("diff --git"), "head lost");
        assert!(
            line_text(lines.last().expect("tail line")).contains("+line 98"),
            "tail lost"
        );
    }

    #[test]
    fn truncation_marker_is_ascii_at_ascii_tier() {
        let lines = render_diff(&diff_src(99), &ts(), IconTier::Ascii);
        let marker = line_text(&lines[KEEP]);
        assert!(marker.contains("... 60 ..."), "{marker}");
        assert!(!marker.contains('⋯'), "{marker}");
    }

    #[test]
    fn truncation_marker_is_muted_not_diff_colored() {
        let t = ts();
        let lines = render_diff(&diff_src(99), &t, IconTier::Unicode);
        // rail span, line-number span, content span
        let content = lines[KEEP].spans.last().expect("content span");
        assert_eq!(content.style, t.style(Sem::Muted));
    }

    #[test]
    fn hunk_header_is_bold_info() {
        let t = ts();
        let lines = render_diff("@@ -1,2 +1,2 @@\n ctx", &t, IconTier::Unicode);
        let hunk = lines[0].spans.last().expect("hunk span");
        assert_eq!(hunk.style.fg, Some(t.color(Sem::Info)));
        assert!(hunk.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn add_and_remove_lines_use_status_colors() {
        let t = ts();
        let lines = render_diff("header\n+add\n-del\n ctx", &t, IconTier::Unicode);
        let span_of = |text: &str| {
            lines
                .iter()
                .flat_map(|l| l.spans.iter())
                .find(|s| s.content.as_ref() == text)
                .unwrap_or_else(|| panic!("no span {text:?}"))
                .style
        };
        assert_eq!(span_of("+add").fg, Some(t.color(Sem::Success)));
        assert_eq!(span_of("-del").fg, Some(t.color(Sem::Error)));
        assert_eq!(span_of(" ctx").fg, t.style(Sem::Muted).fg);
    }
}
