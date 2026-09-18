use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::app::AppState;
use crate::glyph::Glyph;
use crate::markdown::block;
use crate::session::dto::{Message, Part};
use crate::theme::Sem;

const MESSAGE_PADDING: usize = 2;

/// Re-renders the complete accumulated stream on every frame. This keeps partial
/// Markdown safe: an unfinished marker is literal until a later delta closes it.
pub fn assistant_lines(state: &AppState, text: &str) -> Vec<Line<'static>> {
    block::render_markdown(text, &state.theme, state.icons)
        .into_iter()
        .map(indent)
        .collect()
}

/// Render structured user parts without turning every part into a visual block.
/// Attachments are `@name`, skills are `/name`; only newlines present in Text parts
/// create new rows.
pub fn user_lines(state: &AppState, message: &Message) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Vec::<Span<'static>>::new())];
    let mut force_newline = false;
    for part in &message.parts {
        match part {
            Part::Text { text } => append_markdown(state, &mut lines, text, &mut force_newline),
            Part::File { display, .. }
            | Part::Directory { display, .. }
            | Part::Image { display, .. } => {
                append_token(
                    &mut lines,
                    format!("@{}", display.trim_start_matches('@')),
                    state.theme.style(Sem::AccentSoft),
                    &mut force_newline,
                );
            }
            Part::Url { url, display } => append_token(
                &mut lines,
                display.clone().unwrap_or_else(|| url.clone()),
                state.theme.style(Sem::AccentSoft),
                &mut force_newline,
            ),
            Part::Command { name, text, .. } => {
                append_token(
                    &mut lines,
                    format!("/{}", name.trim_start_matches('/')),
                    state.theme.style(Sem::AccentSoft),
                    &mut force_newline,
                );
                append_markdown(state, &mut lines, text, &mut force_newline);
            }
        }
    }
    let mut rendered: Vec<Line<'static>> = lines.into_iter().map(indent).collect();
    if let Some(first) = rendered.first_mut() {
        first.spans.insert(
            1,
            Span::styled(
                format!("{} ", Glyph::Prompt.render(state.icons)),
                Style::default()
                    .fg(state.theme.color(Sem::Accent))
                    .add_modifier(Modifier::BOLD),
            ),
        );
    }
    rendered
}

fn append_markdown(
    state: &AppState,
    lines: &mut Vec<Line<'static>>,
    text: &str,
    force_newline: &mut bool,
) {
    let text = text.trim_matches(|ch| ch == ' ' || ch == '\t');
    if text.is_empty() {
        if text.contains('\n') {
            *force_newline = true;
        }
        return;
    }
    let rendered = block::render_markdown(text, &state.theme, state.icons);
    if rendered.is_empty() {
        return;
    }
    let mut rendered = rendered.into_iter();
    if let Some(first) = rendered.next() {
        if *force_newline {
            lines.push(first);
        } else {
            append_line(lines.last_mut().expect("one line exists"), first);
        }
    }
    lines.extend(rendered);
    *force_newline = text.ends_with('\n');
}

fn append_token(
    lines: &mut Vec<Line<'static>>,
    token: String,
    style: ratatui::style::Style,
    force_newline: &mut bool,
) {
    if *force_newline {
        lines.push(Line::from(Vec::<Span<'static>>::new()));
        *force_newline = false;
    }
    let line = lines.last_mut().expect("one line exists");
    add_separator(line);
    line.spans.push(Span::styled(token, style));
}

fn append_line(target: &mut Line<'static>, mut source: Line<'static>) {
    if !source.spans.is_empty() {
        add_separator(target);
        target.spans.append(&mut source.spans);
    }
}

fn add_separator(line: &mut Line<'static>) {
    if line
        .spans
        .iter()
        .any(|span| !span.content.as_ref().is_empty())
    {
        line.spans.push(Span::raw(" "));
    }
}

fn indent(mut line: Line<'static>) -> Line<'static> {
    line.spans.insert(0, Span::raw(" ".repeat(MESSAGE_PADDING)));
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyph::IconTier;
    use crate::session::dto::Part;
    use crate::theme::{themes, ColorTier, ThemeState};

    #[test]
    fn streaming_text_is_rendered_as_markdown() {
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
        let lines = assistant_lines(&state, "# Title\n\n**bold** and `code`");
        let rendered = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(rendered.contains("Title"));
        assert!(rendered.contains("bold and code"));
        assert!(!rendered.contains("**"));
    }

    #[test]
    fn structured_user_parts_stay_inline_and_use_type_prefixes() {
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
        let message = Message::from_parts(vec![
            Part::File {
                path: "Cargo.toml".into(),
                display: "Cargo.toml".into(),
                preview: None,
            },
            Part::Text {
                text: "hello **world**".into(),
            },
            Part::File {
                path: "README.md".into(),
                display: "README.md".into(),
                preview: None,
            },
        ]);
        let lines = user_lines(&state, &message);
        assert_eq!(lines.len(), 1);
        let text = lines[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(text, "  > @Cargo.toml hello world @README.md");
    }

    #[test]
    fn only_explicit_user_newlines_split_structured_parts() {
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
        let message = Message::from_parts(vec![
            Part::File {
                path: "Cargo.toml".into(),
                display: "Cargo.toml".into(),
                preview: None,
            },
            Part::Text {
                text: "\nhello".into(),
            },
            Part::File {
                path: "README.md".into(),
                display: "README.md".into(),
                preview: None,
            },
        ]);
        let lines = user_lines(&state, &message);
        let text = |line: &Line<'static>| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };
        assert_eq!(lines.len(), 2);
        assert_eq!(text(&lines[0]), "  > @Cargo.toml");
        assert_eq!(text(&lines[1]), "  hello @README.md");
    }
}
