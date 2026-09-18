use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use crate::glyph::{Glyph, IconTier};
use crate::theme::{Sem, ThemeState};

#[derive(Clone, Copy)]
pub enum LogKind {
    User,
    Assistant,
    Thinking,
    Round,
    Tool,
    SubAgent,
    Compaction,
    Steering,
    Permission,
    Notice,
    Error,
}

pub fn line(
    kind: LogKind,
    text: impl Into<String>,
    theme: &ThemeState,
    icons: IconTier,
) -> Line<'static> {
    let text = text.into();
    let (indent, glyph, sem, body_sem, modifier) = spec(kind);
    let mut spans = Vec::new();
    if indent > 0 {
        spans.push(Span::raw(" ".repeat(indent)));
    }
    if let Some(g) = glyph {
        spans.push(Span::styled(
            format!("{} ", g.render(icons)),
            theme.style(sem).add_modifier(Modifier::BOLD),
        ));
    }
    spans.push(Span::styled(
        text,
        theme.style(body_sem).add_modifier(modifier),
    ));
    Line::from(spans)
}

fn spec(kind: LogKind) -> (usize, Option<Glyph>, Sem, Sem, Modifier) {
    match kind {
        LogKind::User => (
            0,
            Some(Glyph::Prompt),
            Sem::Accent,
            Sem::Muted,
            Modifier::empty(),
        ),
        LogKind::Assistant => (
            0,
            Some(Glyph::Session),
            Sem::AccentSoft,
            Sem::Muted,
            Modifier::empty(),
        ),
        LogKind::Thinking => (
            2,
            Some(Glyph::Thinking),
            Sem::ToolRunning,
            Sem::Thinking,
            Modifier::ITALIC,
        ),
        LogKind::Round => (
            2,
            Some(Glyph::Round),
            Sem::Info,
            Sem::Muted,
            Modifier::empty(),
        ),
        LogKind::Tool => (
            4,
            Some(Glyph::Tool),
            Sem::AccentSoft,
            Sem::Muted,
            Modifier::empty(),
        ),
        LogKind::SubAgent => (
            2,
            Some(Glyph::Subagent),
            Sem::AccentSoft,
            Sem::Muted,
            Modifier::empty(),
        ),
        LogKind::Compaction => (
            2,
            Some(Glyph::Compaction),
            Sem::Info,
            Sem::Muted,
            Modifier::empty(),
        ),
        LogKind::Steering => (
            2,
            Some(Glyph::Steering),
            Sem::Info,
            Sem::Muted,
            Modifier::empty(),
        ),
        LogKind::Permission => (
            2,
            Some(Glyph::Permission),
            Sem::Warning,
            Sem::Warning,
            Modifier::BOLD,
        ),
        LogKind::Notice => (0, None, Sem::Muted, Sem::Muted, Modifier::empty()),
        LogKind::Error => (0, Some(Glyph::Fail), Sem::Error, Sem::Error, Modifier::BOLD),
    }
}

/// How much of a summary a transcript row shows before it says how much is left.
/// A summary can be long — it stands in for as much conversation as a window holds — and the
/// scrollback is where the *replies* live. Ten lines is enough to recognise what was folded, and the
/// rest stays readable in the session-history view, which is one keystroke away.
/// Public because the caller that renders the live event has to name the same number when it decides
/// whether a "… and N more lines" tail is needed; two constants would drift on the first summary
/// that is exactly the length of one of them.
pub const SUMMARY_LINES: usize = 10;

/// A compaction marker and the summary it wrote.
/// **One function for the live stream and for a reopened session.** They are the same event, and the
/// desktop's reducer states the rule for markers: the types may differ, the picture may not. Two
/// renderers would drift, and the drift is only visible to whoever happens to reopen a session.
/// The summary is the only body in this block, indented under the marker, so the block reads as one
/// thing: "this much conversation became these words".
pub fn compaction_lines(
    marker: impl Into<String>,
    summary: &str,
    more: Option<String>,
    theme: &ThemeState,
    icons: IconTier,
) -> Vec<Line<'static>> {
    let mut lines = vec![line(LogKind::Compaction, marker, theme, icons)];
    let body = summary.lines().filter(|l| !l.trim().is_empty());
    for text in body.clone().take(SUMMARY_LINES) {
        lines.push(compaction_body(text, theme));
    }
    if let Some(more) = more {
        lines.push(compaction_body(&more, theme));
    }
    lines
}

fn compaction_body(text: &str, theme: &ThemeState) -> Line<'static> {
    Line::from(vec![
        Span::raw("    "),
        Span::styled(text.to_string(), theme.style(Sem::Muted)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyph::IconTier;
    use crate::theme::{ColorTier, ThemeState};

    fn theme() -> ThemeState {
        ThemeState {
            theme: crate::theme::by_id("dark").expect("dark theme"),
            tier: ColorTier::Rich,
        }
    }

    fn text_of(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn a_compaction_block_marks_the_range_then_indents_the_summary() {
        let long = (1..=40)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = compaction_lines(
            "compaction · turns 1-6",
            &long,
            Some("⋯⋯ 30 more".into()),
            &theme(),
            IconTier::Unicode,
        );

        assert_eq!(text_of(&lines[0]), "  ⊟ compaction · turns 1-6");
        assert!(text_of(&lines[1]).starts_with("    line 1"));
        assert_eq!(lines.len(), SUMMARY_LINES + 2);
        assert_eq!(text_of(lines.last().unwrap()), "    ⋯⋯ 30 more");
    }

    #[test]
    fn the_summary_keeps_its_own_line_breaks() {
        let lines = compaction_lines(
            "marker",
            "\nfirst\n\nsecond\n",
            None,
            &theme(),
            IconTier::Unicode,
        );
        let body: Vec<String> = lines[1..].iter().map(text_of).collect();
        assert_eq!(body, vec!["    first", "    second"]);
    }

    #[test]
    fn round_and_tools_follow_the_message_hierarchy() {
        let (round_indent, ..) = spec(LogKind::Round);
        let (tool_indent, ..) = spec(LogKind::Tool);
        assert_eq!(round_indent, 2);
        assert_eq!(tool_indent, 4);
    }

    #[test]
    fn round_and_thinking_icons_use_distinct_colors() {
        let (_, _, round_icon, round_body, _) = spec(LogKind::Round);
        assert_eq!(round_icon, Sem::Info);
        assert_eq!(round_body, Sem::Muted);
        let (_, _, think_icon, think_body, _) = spec(LogKind::Thinking);
        assert_eq!(think_icon, Sem::ToolRunning);
        assert_eq!(think_body, Sem::Thinking);
    }
}
