//! Theme system (theme-first). Components depend on `Sem` + `&Theme` only.
//! Themes declare rich hex + a manual ansi16 table; ansi256 derives (tiers.rs).
//! Two lookup entry points:
//! - `style(sem)` — preferred; returns a full `Style` so the ansi16/NO_COLOR tiers can
//! express "default fg + DIM" for auxiliary text (bright-black ban).
//! - `color(sem)` — colour-only call sites (e.g. storing a `Color` in a struct);
//!   DIM-marked tokens degrade to `DarkGray` at ansi16 since a bare `Color` cannot
//!   carry a modifier. Prefer `style` wherever a `Style` is being built.

pub mod resolve;
pub mod themes;
pub mod tiers;
pub mod token;

use ratatui::style::{Color, Modifier, Style};

pub use tiers::{ColorTier, Rgb};
pub use token::Sem;

/// Hex declaration bag a theme fills in — the ONLY place hex literals live.
pub struct TokenHex {
    pub accent: &'static str,
    pub accent_soft: &'static str,
    pub border: &'static str,
    pub muted: &'static str,
    pub success: &'static str,
    pub error: &'static str,
    pub warning: &'static str,
    pub info: &'static str,
    pub thinking: &'static str,
    pub tool_running: &'static str,
    pub chart: &'static [&'static str],
    pub logo: &'static [&'static str],
}

/// One ansi16 table entry (manual mapping).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum A16 {
    /// A named ANSI colour. The tables below ban: `Blue` on dark themes
    /// (conhost #000080 unreadable), `DarkGray` for auxiliary text, `LightYellow`
    /// on light themes.
    C(Color),
    /// Terminal default fg + DIM modifier — the replacement for bright-black
    /// auxiliary text. Readable on every scheme because it derives from the user's
    /// own foreground colour.
    Dim,
}

/// Manual ansi16 mapping a theme declares alongside its rich hex.
/// Calibration baseline: WT (Campbell), conhost legacy, macOS Terminal (Basic),
/// iTerm2 default.
pub struct TokenAnsi16 {
    pub accent: A16,
    pub accent_soft: A16,
    pub border: A16,
    pub muted: A16,
    pub success: A16,
    pub error: A16,
    pub warning: A16,
    pub info: A16,
    pub thinking: A16,
    pub tool_running: A16,
    pub chart: &'static [Color],
    pub logo: &'static [Color],
}

#[derive(Clone)]
pub struct Theme {
    pub id: &'static str,
    pub label: &'static str,
    pub is_dark: bool,
    accent: Rgb,
    accent_soft: Rgb,
    border: Rgb,
    muted: Rgb,
    success: Rgb,
    error: Rgb,
    warning: Rgb,
    info: Rgb,
    thinking: Rgb,
    tool_running: Rgb,
    chart: Vec<Rgb>,
    logo: Vec<Rgb>,
    ansi16: &'static TokenAnsi16,
}

impl Theme {
    pub fn from_hex(
        id: &'static str,
        label: &'static str,
        is_dark: bool,
        h: TokenHex,
        ansi16: &'static TokenAnsi16,
    ) -> Theme {
        Theme {
            id,
            label,
            is_dark,
            accent: Rgb::hex(h.accent),
            accent_soft: Rgb::hex(h.accent_soft),
            border: Rgb::hex(h.border),
            muted: Rgb::hex(h.muted),
            success: Rgb::hex(h.success),
            error: Rgb::hex(h.error),
            warning: Rgb::hex(h.warning),
            info: Rgb::hex(h.info),
            thinking: Rgb::hex(h.thinking),
            tool_running: Rgb::hex(h.tool_running),
            chart: h.chart.iter().map(|s| Rgb::hex(s)).collect(),
            logo: h.logo.iter().map(|s| Rgb::hex(s)).collect(),
            ansi16,
        }
    }

    fn rgb(&self, sem: Sem) -> Rgb {
        match sem {
            Sem::Accent => self.accent,
            Sem::AccentSoft => self.accent_soft,
            Sem::Border => self.border,
            Sem::Muted => self.muted,
            Sem::Success => self.success,
            Sem::Error => self.error,
            Sem::Warning => self.warning,
            Sem::Info => self.info,
            Sem::Thinking => self.thinking,
            Sem::ToolRunning => self.tool_running,
            Sem::Chart(i) => {
                if self.chart.is_empty() {
                    self.accent
                } else {
                    self.chart[i % self.chart.len()]
                }
            }
        }
    }

