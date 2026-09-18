//! Startup splash: a big logo written to scrollback, a brief
//! single-line boot animation in the viewport, then a static multi-line help block
//! (a few tips picked pseudo-randomly per launch — no animation).
//! The logo has two embedded variants (no generator dependency): a block-glyph
//! wordmark (ANSI-Shadow style) for Unicode-capable terminals, and a plain-ASCII
//! fallback — block/box-drawing glyphs are East-Asian-AMBIGUOUS width, so any
//! terminal flagged `ambiguous_wide` (and the legacy console) gets the ASCII form
//! via `IconTier::Ascii`. Colour is a smooth diagonal gradient across the theme's
//! logo stops (`Theme::logo_gradient`), not the old 3-colour row cycling.

use ratatui::style::Color;

use unicode_width::UnicodeWidthStr;

use crate::glyph::IconTier;
use crate::theme::ThemeState;

const BOTTOM_PADDING: u16 = 1;
const MIN_TOP_PADDING: u16 = 3;
const MAX_TOP_PADDING: u16 = 24;
const LOGO_ANCHOR_LIFT: u16 = 4;
pub const TIP_CMD_WIDTH: usize = 10;
pub const TIP_GAP: &str = "  ";

/// Boot loading steps, shown one at a time in the viewport (~250ms each), then discarded.
pub const LOADING: &[&str] = &[
    "Initializing workspace",
    "Loading kernel modules",
    "Connecting model router",
    "Calibrating terminal capabilities",
    "Ready",
];

/// Tip pool. A few are picked at random each launch for the static help block.
pub const TIPS: &[(&str, &str)] = &[
    ("/help", "Show help"),
    ("/stats", "Usage stats"),
    ("/model", "Switch / manage models"),
    ("/session", "Session history"),
    ("/new", "New session"),
    ("@file", "Reference file"),
    ("/plan", "Read-only plan"),
    ("/theme", "Switch theme"),
    ("Ctrl+J", "Insert newline"),
    ("Ctrl+C×2", "Quit"),
];

/// Pick `count` distinct tips starting at `seed`, spread by a coprime stride.
pub fn pick_tips(count: usize, seed: usize) -> Vec<usize> {
    let n = TIPS.len();
    let count = count.min(n);
    let start = seed % n;
    const STRIDE: usize = 3; // coprime with 10
    (0..count).map(|i| (start + i * STRIDE) % n).collect()
}

// ── logo art ────────────────────────────────────────────────────────────────
// `ZLOGIC`, borderless. Two embedded variants, each rectangular (every row of a
// variant has the same char count, asserted in tests) so centering stays exact.

/// ANSI-Shadow style block wordmark — Unicode/Nerd icon tiers. Generated with
/// the `figlet` library (font: "ANSI Shadow").
const LOGO_BLOCK: &[&str] = &[
    "███████╗██╗      ██████╗  ██████╗ ██╗ ██████╗",
    "╚══███╔╝██║     ██╔═══██╗██╔════╝ ██║██╔════╝",
    "  ███╔╝ ██║     ██║   ██║██║  ███╗██║██║     ",
    " ███╔╝  ██║     ██║   ██║██║   ██║██║██║     ",
    "███████╗███████╗╚██████╔╝╚██████╔╝██║╚██████╗",
    "╚══════╝╚══════╝ ╚═════╝  ╚═════╝ ╚═╝ ╚═════╝",
    "                                             ",
];

/// Plain-ASCII wordmark — legacy console / ambiguous-width terminals, where
/// block and box-drawing glyphs can render double-width. Generated with the
/// `figlet` library (font: "standard").
const LOGO_ASCII: &[&str] = &[
    "  _____  _        ___     ____   ___    ____ ",
    " |__  / | |      / _ \\   / ___| |_ _|  / ___|",
    "   / /  | |     | | | | | |  _   | |  | |    ",
    "  / /_  | |___  | |_| | | |_| |  | |  | |___ ",
    " /____| |_____|  \\___/   \\____| |___|  \\____|",
    "                                             ",
];

fn logo_body(icons: IconTier) -> &'static [&'static str] {
    match icons {
        IconTier::Ascii => LOGO_ASCII,
        _ => LOGO_BLOCK,
    }
}

/// The logo art lines (no padding).
pub fn logo_lines(icons: IconTier) -> Vec<String> {
    logo_body(icons).iter().map(|l| l.to_string()).collect()
}

pub fn logo_width(icons: IconTier) -> u16 {
    logo_body(icons)
        .first()
        .map(|l| l.chars().count() as u16)
        .unwrap_or(0)
}

pub fn logo_height(icons: IconTier) -> u16 {
    logo_body(icons).len() as u16
}

