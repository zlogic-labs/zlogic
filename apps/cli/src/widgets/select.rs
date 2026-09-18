use ratatui::style::Modifier;
use ratatui::text::Line;

pub fn maybe_selected(mut line: Line<'static>, selected: bool) -> Line<'static> {
    if selected {
        line.style = line.style.add_modifier(Modifier::REVERSED);
        for span in &mut line.spans {
            span.style = span.style.add_modifier(Modifier::REVERSED);
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Style};
    use ratatui::text::Span;

    fn sample_line() -> Line<'static> {
        Line::from(vec![
            Span::raw("plain "),
            Span::styled("colored", Style::default().fg(Color::Cyan)),
        ])
    }

    #[test]
    fn selected_reverses_line_and_every_span() {
        let out = maybe_selected(sample_line(), true);
        assert!(out.style.add_modifier.contains(Modifier::REVERSED));
        for span in &out.spans {
            assert!(
                span.style.add_modifier.contains(Modifier::REVERSED),
                "span {:?} not reversed",
                span.content
            );
        }
        // Existing styling is kept, not replaced.
        assert_eq!(out.spans[1].style.fg, Some(Color::Cyan));
        // Content untouched.
        let text: String = out.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "plain colored");
    }

    #[test]
    fn unselected_line_is_unchanged() {
        let out = maybe_selected(sample_line(), false);
        assert!(!out.style.add_modifier.contains(Modifier::REVERSED));
        for span in &out.spans {
            assert!(!span.style.add_modifier.contains(Modifier::REVERSED));
        }
        assert_eq!(out.spans[1].style.fg, Some(Color::Cyan));
    }
}