    /// The manual ansi16 entry for a token.
    fn a16(&self, sem: Sem) -> A16 {
        let t = self.ansi16;
        match sem {
            Sem::Accent => t.accent,
            Sem::AccentSoft => t.accent_soft,
            Sem::Border => t.border,
            Sem::Muted => t.muted,
            Sem::Success => t.success,
            Sem::Error => t.error,
            Sem::Warning => t.warning,
            Sem::Info => t.info,
            Sem::Thinking => t.thinking,
            Sem::ToolRunning => t.tool_running,
            Sem::Chart(i) => {
                if t.chart.is_empty() {
                    t.accent
                } else {
                    A16::C(t.chart[i % t.chart.len()])
                }
            }
        }
    }

    /// Colour-only lookup. Prefer `style` — at ansi16 a DIM-marked token can only
    /// degrade to `DarkGray` here (a bare `Color` can't carry the modifier).
    pub fn color(&self, sem: Sem, tier: ColorTier) -> Color {
        match tier {
            ColorTier::Ansi16 => match self.a16(sem) {
                A16::C(c) => c,
                A16::Dim => Color::DarkGray,
            },
            ColorTier::None => Color::Reset,
            _ => tier.to_color(self.rgb(sem)),
        }
    }

    /// Smooth splash-logo gradient: `t` ∈ [0,1] across the theme's logo stops.
    /// Rich lerps between stops; ansi256 quantizes the lerped colour; ansi16 steps
    /// through the manual stop table (no lerp exists there); NO_COLOR stays colourless.
    pub fn logo_gradient(&self, t: f32, tier: ColorTier) -> Color {
        let t = t.clamp(0.0, 1.0);
        match tier {
            ColorTier::None => Color::Reset,
            ColorTier::Ansi16 => {
                let stops = self.ansi16.logo;
                match stops.is_empty() {
                    true => self.color(Sem::Accent, tier),
                    false => stops[((t * stops.len() as f32) as usize).min(stops.len() - 1)],
                }
            }
            _ => {
                let stops = &self.logo;
                match stops.len() {
                    0 => tier.to_color(self.accent),
                    1 => tier.to_color(stops[0]),
                    n => {
                        let scaled = t * (n - 1) as f32;
                        let i = (scaled as usize).min(n - 2);
                        let f = scaled - i as f32;
                        let (a, b) = (stops[i], stops[i + 1]);
                        let lerp =
                            |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * f).round() as u8;
                        tier.to_color(Rgb(lerp(a.0, b.0), lerp(a.1, b.1), lerp(a.2, b.2)))
                    }
                }
            }
        }
    }

    /// The preferred lookup: a full `Style`, so ansi16/NO_COLOR can express
    /// "default fg + DIM" for auxiliary text.
    pub fn style(&self, sem: Sem, tier: ColorTier) -> Style {
        match tier {
            ColorTier::Ansi16 => match self.a16(sem) {
                A16::C(c) => Style::default().fg(c),
                A16::Dim => Style::default().add_modifier(Modifier::DIM),
            },
            // NO_COLOR: no colour codes at all; keep the muted/aux hierarchy via DIM
            // (the ansi16 table's Dim markers double as the "auxiliary text" set).
            ColorTier::None => match self.a16(sem) {
                A16::Dim => Style::default().add_modifier(Modifier::DIM),
                A16::C(_) => Style::default(),
            },
            _ => Style::default().fg(tier.to_color(self.rgb(sem))),
        }
    }
}

/// All 8 themes. Order is the picker order.
pub fn all() -> Vec<Theme> {
    vec![
        themes::dark::theme(),
        themes::light::theme(),
        themes::matrix::theme(),
        themes::cyberpunk::theme(),
        themes::monokai::theme(),
        themes::blood::theme(),
        themes::god::theme(),
        themes::mono::theme(),
    ]
}

pub fn by_id(id: &str) -> Option<Theme> {
    all().into_iter().find(|t| t.id == id)
}

/// Live theme + colour tier bundle threaded through the UI.
#[derive(Clone)]
pub struct ThemeState {
    pub theme: Theme,
    pub tier: ColorTier,
}

impl ThemeState {
    pub fn color(&self, sem: Sem) -> Color {
        self.theme.color(sem, self.tier)
    }

    pub fn style(&self, sem: Sem) -> Style {
        self.theme.style(sem, self.tier)
    }

