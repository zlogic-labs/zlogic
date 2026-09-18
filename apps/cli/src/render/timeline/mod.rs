//! Per-kind timeline renderers for normalized turn/round state.

pub mod interaction;
pub mod mailbox;
pub mod message;
pub mod round;
pub mod thinking;
pub mod tool;
pub mod usage;

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::app::{AppState, RoundBlock, ViewMode};
use crate::theme::Sem;

pub fn live_lines(state: &AppState, spinner: &str) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if !state.live.active {
        return lines;
    }
    let mode = state.view_mode;
    for (index, item) in state.live.rounds.iter().enumerate() {
        if index > 0 {
            lines.push(Line::default());
        }
        lines.push(round::header(state, index, item));
        for block in &item.blocks {
            match block {
                RoundBlock::Thinking(thinking) => {
                    lines.extend(thinking::lines(state, spinner, thinking, mode));
                }
                RoundBlock::Tool(row) if mode != ViewMode::Minimal => {
                    lines.push(tool::line(state, spinner, row, mode));
                }
                RoundBlock::Tool(_) => {}
                RoundBlock::Text(text) => lines.extend(message::assistant_lines(state, text)),
                RoundBlock::Note(note) => lines.push(mailbox::line(state, note)),
                RoundBlock::Mailbox(entry) => {
                    lines.extend(mailbox::consumed_entry_lines(state, entry));
                }
            }
        }
    }
    let show_loading = match state.live.rounds.last().map(|round| round.blocks.last()) {
        None | Some(None) => true,
        Some(Some(RoundBlock::Text(_))) => true,
        _ => false,
    };
    if show_loading {
        let mut args = fluent_bundle::FluentArgs::new();
        args.set("spinner", spinner);
        let mut line = Line::from(vec![Span::raw("   ")]);
        line.spans.push(Span::styled(
            state.i18n.format("live-generating", Some(&args)),
            Style::default().fg(state.theme.color(Sem::ToolRunning)),
        ));
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyph::IconTier;
    use crate::theme::{themes, ColorTier, ThemeState};

    fn state() -> AppState {
        AppState::new(
            ThemeState {
                theme: themes::dark::theme(),
                tier: ColorTier::Rich,
            },
            IconTier::Ascii,
            80,
            24,
            24,
        )
    }

    #[test]
    fn idle_turn_renders_no_live_lines() {
        let s = state();
        assert!(live_lines(&s, "-").is_empty());
    }

    #[test]
    fn streaming_text_keeps_a_loading_line_below_the_reply() {
        let mut s = state();
        s.live.active = true;
        s.live.start_round("r".into());
        s.live.current_round_mut().append_text("hello".into());
        let lines = live_lines(&s, "-");
        assert!(
            lines.iter().any(|line| line
                .spans
                .iter()
                .any(|span| span.content.as_ref().contains("hello"))),
            "reply text missing"
        );
        let last = lines.last().expect("loading line below the reply");
        let text: String = last
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(
            text.trim_start().starts_with("-"),
            "expected spinner line, got {text:?}"
        );
        assert!(
            text.contains("生成") || text.contains("generating"),
            "{text:?}"
        );
    }

    #[test]
    fn awaiting_first_round_still_shows_a_loading_line() {
        let mut s = state();
        s.live.active = true;
        let lines = live_lines(&s, "-");
        assert_eq!(
            lines.len(),
            1,
            "only the loading line while awaiting: {lines:?}"
        );
    }

    #[test]
    fn consecutive_rounds_are_separated_by_a_blank_line() {
        let mut s = state();
        s.live.active = true;
        s.live.start_round("r1".into());
        s.live.current_round_mut().append_text("first round".into());
        s.live.start_round("r2".into());
        s.live
            .current_round_mut()
            .append_text("second round".into());
        let lines = live_lines(&s, "-");
        let blank = lines
            .iter()
            .position(|line| line.spans.is_empty())
            .expect("blank line between rounds");
        assert_eq!(
            blank, 2,
            "round 1 head + text, then blank, then round 2: {lines:?}"
        );
        let after: String = lines[blank + 1]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(after.contains("round 2"), "{after:?}");
    }
}
