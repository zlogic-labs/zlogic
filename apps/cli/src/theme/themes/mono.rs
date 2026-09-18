//! mono — grayscale. Also the `NO_COLOR` target (colour never the sole
//! information carrier — icons/labels carry semantics, so the status tokens are
//! ALLOWED to collapse here; every other theme keeps them distinct).
use ratatui::style::Color;

use crate::theme::{Theme, TokenAnsi16, TokenHex, A16};

/// Manual ansi16 map. Monochrome by identity: White / Gray / Dim are the whole
/// ladder. Charts cycle 3 grays (6 distinct grays don't exist in ansi16).
static ANSI16: TokenAnsi16 = TokenAnsi16 {
    accent: A16::C(Color::White),
    accent_soft: A16::C(Color::Gray),
    border: A16::Dim,
    muted: A16::Dim,
    success: A16::C(Color::Gray),
    error: A16::C(Color::White),
    warning: A16::C(Color::Gray),
    info: A16::C(Color::Gray),
    thinking: A16::Dim,
    tool_running: A16::C(Color::Gray),
    chart: &[Color::White, Color::Gray, Color::DarkGray],
    logo: &[Color::DarkGray, Color::Gray, Color::White],
};

pub fn theme() -> Theme {
    Theme::from_hex(
        "mono",
        "Mono",
        true,
        TokenHex {
            accent: "#ffffff",
            accent_soft: "#cccccc",
            border: "#666666",
            muted: "#888888",
            success: "#cccccc",
            error: "#ffffff",
            warning: "#cccccc",
            info: "#aaaaaa",
            thinking: "#888888",
            tool_running: "#cccccc",
            chart: &[
                "#ffffff", "#dddddd", "#bbbbbb", "#999999", "#777777", "#555555",
            ],
            logo: &["#555555", "#aaaaaa", "#ffffff"],
        },
        &ANSI16,
    )
}