/// One logo row as (text, colour) runs under a smooth diagonal gradient — left→right
/// dominant with a slight vertical drift. Consecutive same-colour chars are grouped so
/// low-colour tiers emit a handful of spans, not one per char.
pub fn logo_row_chunks(theme: &ThemeState, icons: IconTier, row: usize) -> Vec<(String, Color)> {
    let body = logo_body(icons);
    let Some(line) = body.get(row) else {
        return Vec::new();
    };
    let rows = body.len().max(1);
    let cols = line.chars().count().max(2);
    let mut chunks: Vec<(String, Color)> = Vec::new();
    for (x, ch) in line.chars().enumerate() {
        let xf = x as f32 / (cols - 1) as f32;
        let yf = row as f32 / (rows - 1).max(1) as f32;
        let color = theme.logo_gradient((2.0 * xf + yf) / 3.0);
        match chunks.last_mut() {
            Some((text, c)) if *c == color => text.push(ch),
            _ => chunks.push((ch.to_string(), color)),
        }
    }
    chunks
}

pub fn top_padding(
    visible_rows: u16,
    tip_count: usize,
    reserved_bottom_rows: u16,
    icons: IconTier,
) -> u16 {
    let target = visible_rows.saturating_sub(reserved_bottom_rows);
    let fixed = logo_height(icons) + 1 + tip_count as u16 + BOTTOM_PADDING;
    let max_top = target.saturating_sub(fixed);
    let top = max_top
        .min(MAX_TOP_PADDING)
        .max(MIN_TOP_PADDING.min(max_top));
    top.saturating_sub(LOGO_ANCHOR_LIFT)
}

pub fn tip_count_for_rows(term_rows: u16) -> usize {
    if term_rows >= 30 {
        4
    } else {
        3
    }
}

pub fn tip_indent(term_width: u16) -> String {
    let width = tip_block_width();
    let m = (term_width as usize).saturating_sub(width) / 2;
    " ".repeat(m)
}

pub fn tip_group_width(icons: IconTier) -> usize {
    tip_block_width().max(logo_width(icons) as usize)
}

pub fn tip_inner_padding(icons: IconTier) -> String {
    " ".repeat(tip_group_width(icons).saturating_sub(tip_block_width()) / 2)
}

pub fn tip_block_width() -> usize {
    TIPS.iter()
        .map(|(_, desc)| TIP_CMD_WIDTH + TIP_GAP.len() + UnicodeWidthStr::width(*desc))
        .max()
        .unwrap_or(0)
}

pub fn left_aligned_cmd(cmd: &str) -> String {
    let width = UnicodeWidthStr::width(cmd);
    if width >= TIP_CMD_WIDTH {
        cmd.to_string()
    } else {
        format!("{cmd}{}", " ".repeat(TIP_CMD_WIDTH - width))
    }
}

/// Left margin to center the logo; the info block below aligns to the same left edge
/// (center the block, left-align content inside it).
pub fn indent(term_width: u16, icons: IconTier) -> String {
    let m = (term_width.saturating_sub(logo_width(icons)) / 2) as usize;
    " ".repeat(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{by_id, ColorTier, ThemeState};

    /// Centering math depends on every row of a variant having the same char count.
    #[test]
    fn logo_variants_are_rectangular_and_same_height() {
        for body in [LOGO_BLOCK, LOGO_ASCII] {
            let w = body[0].chars().count();
            for (i, row) in body.iter().enumerate() {
                assert_eq!(row.chars().count(), w, "row {i} width differs");
            }
        }
    }

    /// The ASCII fallback must stay pure ASCII (that's its whole point).
    #[test]
    fn ascii_logo_is_pure_ascii() {
        for row in LOGO_ASCII {
            assert!(row.is_ascii(), "non-ASCII char in ASCII logo: {row}");
        }
    }

    #[test]
    fn row_chunks_reassemble_the_row_and_group_runs() {
        let theme = ThemeState {
            theme: by_id("dark").unwrap(),
            tier: ColorTier::Rich,
        };
        for icons in [IconTier::Unicode, IconTier::Ascii] {
            for row in 0..logo_height(icons) as usize {
                let chunks = logo_row_chunks(&theme, icons, row);
                let joined: String = chunks.iter().map(|(t, _)| t.as_str()).collect();
                assert_eq!(joined, logo_body(icons)[row]);
            }
        }
        // ansi16 has 3 gradient stops → each row must collapse to at most 3 runs of
        // colour change… plus grouping means far fewer spans than chars.
        let theme16 = ThemeState {
            theme: by_id("dark").unwrap(),
            tier: ColorTier::Ansi16,
        };
        let chunks = logo_row_chunks(&theme16, IconTier::Unicode, 0);
        assert!(chunks.len() <= 3, "expected ≤3 runs, got {}", chunks.len());
    }
}