    pub fn logo_gradient(&self, t: f32) -> Color {
        self.theme.logo_gradient(t, self.tier)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tokens that must be `Dim` at ansi16 — auxiliary text never uses
    /// bright-black, some schemes render it ≈ background.
    const AUX: [Sem; 3] = [Sem::Border, Sem::Muted, Sem::Thinking];

    fn fg_tokens() -> Vec<Sem> {
        vec![
            Sem::Accent,
            Sem::AccentSoft,
            Sem::Success,
            Sem::Error,
            Sem::Warning,
            Sem::Info,
            Sem::ToolRunning,
        ]
    }

    #[test]
    fn aux_tokens_are_dim_at_ansi16_in_every_theme() {
        for t in all() {
            for sem in AUX {
                assert_eq!(
                    t.a16(sem),
                    A16::Dim,
                    "{}: {:?} must be Dim at ansi16 (bright-black ban)",
                    t.id,
                    sem
                );
            }
        }
    }

    #[test]
    fn dark_themes_never_emit_plain_blue_at_ansi16() {
        // Campbell #0037DA too deep, conhost #000080 unreadable on dark.
        for t in all().into_iter().filter(|t| t.is_dark) {
            for sem in fg_tokens() {
                assert_ne!(
                    t.color(sem, ColorTier::Ansi16),
                    Color::Blue,
                    "{}: {:?} maps to plain Blue on a dark theme",
                    t.id,
                    sem
                );
            }
            for (i, c) in t.ansi16.chart.iter().enumerate() {
                assert_ne!(*c, Color::Blue, "{}: chart[{i}] is plain Blue", t.id);
            }
            for (i, c) in t.ansi16.logo.iter().enumerate() {
                assert_ne!(*c, Color::Blue, "{}: logo[{i}] is plain Blue", t.id);
            }
        }
    }

    #[test]
    fn light_themes_never_emit_bright_yellow_at_ansi16() {
        // Bright yellow unreadable on white.
        for t in all().into_iter().filter(|t| !t.is_dark) {
            for sem in fg_tokens() {
                assert_ne!(
                    t.color(sem, ColorTier::Ansi16),
                    Color::LightYellow,
                    "{}: {:?} maps to LightYellow on a light theme",
                    t.id,
                    sem
                );
            }
        }
    }

    #[test]
    fn status_colors_stay_distinct_at_ansi16() {
        // mono is exempt: grayscale by identity, semantics ride on icons/labels.
        for t in all().into_iter().filter(|t| t.id != "mono") {
            let status = [Sem::Success, Sem::Error, Sem::Warning, Sem::Info];
            for a in 0..status.len() {
                for b in (a + 1)..status.len() {
                    assert_ne!(
                        t.color(status[a], ColorTier::Ansi16),
                        t.color(status[b], ColorTier::Ansi16),
                        "{}: {:?} and {:?} collapse at ansi16",
                        t.id,
                        status[a],
                        status[b]
                    );
                }
            }
        }
    }

    #[test]
    fn chart_palette_entries_distinct_at_ansi16() {
        for t in all() {
            let chart = t.ansi16.chart;
            for a in 0..chart.len() {
                for b in (a + 1)..chart.len() {
                    assert_ne!(
                        chart[a], chart[b],
                        "{}: chart[{a}] == chart[{b}] at ansi16",
                        t.id
                    );
                }
            }
        }
    }

    #[test]
    fn no_color_tier_emits_no_color_codes() {
        for t in all() {
            for sem in fg_tokens().into_iter().chain(AUX) {
                assert_eq!(t.color(sem, ColorTier::None), Color::Reset, "{}", t.id);
                let style = t.style(sem, ColorTier::None);
                assert_eq!(style.fg, None, "{}: {:?} sets fg under NO_COLOR", t.id, sem);
            }
            // Aux hierarchy survives via DIM, not colour.
            for sem in AUX {
                assert!(
                    t.style(sem, ColorTier::None)
                        .add_modifier
                        .contains(Modifier::DIM),
                    "{}: {:?} lost DIM under NO_COLOR",
                    t.id,
                    sem
                );
            }
        }
    }

    #[test]
    fn ansi16_dim_style_has_no_fg() {
        for t in all() {
            for sem in AUX {
                let style = t.style(sem, ColorTier::Ansi16);
                assert_eq!(style.fg, None, "{}: {:?}", t.id, sem);
                assert!(style.add_modifier.contains(Modifier::DIM), "{}", t.id);
            }
        }
    }

    #[test]
    fn weighted_fallback_keeps_hue_for_desaturated_green() {
        // Raw RGB² distance mapped One-Dark success #98c379 to DarkGray ("success
        // rendered as muted") — the luminance-weighted metric must keep it green.
        let c = tiers::nearest_ansi16(Rgb::hex("#98c379"));
        assert!(
            c == Color::Green || c == Color::LightGreen,
            "expected a green, got {c:?}"
        );
    }

    #[test]
    fn rich_tier_is_untouched_by_the_tables() {
        let t = by_id("dark").unwrap();
        assert_eq!(
            t.color(Sem::Accent, ColorTier::Rich),
            Color::Rgb(0xc6, 0x78, 0xdd)
        );
        assert_eq!(
            t.style(Sem::Muted, ColorTier::Rich).fg,
            Some(Color::Rgb(0x7f, 0x84, 0x8e))
        );
    }
}
