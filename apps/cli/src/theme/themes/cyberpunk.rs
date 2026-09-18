//! cyberpunk — neon magenta/cyan.
use ratatui::style::Color;

use crate::theme::{Theme, TokenAnsi16, TokenHex, A16};

/// Manual ansi16 map. Neon reads as the bright variants on dark; no plain
/// `Blue` (conhost #000080) — azure info uses `LightBlue`. Aux text is `Dim`.
static ANSI16: TokenAnsi16 = TokenAnsi16 {
    accent: A16::C(Color::LightMagenta),
    accent_soft: A16::C(Color::LightCyan),
    border: A16::Dim,
    muted: A16::Dim,
    success: A16::C(Color::LightGreen),
    error: A16::C(Color::LightRed),
    warning: A16::C(Color::LightYellow),
    info: A16::C(Color::LightBlue),
    thinking: A16::Dim,
    tool_running: A16::C(Color::LightCyan),
    chart: &[
        Color::LightCyan,
        Color::LightMagenta,
        Color::LightYellow,
        Color::LightGreen,
        Color::LightRed,
        Color::LightBlue,
    ],
    logo: &[Color::LightMagenta, Color::LightBlue, Color::LightCyan],
};

pub fn theme() -> Theme {
    Theme::from_hex(
        "cyberpunk",
        "Cyberpunk",
        true,
        TokenHex {
            accent: "#ff00ff",
            accent_soft: "#00ffff",
            border: "#2a1a4a",
            muted: "#6b5b95",
            success: "#00ff9f",
            error: "#ff3860",
            warning: "#ffd300",
            info: "#00b8ff",
            thinking: "#6b5b95",
            tool_running: "#00ffff",
            chart: &[
                "#00ffff", "#ff00ff", "#ffd300", "#00ff9f", "#ff3860", "#00b8ff",
            ],
            logo: &["#ff00ff", "#00b8ff", "#00ffff"],
        },
        &ANSI16,
    )
}
