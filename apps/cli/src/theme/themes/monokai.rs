//! monokai.
use ratatui::style::Color;

use crate::theme::{Theme, TokenAnsi16, TokenHex, A16};

/// Manual ansi16 map. Rich accent==error (#f92672) — ansi16 deliberately splits
/// them (accent pink `LightMagenta`, error `LightRed`) so errors still read as errors.
static ANSI16: TokenAnsi16 = TokenAnsi16 {
    accent: A16::C(Color::LightMagenta),
    accent_soft: A16::C(Color::LightCyan),
    border: A16::Dim,
    muted: A16::Dim,
    success: A16::C(Color::LightGreen),
    error: A16::C(Color::LightRed),
    warning: A16::C(Color::LightYellow),
    info: A16::C(Color::LightCyan),
    thinking: A16::Dim,
    tool_running: A16::C(Color::Cyan),
    chart: &[
        Color::LightCyan,
        Color::LightGreen,
        Color::LightYellow,
        Color::LightRed,
        Color::Magenta,
        Color::Yellow,
    ],
    logo: &[Color::LightMagenta, Color::Yellow, Color::LightCyan],
};

pub fn theme() -> Theme {
    Theme::from_hex(
        "monokai",
        "Monokai",
        true,
        TokenHex {
            accent: "#f92672",
            accent_soft: "#66d9ef",
            border: "#49483e",
            muted: "#75715e",
            success: "#a6e22e",
            error: "#f92672",
            warning: "#e6db74",
            info: "#66d9ef",
            thinking: "#75715e",
            tool_running: "#a1efe4",
            chart: &[
                "#66d9ef", "#a6e22e", "#e6db74", "#f92672", "#ae81ff", "#fd971f",
            ],
            logo: &["#f92672", "#fd971f", "#66d9ef"],
        },
        &ANSI16,
    )
}
