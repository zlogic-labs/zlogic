//! Theme resolution. `auto` → dark/light via terminal background;
//! explicit id → that theme; `NO_COLOR` → mono; godMode → god (not in the picker).

use crate::term::caps::{Background, TermCaps};

use super::{by_id, Theme, ThemeState};

/// Resolve the startup theme.
pub fn resolve(requested: &str, god_mode: bool, background: Background) -> Theme {
    if god_mode {
        return by_id("god").expect("god theme");
    }
    if std::env::var_os("NO_COLOR").is_some() {
        return by_id("mono").unwrap_or_else(fallback);
    }
    match requested {
        "auto" => by_id(theme_for_background(background)).unwrap_or_else(fallback),
        other => by_id(other).unwrap_or_else(fallback),
    }
}

fn theme_for_background(background: Background) -> &'static str {
    match background {
        Background::Light => "light",
        Background::Dark | Background::Unknown => "dark",
    }
}

pub fn resolve_state(requested: &str, god_mode: bool, caps: &TermCaps) -> ThemeState {
    ThemeState {
        theme: resolve(requested, god_mode, caps.background),
        tier: caps.color_tier,
    }
}

fn fallback() -> Theme {
    by_id("dark").expect("dark theme always present")
}
