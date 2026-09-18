//! dark — One-Dark baseline.
use ratatui::style::Color;

use crate::theme::{Theme, TokenAnsi16, TokenHex, A16};

/// Manual ansi16 map. Dark theme: no plain `Blue` (conhost #000080 unreadable —
/// bright `LightBlue` instead); aux text is `Dim`, never bright-black.
static ANSI16: TokenAnsi16 = TokenAnsi16 {
    accent: A16::C(Color::LightMagenta),
    accent_soft: A16::C(Color::Cyan),
    border: A16::Dim,
    muted: A16::Dim,
    success: A16::C(Color::Green),
    error: A16::C(Color::LightRed),
    warning: A16::C(Color::Yellow),
    info: A16::C(Color::LightBlue),
    thinking: A16::Dim,
    tool_running: A16::C(Color::Cyan),
    chart: &[
        Color::LightBlue,
        Color::Green,
        Color::Yellow,
        Color::LightRed,
        Color::LightMagenta,
        Color::Cyan,
    ],
    logo: &[Color::Cyan, Color::LightBlue, Color::LightMagenta],
};

pub fn theme() -> Theme {
    Theme::from_hex(
        "dark",
        "Dark",
        true,
        TokenHex {
            accent: "#c678dd",
            accent_soft: "#56b6c2",
            border: "#3e4451",
            muted: "#7f848e",
            success: "#98c379",
            error: "#e06c75",
            warning: "#e5c07b",
            info: "#61afef",
            thinking: "#7f848e",
            tool_running: "#56b6c2",
            chart: &[
                "#61afef", "#98c379", "#e5c07b", "#e06c75", "#c678dd", "#56b6c2",
            ],
            logo: &["#56b6c2", "#61afef", "#c678dd"],
        },
        &ANSI16,
    )
}
