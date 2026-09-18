//! light — GitHub Light reference.
use ratatui::style::Color;

use crate::theme::{Theme, TokenAnsi16, TokenHex, A16};

/// Manual ansi16 map. Light theme: never bright `LightYellow` on white; plain
/// `Yellow` (index 3 — dark amber/olive in all four benchmark palettes) carries the
/// warning/dir semantics readably. Plain `Blue` is fine on light backgrounds (the
/// blue ban is dark-theme only). Aux text is `Dim`, never bright-black.
static ANSI16: TokenAnsi16 = TokenAnsi16 {
    accent: A16::C(Color::Magenta),
    accent_soft: A16::C(Color::Cyan),
    border: A16::Dim,
    muted: A16::Dim,
    success: A16::C(Color::Green),
    error: A16::C(Color::Red),
    warning: A16::C(Color::Yellow),
    info: A16::C(Color::Blue),
    thinking: A16::Dim,
    tool_running: A16::C(Color::Cyan),
    chart: &[
        Color::Blue,
        Color::Green,
        Color::Yellow,
        Color::Red,
        Color::Magenta,
        Color::Cyan,
    ],
    logo: &[Color::Cyan, Color::Blue, Color::Magenta],
};

pub fn theme() -> Theme {
    Theme::from_hex(
        "light",
        "Light",
        false,
        TokenHex {
            accent: "#6f42c1",
            accent_soft: "#1b7c83",
            border: "#d0d7de",
            muted: "#6e7781",
            success: "#1a7f37",
            error: "#cf222e",
            warning: "#9a6700",
            info: "#0969da",
            thinking: "#6e7781",
            tool_running: "#1b7c83",
            chart: &[
                "#0969da", "#1a7f37", "#9a6700", "#cf222e", "#6f42c1", "#1b7c83",
            ],
            logo: &["#1b7c83", "#0969da", "#6f42c1"],
        },
        &ANSI16,
    )
}
