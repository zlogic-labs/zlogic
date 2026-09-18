use ratatui::text::Line;

use crate::app::{AppState, ReasoningRound, RoundBlock};
use crate::log::classify::{self, LogKind};

/// One-line round header, desktop-style: `round N · Think · read ×2 · 5 steps`.
/// The round uuid stays internal — it exists to match core summaries and group
/// database rows, but it never reaches the screen. The summary mirrors the
/// desktop ActivityGroup one-liner: thinking badge, tool names with ×count
/// (first three, then `…`), and the total work-step count.
pub fn header(state: &AppState, index: usize, round: &ReasoningRound) -> Line<'static> {
    let (summary, has_work) = desktop_summary(state, round);
    if !has_work {
        return classify::line(
            LogKind::Round,
            format!("round {}", index + 1),
            &state.theme,
            state.icons,
        );
    }
    classify::line(
        LogKind::Round,
        format!("round {} · {}", index + 1, summary),
        &state.theme,
        state.icons,
    )
}

/// `Think · read ×2 · edit · 4 steps` — the desktop group summary shape.
/// Returns (text, whether the round has any work blocks at all).
fn desktop_summary(state: &AppState, round: &ReasoningRound) -> (String, bool) {
    let mut think = false;
    let mut steps = 0usize;
    let mut tool_counts: Vec<(String, usize)> = Vec::new();
    for block in &round.blocks {
        match block {
            RoundBlock::Thinking(_) => {
                think = true;
                steps += 1;
            }
            RoundBlock::Tool(row) => {
                steps += 1;
                match tool_counts.iter_mut().find(|(name, _)| name == &row.name) {
                    Some((_, count)) => *count += 1,
                    None => tool_counts.push((row.name.clone(), 1)),
                }
            }
            // Prose / notices / mailbox entries are not "work steps" — same rule
            // as the desktop ActivityGroup, which keeps markdown out of the group.
            RoundBlock::Text(_) | RoundBlock::Note(_) | RoundBlock::Mailbox(_) => {}
        }
    }
    if steps == 0 {
        return (String::new(), false);
    }
    let mut parts: Vec<String> = Vec::new();
    if think {
        parts.push(state.i18n.t("round-think"));
    }
    for (name, count) in tool_counts.iter().take(3) {
        if *count > 1 {
            parts.push(format!("{name} ×{count}"));
        } else {
            parts.push(name.clone());
        }
    }
    if tool_counts.len() > 3 {
        parts.push("…".into());
    }
    parts.push(state.i18n.count("round-steps", steps));
    (parts.join(" · "), true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{ReasoningRound, ToolRow};
    use crate::glyph::IconTier;
    use crate::theme::{themes, ColorTier, ThemeState};

    fn state() -> AppState {
        AppState::new(
            ThemeState {
                theme: themes::dark::theme(),
                tier: ColorTier::Rich,
            },
            IconTier::Unicode,
            80,
            24,
            24,
        )
    }

    fn round_with(blocks: Vec<RoundBlock>) -> ReasoningRound {
        ReasoningRound {
            id: "round-1".into(),
            blocks,
        }
    }

    #[test]
    fn live_round_header_uses_the_same_padding_as_history() {
        let line = header(
            &state(),
            0,
            &round_with(vec![RoundBlock::Text("hi".into())]),
        );
        assert_eq!(line.spans[0].content.as_ref(), "  ");
        assert!(line.spans[1].content.contains('◆'));
    }

    #[test]
    fn header_drops_the_uuid_and_shows_tool_stats() {
        let line = header(
            &state(),
            0,
            &round_with(vec![
                RoundBlock::Thinking(crate::app::ThinkingBlock {
                    text: String::new(),
                    open: true,
                }),
                RoundBlock::Tool(ToolRow {
                    id: "t1".into(),
                    name: "read".into(),
                    arg: "a".into(),
                    ok: Some(true),
                    params: None,
                    result: None,
                }),
                RoundBlock::Tool(ToolRow {
                    id: "t2".into(),
                    name: "read".into(),
                    arg: "b".into(),
                    ok: Some(true),
                    params: None,
                    result: None,
                }),
                RoundBlock::Tool(ToolRow {
                    id: "t3".into(),
                    name: "edit".into(),
                    arg: "c".into(),
                    ok: None,
                    params: None,
                    result: None,
                }),
                RoundBlock::Text("prose".into()),
            ]),
        );
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        // The uuid must never appear.
        assert!(!text.contains("round-1"), "uuid leaked into header: {text}");
        assert!(text.contains("round 1"), "{text}");
        assert!(text.contains("Think"), "missing thinking badge: {text}");
        assert!(text.contains("read ×2"), "{text}");
        assert!(text.contains("edit"), "{text}");
        assert!(text.contains("4 steps"), "steps count wrong: {text}");
    }

    #[test]
    fn header_more_than_three_tools_elides_with_ellipsis() {
        let line = header(
            &state(),
            2,
            &round_with(vec![
                RoundBlock::Tool(ToolRow {
                    id: "t1".into(),
                    name: "read".into(),
                    arg: String::new(),
                    ok: Some(true),
                    params: None,
                    result: None,
                }),
                RoundBlock::Tool(ToolRow {
                    id: "t2".into(),
                    name: "grep".into(),
                    arg: String::new(),
                    ok: Some(true),
                    params: None,
                    result: None,
                }),
                RoundBlock::Tool(ToolRow {
                    id: "t3".into(),
                    name: "edit".into(),
                    arg: String::new(),
                    ok: Some(true),
                    params: None,
                    result: None,
                }),
                RoundBlock::Tool(ToolRow {
                    id: "t4".into(),
                    name: "shell".into(),
                    arg: String::new(),
                    ok: Some(true),
                    params: None,
                    result: None,
                }),
            ]),
        );
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.contains("…"), "{text}");
        assert!(text.contains("4 steps"), "{text}");
    }
}
