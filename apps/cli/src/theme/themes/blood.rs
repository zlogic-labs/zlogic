//! blood — dark red.
use ratatui::style::Color;

use crate::theme::{Theme, TokenAnsi16, TokenHex, A16};

/// Manual ansi16 map. The red family fans out across Red / LightRed /
/// LightMagenta (pink) so error keeps the brightest slot; the naive nearest-colour
/// mapping collapsed six tokens onto LightRed.
static ANSI16: TokenAnsi16 = TokenAnsi16 {
    accent: A16::C(Color::Red),
    accent_soft: A16::C(Color::LightMagenta),
    border: A16::Dim,
    muted: A16::Dim,
    success: A16::C(Color::LightGreen),
    error: A16::C(Color::LightRed),
    warning: A16::C(Color::LightYellow),
    info: A16::C(Color::LightMagenta),
    thinking: A16::Dim,
    tool_running: A16::C(Color::LightMagenta),
    chart: &[
        Color::LightMagenta,
        Color::LightRed,
        Color::Red,
        Color::LightYellow,
        Color::Magenta,
        Color::LightGreen,
    ],
    logo: &[Color::Red, Color::LightRed, Color::LightMagenta],
};

pub fn theme() -> Theme {
    Theme::from_hex(
        "blood",
        "Blood",
        true,
        TokenHex {
            accent: "#ff2d2d",
            accent_soft: "#ff7b7b",
            border: "#3a1010",
            muted: "#7a3b3b",
            success: "#9acd32",
            error: "#ff0000",
            warning: "#ffae42",
            info: "#d98880",
            thinking: "#7a3b3b",
            tool_running: "#ff7b7b",
            chart: &[
                "#ff7b7b", "#ff2d2d", "#ff0000", "#ffae42", "#d98880", "#9acd32",
            ],
            logo: &["#7a0000", "#ff0000", "#ff7b7b"],
        },
        &ANSI16,
    )
}
