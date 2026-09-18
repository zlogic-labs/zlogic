use ratatui::text::Line;

use crate::app::AppState;
use crate::log::classify::{self, LogKind};
use crate::render::timeline::message;
use crate::session::dto::MailboxEntry;

pub fn line(state: &AppState, text: &str) -> Line<'static> {
    classify::line(LogKind::Notice, text.to_string(), &state.theme, state.icons)
}

fn entry_lines(state: &AppState, entry: &MailboxEntry, label_key: &str) -> Vec<Line<'static>> {
    let mut lines = vec![classify::line(
        LogKind::Steering,
        format!("{} · {}", state.i18n.t(label_key), entry.id),
        &state.theme,
        state.icons,
    )];
    for message in &entry.messages {
        lines.extend(
            message::user_lines(state, message)
                .into_iter()
                .map(|mut line| {
                    line.spans.insert(0, ratatui::text::Span::raw("  "));
                    line
                }),
        );
    }
    lines
}

pub fn consumed_entry_lines(state: &AppState, entry: &MailboxEntry) -> Vec<Line<'static>> {
    entry_lines(state, entry, "mailbox-consumed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyph::IconTier;
    use crate::session::dto::{Message, Part};
    use crate::theme::{themes, ColorTier, ThemeState};

    #[test]
    fn consumed_mailbox_renders_every_structured_message() {
        let state = AppState::new(
            ThemeState {
                theme: themes::dark::theme(),
                tier: ColorTier::Rich,
            },
            IconTier::Ascii,
            80,
            24,
            24,
        );
        let entry = MailboxEntry {
            id: "mailbox-1".into(),
            messages: vec![
                Message::text("first"),
                Message::from_parts(vec![Part::File {
                    path: "README.md".into(),
                    display: "README.md".into(),
                    preview: None,
                }]),
            ],
        };
        let text = consumed_entry_lines(&state, &entry)
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("mailbox-1"));
        assert!(text.contains("first"));
        assert!(text.contains("@README.md"));
    }
}
