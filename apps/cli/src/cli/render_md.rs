//! `--render-md`: render markdown to stdout as ANSI and exit (design/debug aid).
//! Pure pipe: no TUI, no raw mode, no alt-screen — so it is immune to the terminal
//! winsize issues that plague the live viewport, and lets you eyeball the markdown
//! renderer against arbitrary input (`zlogic '**hi**' | zlogic --render-md`).
//! It walks the SAME `block::render_markdown` + `wrap_hist_line` path the live app
//! uses, then converts each styled span to an SGR escape.

use std::io::{Read, Write};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;

use crate::cli::args::Args;
use crate::glyph::IconTier;
use crate::markdown::block;
use crate::render::history::{wrap_hist_line, HistLine};
use crate::theme::ThemeState;

/// Read the source (file path, or `-`/piped stdin), render, print. Returns the
/// process exit code.
pub fn run(args: &Args, theme: &ThemeState, icons: IconTier) -> i32 {
    let src = match args.render_md.as_deref() {
        Some("-") | None => match read_stdin() {
            Ok(source) => source,
            Err(error) => {
                eprintln!("zlogic: cannot read stdin: {error}");
                return 1;
            }
        },
        Some(path) => match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("zlogic: cannot read {path}: {e}");
                return 1;
            }
        },
    };

    let width = args
        .width
        .or_else(|| crossterm::terminal::size().ok().map(|(w, _)| w))
        .unwrap_or(80)
        .max(4) as usize;

    let mut out = std::io::stdout();
    for line in block::render_markdown(&src, theme, icons) {
        // Wrap exactly like the transcript does, so what you see is what the app draws.
        for wrapped in wrap_hist_line(&HistLine::from_line(line), width, width) {
            if let Err(error) = writeln!(out, "{}", line_to_ansi(&wrapped)) {
                if error.kind() == std::io::ErrorKind::BrokenPipe {
                    return 0;
                }
                eprintln!("zlogic: output error: {error}");
                return 1;
            }
        }
    }
    0
}

fn read_stdin() -> std::io::Result<String> {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    Ok(buf)
}

/// A ratatui `Line` → an ANSI string: each span wrapped in its SGR params + reset.
fn line_to_ansi(line: &Line<'static>) -> String {
    let mut s = String::new();
    for span in &line.spans {
        let sgr = style_to_sgr(span.style);
        if sgr.is_empty() {
            s.push_str(&span.content);
        } else {
            s.push_str("\x1b[");
            s.push_str(&sgr);
            s.push('m');
            s.push_str(&span.content);
            s.push_str("\x1b[0m");
        }
    }
    s
}

fn style_to_sgr(style: Style) -> String {
    let mut params: Vec<String> = Vec::new();
    let m = style.add_modifier;
    if m.contains(Modifier::BOLD) {
        params.push("1".into());
    }
    if m.contains(Modifier::DIM) {
        params.push("2".into());
    }
    if m.contains(Modifier::ITALIC) {
        params.push("3".into());
    }
    if m.contains(Modifier::UNDERLINED) {
        params.push("4".into());
    }
    if m.contains(Modifier::REVERSED) {
        params.push("7".into());
    }
    if m.contains(Modifier::CROSSED_OUT) {
        params.push("9".into());
    }
    if let Some(fg) = style.fg {
        params.push(fg_sgr(fg));
    }
    params.join(";")
}

fn fg_sgr(c: Color) -> String {
    match c {
        Color::Reset => "39".into(),
        Color::Black => "30".into(),
        Color::Red => "31".into(),
        Color::Green => "32".into(),
        Color::Yellow => "33".into(),
        Color::Blue => "34".into(),
        Color::Magenta => "35".into(),
        Color::Cyan => "36".into(),
        Color::Gray => "37".into(),
        Color::DarkGray => "90".into(),
        Color::LightRed => "91".into(),
        Color::LightGreen => "92".into(),
        Color::LightYellow => "93".into(),
        Color::LightBlue => "94".into(),
        Color::LightMagenta => "95".into(),
        Color::LightCyan => "96".into(),
        Color::White => "97".into(),
        Color::Rgb(r, g, b) => format!("38;2;{r};{g};{b}"),
        Color::Indexed(i) => format!("38;5;{i}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::Span;

    #[test]
    fn plain_span_has_no_escapes() {
        let line = Line::from(vec![Span::raw("hello")]);
        assert_eq!(line_to_ansi(&line), "hello");
    }

    #[test]
    fn bold_rgb_span_wraps_in_sgr_and_reset() {
        let line = Line::from(vec![Span::styled(
            "x",
            Style::default()
                .fg(Color::Rgb(1, 2, 3))
                .add_modifier(Modifier::BOLD),
        )]);
        assert_eq!(line_to_ansi(&line), "\x1b[1;38;2;1;2;3mx\x1b[0m");
    }

    #[test]
    fn indexed_and_named_colors() {
        assert_eq!(fg_sgr(Color::Indexed(208)), "38;5;208");
        assert_eq!(fg_sgr(Color::LightMagenta), "95");
        assert_eq!(fg_sgr(Color::Reset), "39");
    }

    #[test]
    fn dim_only_span() {
        let line = Line::from(vec![Span::styled(
            "m",
            Style::default().add_modifier(Modifier::DIM),
        )]);
        assert_eq!(line_to_ansi(&line), "\x1b[2mm\x1b[0m");
    }
}
