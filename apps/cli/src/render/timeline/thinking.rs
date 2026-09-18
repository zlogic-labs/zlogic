use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::app::{AppState, ThinkingBlock, ViewMode};
use crate::glyph::Glyph;
use crate::theme::Sem;

pub fn lines(
    state: &AppState,
    spinner: &str,
    thinking: &ThinkingBlock,
    mode: ViewMode,
) -> Vec<Line<'static>> {
    let marker = Span::styled(
        format!(
            "{} ",
            if thinking.open {
                spinner
            } else {
                Glyph::Thinking.render(state.icons)
            }
        ),
        Style::default().fg(state.theme.color(Sem::ToolRunning)),
    );
    let label = state.i18n.t("live-thinking");
    let mut out = vec![Line::from(vec![
        Span::raw("  "),
        marker,
        Span::styled(
            label,
            state
                .theme
                .style(Sem::Thinking)
                .add_modifier(Modifier::ITALIC),
        ),
    ])];
    let text = thinking.text.trim();
    if mode == ViewMode::Minimal || text.is_empty() {
        return out;
    }
    out.push(Line::from(vec![
        Span::raw("     "),
        Span::styled(
            thinking.text.to_string(),
            state
                .theme
                .style(Sem::Thinking)
                .add_modifier(Modifier::ITALIC),
        ),
    ]));
    out
}
