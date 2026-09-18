//! god — full-access danger theme (`--god-alert` #ff3b30).
//! Not in the picker — selected only when godMode is on.
use ratatui::style::Color;

use crate::theme::{Theme, TokenAnsi16, TokenHex, A16};

/// Manual ansi16 map. accent==error==LightRed is intentional (god screams red
/// everywhere in rich too); soft/info/file fall back to pink (`LightMagenta`) so the
/// UI keeps SOME hierarchy — naive mapping collapsed seven tokens onto LightRed.
static ANSI16: TokenAnsi16 = TokenAnsi16 {
    accent: A16::C(Color::LightRed),
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
        Color::LightRed,
        Color::Red,
        Color::LightYellow,
        Color::LightMagenta,
        Color::Gray,
        Color::Magenta,
    ],
    logo: &[Color::LightRed, Color::LightMagenta, Color::LightYellow],
};

pub fn theme() -> Theme {
    Theme::from_hex(
        "god",
        "Full access mode",
        true,
        TokenHex {
            accent: "#ff3b30",
            accent_soft: "#ff6b60",
            border: "#4a1512",
            muted: "#8a4b48",
            success: "#98c379",
            error: "#ff3b30",
            warning: "#ffae42",
            info: "#ff6b60",
            thinking: "#8a4b48",
            tool_running: "#ff6b60",
            chart: &[
                "#ff6b60", "#ff3b30", "#ffae42", "#ff8c69", "#ffd0cc", "#c0392b",
            ],
            logo: &["#ff3b30", "#ff6b60", "#ffae42"],
        },
        &ANSI16,
    )
}
