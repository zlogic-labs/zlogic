//! matrix — green-on-black.
use ratatui::style::Color;

use crate::theme::{Theme, TokenAnsi16, TokenHex, A16};

/// Manual ansi16 map. The theme is monochrome-green by identity, so ansi16
/// keeps hierarchy with the two greens + cyans; charts borrow White/Gray for
/// distinguishability (6 series can't fit in 2 greens).
static ANSI16: TokenAnsi16 = TokenAnsi16 {
    accent: A16::C(Color::LightGreen),
    accent_soft: A16::C(Color::Green),
    border: A16::Dim,
    muted: A16::Dim,
    success: A16::C(Color::LightGreen),
    error: A16::C(Color::LightRed),
    warning: A16::C(Color::Yellow),
    info: A16::C(Color::LightCyan),
    thinking: A16::Dim,
    tool_running: A16::C(Color::LightCyan),
    chart: &[
        Color::LightGreen,
        Color::Green,
        Color::LightCyan,
        Color::Cyan,
        Color::White,
        Color::Gray,
    ],
    logo: &[Color::Green, Color::LightGreen, Color::LightGreen],
};

pub fn theme() -> Theme {
    Theme::from_hex(
        "matrix",
        "Matrix",
        true,
        TokenHex {
            accent: "#00ff00",
            accent_soft: "#33cc33",
            border: "#114411",
            muted: "#227722",
            success: "#00ff66",
            error: "#ff5555",
            warning: "#cccc00",
            info: "#00cc99",
            thinking: "#227722",
            tool_running: "#00cc99",
            chart: &[
                "#00ff66", "#33cc33", "#66ff66", "#00cc99", "#99ff99", "#00ff00",
            ],
            // Gradient stops (left→right since the logo redesign): keep every stop
            // readable on black — the old #003300 start vanished as a gradient origin.
            logo: &["#00aa00", "#00ff00", "#99ff99"],
        },
        &ANSI16,
    )
}
