//! Colour-tier derivation. Themes declare rich hex plus a MANUAL
//! ansi16 override table (`TokenAnsi16` in `mod.rs`); the algorithmic mapping here
//! is only the fallback for tokens a theme leaves un-overridden. The rules encoded
//! by the tables: dark themes never emit `Blue` (Campbell #0037DA too deep, conhost
//! #000080 unreadable), auxiliary text never emits bright-black (`DarkGray` — some
//! schemes render it ≈ background) and uses default-fg + DIM instead, light themes
//! avoid bright yellow on white.

use ratatui::style::Color;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// Parse `#rrggbb` (also tolerates missing `#`). Falls back to white on bad input.
    pub fn hex(s: &str) -> Rgb {
        let s = s.trim_start_matches('#');
        if s.len() == 6 {
            if let (Ok(r), Ok(g), Ok(b)) = (
                u8::from_str_radix(&s[0..2], 16),
                u8::from_str_radix(&s[2..4], 16),
                u8::from_str_radix(&s[4..6], 16),
            ) {
                return Rgb(r, g, b);
            }
        }
        Rgb(0xff, 0xff, 0xff)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorTier {
    Rich,    // truecolor
    Ansi256, // 256-index cube
    Ansi16,  // 16 named colours (manual table, algorithmic fallback)
    /// `NO_COLOR` — emit no colour codes at all (spec-pure); hierarchy is carried by
    /// modifiers (DIM/BOLD) only. `to_color` yields `Color::Reset` (terminal default).
    None,
}

impl ColorTier {
    pub fn to_color(self, c: Rgb) -> Color {
        match self {
            ColorTier::Rich => Color::Rgb(c.0, c.1, c.2),
            ColorTier::Ansi256 => Color::Indexed(rgb_to_256(c)),
            ColorTier::Ansi16 => nearest_ansi16(c),
            ColorTier::None => Color::Reset,
        }
    }
}

/// Standard xterm 6×6×6 cube + grayscale ramp mapping.
fn rgb_to_256(c: Rgb) -> u8 {
    let Rgb(r, g, b) = c;
    // Grayscale shortcut when the channels are close.
    if r.abs_diff(g) < 8 && g.abs_diff(b) < 8 && r.abs_diff(b) < 8 {
        if r < 8 {
            return 16;
        }
        if r > 248 {
            return 231;
        }
        return 232 + ((r as u16 - 8) * 24 / 247) as u8;
    }
    let q = |v: u8| -> u16 {
        // xterm cube steps: 0,95,135,175,215,255
        const STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];
        let mut best = 0u16;
        let mut bestd = u16::MAX;
        for (i, s) in STEPS.iter().enumerate() {
            let d = (v as i16 - *s as i16).unsigned_abs();
            if d < bestd {
                bestd = d;
                best = i as u16;
            }
        }
        best
    };
    (16 + 36 * q(r) + 6 * q(g) + q(b)) as u8
}

/// Nearest of the 16 ANSI colours — ALGORITHMIC FALLBACK ONLY; every shipped theme
/// carries a manual `TokenAnsi16` table that wins over this.
/// Distance is luminance-weighted (2Δr² + 4Δg² + 3Δb²), not raw RGB²: raw distance
/// famously mis-buckets low-saturation hues (One-Dark green #98c379 landed on
/// DarkGray, i.e. "success rendered as muted"). The green-heavy weighting keeps hue
/// identity for exactly those desaturated UI colours.
pub(crate) fn nearest_ansi16(c: Rgb) -> Color {
    const PALETTE: [(Color, Rgb); 16] = [
        (Color::Black, Rgb(0, 0, 0)),
        (Color::Red, Rgb(205, 49, 49)),
        (Color::Green, Rgb(13, 188, 121)),
        (Color::Yellow, Rgb(229, 229, 16)),
        (Color::Blue, Rgb(36, 114, 200)),
        (Color::Magenta, Rgb(188, 63, 188)),
        (Color::Cyan, Rgb(17, 168, 205)),
        (Color::Gray, Rgb(229, 229, 229)),
        (Color::DarkGray, Rgb(102, 102, 102)),
        (Color::LightRed, Rgb(241, 76, 76)),
        (Color::LightGreen, Rgb(35, 209, 139)),
        (Color::LightYellow, Rgb(245, 245, 67)),
        (Color::LightBlue, Rgb(59, 142, 234)),
        (Color::LightMagenta, Rgb(214, 112, 214)),
        (Color::LightCyan, Rgb(41, 184, 219)),
        (Color::White, Rgb(255, 255, 255)),
    ];
    let d2 = |a: Rgb, b: Rgb| -> i32 {
        let dr = a.0 as i32 - b.0 as i32;
        let dg = a.1 as i32 - b.1 as i32;
        let db = a.2 as i32 - b.2 as i32;
        2 * dr * dr + 4 * dg * dg + 3 * db * db
    };
    PALETTE
        .iter()
        .min_by_key(|(_, p)| d2(c, *p))
        .map(|(col, _)| *col)
        .unwrap_or(Color::White)
}
