//! Terminal capability detection and downgrade policy.
//! This is intentionally centralized: rendering code consumes `TermCaps` through
//! `ThemeState` and `IconTier` instead of inspecting environment variables ad hoc.
//! One `known_terminal()` table feeds BOTH the colour tier and the DEC 2026 sync
//! gate — the two used to keep separate terminal lists and drifted (kitty counted
//! as modern for sync but not for colour, so kitty-over-ssh without COLORTERM fell
//! all the way to ansi16).

use crate::glyph::IconTier;
use crate::theme::ColorTier;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Background {
    Dark,
    Light,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalKind {
    AppleTerminal,
    ITerm2,
    Vscode,
    Windows,
    XtermLike,
}

pub fn detect_terminal_kind() -> TerminalKind {
    classify_terminal(
        &env_lower("TERM_PROGRAM"),
        &env_lower("TERM"),
        std::env::var_os("WT_SESSION").is_some(),
    )
}

fn classify_terminal(term_program: &str, term: &str, windows_terminal: bool) -> TerminalKind {
    if windows_terminal {
        return TerminalKind::Windows;
    }
    match term_program {
        "apple_terminal" => return TerminalKind::AppleTerminal,
        "iterm.app" => return TerminalKind::ITerm2,
        "vscode" => return TerminalKind::Vscode,
        _ => {}
    }
    if cfg!(windows) || term.contains("cygwin") || term.contains("msys") {
        TerminalKind::Windows
    } else {
        TerminalKind::XtermLike
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TermCaps {
    pub color_tier: ColorTier,
    pub icon_tier: IconTier,
    pub ambiguous_wide: bool,
    pub background: Background,
    pub legacy_windows_console: bool,
    /// Terminal supports DEC 2026 synchronized output. When false we send NO `?2026`
    /// bytes — unsupported terminals (Terminal.app, conhost, TTY)
    /// leak the intro as a stray `[`. Env heuristic at detect(); the startup DECRQM
    /// probe (`\x1b[?2026$p`, term/probe.rs) replaces it via `refine_with_probe`.
    pub supports_sync: bool,
}

impl TermCaps {
    pub fn detect() -> Self {
        let legacy_windows_console = detect_legacy_windows_console();
        let ambiguous_wide = detect_ambiguous_wide(legacy_windows_console);
        let color_tier = detect_color_tier(legacy_windows_console);
        let icon_tier = detect_icon_tier(legacy_windows_console, ambiguous_wide);
        let background = detect_background();
        let supports_sync = detect_supports_sync(legacy_windows_console);

        Self {
            color_tier,
            icon_tier,
            ambiguous_wide,
            background,
            legacy_windows_console,
            supports_sync,
        }
    }

    /// Fold in the startup probe results (term/probe.rs). Measured facts replace the
    /// env heuristics — but explicit user config (`ZLOGIC_AMBIGUOUS_WIDE`,
    /// `ZLOGIC_BACKGROUND`, `ZLOGIC_ICON_TIER`) and the legacy-console hard downgrade
    /// still win; the probe only overrides guesses like "Terminal.app → wide".
    pub fn refine_with_probe(&mut self, outcome: &crate::term::probe::ProbeOutcome) {
        if let Some(bg) = outcome.background {
            if std::env::var_os("ZLOGIC_BACKGROUND").is_none() {
                self.background = bg;
            }
        }
        if let Some(wide) = outcome.ambiguous_wide {
            if env_ambiguous_override().is_none() && !self.legacy_windows_console {
                // Windows: the DSR answer can come from the local conhost/conpty cursor
                // math (console font metrics) instead of the renderer the user actually
                // sees — Windows Terminal paints ambiguous chars narrow while its hidden
                // console may report wide. So only a NARROW measurement upgrades an
                // env-guessed terminal; a WIDE answer never overrides detection that
                // already landed on unicode. Unix probes are definitive in both
                // directions (the real terminal answers directly).
                let trustworthy = !cfg!(windows) || !wide || self.ambiguous_wide;
                if trustworthy {
                    self.ambiguous_wide = wide;
                    self.icon_tier = detect_icon_tier(self.legacy_windows_console, wide);
                }
            }
        }
        if let Some(sync) = outcome.supports_sync {
            // DECRQM measured the real answer — it REPLACES the known-terminal-table
            // guess in both directions (upgrades generic xterm-256color terminals
            // that do support 2026; disables it where the table was optimistic).
            if env_sync_override().is_none() && !self.legacy_windows_console {
                self.supports_sync = sync;
            }
        }
    }
}

/// Capability row for a terminal we can positively identify from the environment.
#[derive(Debug, Clone, Copy)]
struct KnownTerm {
    truecolor: bool,
    /// DEC 2026 synchronized output. Conservative: only true where current releases
    /// are known-good (sync-on when unsupported leaks a stray `[`; sync-off only
    /// costs mild tearing).
    sync: bool,
}

/// The single known-terminal table (colour AND sync decisions read this).
/// `None` = unidentified; callers fall back to TERM/COLORTERM heuristics.
fn known_terminal() -> Option<KnownTerm> {
    let good = KnownTerm {
        truecolor: true,
        sync: true,
    };
    // Env markers survive nested shells better than TERM. `WT_PROFILE_ID` sits
    // beside `WT_SESSION` because both are inherited by EVERY child spawned inside
    // Windows Terminal — including cmd.exe and anything it launches — so a plain
    // cmd inside WT is never misread as bare conhost (which would drag the icon
    // tier down to ASCII via the ambiguous-width downgrade).
    if std::env::var_os("WT_SESSION").is_some()
        || std::env::var_os("WT_PROFILE_ID").is_some()
        || std::env::var_os("WEZTERM_EXECUTABLE").is_some()
        || std::env::var_os("ALACRITTY_LOG").is_some()
        || std::env::var_os("KITTY_WINDOW_ID").is_some()
        || std::env::var_os("GHOSTTY_RESOURCES_DIR").is_some()
    {
        return Some(good);
    }
    match env_lower("TERM_PROGRAM").as_str() {
        "wezterm" | "vscode" | "iterm.app" | "ghostty" | "rio" => return Some(good),
        // mintty (Git Bash) does truecolor everywhere current; DEC 2026 only in
        // recent releases — stay conservative on sync.
        "mintty" => {
            return Some(KnownTerm {
                truecolor: true,
                sync: false,
            })
        }
        // macOS Terminal.app: identified, but 256-colour max and no DEC 2026.
        "apple_terminal" => {
            return Some(KnownTerm {
                truecolor: false,
                sync: false,
            })
        }
        _ => {}
    }
    // ConEmu / Cmder set ConEmuANSI=ON when VT processing is active.
    if env_lower("CONEMUANSI") == "on" {
        return Some(KnownTerm {
            truecolor: true,
            sync: false,
        });
    }
    let term = env_lower("TERM");
    if term.contains("kitty")
        || term.contains("alacritty")
        || term.contains("wezterm")
        || term.contains("foot")
        || term.contains("ghostty")
        || term.contains("contour")
    {
        return Some(good);
    }
    None
}

/// Explicit `ZLOGIC_SYNC` user override. Shared by the startup heuristic below and
/// `refine_with_probe` — explicit config must beat the DECRQM measurement too.
fn env_sync_override() -> Option<bool> {
    match env_lower("ZLOGIC_SYNC").as_str() {
        "1" | "true" | "on" | "yes" => Some(true),
        "0" | "false" | "off" | "no" => Some(false),
        _ => None,
    }
}

/// Whether to emit DEC 2026 synchronized-update sequences. **Conservative: unknown → OFF**.
fn detect_supports_sync(legacy_windows_console: bool) -> bool {
    if let Some(v) = env_sync_override() {
        return v;
    }
    if legacy_windows_console || windows_bare_conhost() {
        return false; // conhost (even with VT): no reliable DEC 2026
    }
    let term = env_lower("TERM");
    if term.is_empty() && !cfg!(windows) || term == "dumb" || term == "linux" {
        return false; // dumb / Linux text console (TTY)
    }
    if term.starts_with("screen") || term.starts_with("tmux") {
        return false; // screen / tmux — passthrough unknown; conservative off
    }
    known_terminal().map(|k| k.sync).unwrap_or(false) // unknown → conservative OFF
}

fn detect_color_tier(legacy_windows_console: bool) -> ColorTier {
    // NO_COLOR (https://no-color.org): emit no colour codes at all — spec-pure tier,
    // not "16 colours". Hierarchy survives via modifiers (theme/mod.rs `style`).
    if std::env::var_os("NO_COLOR").is_some() {
        return ColorTier::None;
    }
    // Explicit override outranks every heuristic (`auto|rich|ansi256|ansi16`).
    match env_lower("ZLOGIC_COLOR_TIER").as_str() {
        "rich" | "truecolor" | "24bit" => return ColorTier::Rich,
        "ansi256" | "256" => return ColorTier::Ansi256,
        "ansi16" | "16" => return ColorTier::Ansi16,
        "none" | "off" => return ColorTier::None,
        _ => {}
    }
    if legacy_windows_console {
        return ColorTier::Ansi16;
    }

    let colorterm = env_lower("COLORTERM");
    if colorterm.contains("truecolor") || colorterm.contains("24bit") {
        return ColorTier::Rich;
    }
    let term = env_lower("TERM");
    if term.contains("truecolor") || term.contains("24bit") || term.contains("direct") {
        return ColorTier::Rich;
    }

    if let Some(k) = known_terminal() {
        if k.truecolor {
            return ColorTier::Rich;
        }
        // Identified but not truecolor (Terminal.app): fall through to TERM.
    }

    // Bare conhost that accepted VT enablement: 24-bit SGR works (Win10 1703+),
    // and Rich beats the ugly legacy 16-colour palette (#000080 blue et al).
    if windows_bare_conhost() {
        return ColorTier::Rich;
    }

    if term.contains("256color") {
        return ColorTier::Ansi256;
    }
    ColorTier::Ansi16
}

fn detect_icon_tier(legacy_windows_console: bool, ambiguous_wide: bool) -> IconTier {
    // Explicit user config outranks the auto-downgrade (it used to be unreachable on
    // Terminal.app because the ambiguous-wide branch returned first).
    match env_lower("ZLOGIC_ICON_TIER").as_str() {
        "ascii" => return IconTier::Ascii,
        "nerd" | "nerdfont" | "nf" => return IconTier::Nerd,
        "unicode" => return IconTier::Unicode,
        _ => {}
    }
    if legacy_windows_console || ambiguous_wide {
        return IconTier::Ascii;
    }
    IconTier::Unicode
}

fn detect_background() -> Background {
    if let Some(bg) = std::env::var_os("ZLOGIC_BACKGROUND").and_then(|s| s.into_string().ok()) {
        return match bg.to_ascii_lowercase().as_str() {
            "dark" => Background::Dark,
            "light" => Background::Light,
            _ => Background::Unknown,
        };
    }

    // COLORFGBG is usually `fg;bg`; ANSI bg 0-6 is dark, 7+ is light.
    // The OSC 11 probe (term/bgprobe.rs) refines this at startup when it answers.
    if let Some(v) = std::env::var_os("COLORFGBG").and_then(|s| s.into_string().ok()) {
        if let Some(last) = v.split(';').next_back().and_then(|s| s.parse::<u8>().ok()) {
            return if last < 7 {
                Background::Dark
            } else {
                Background::Light
            };
        }
    }

    Background::Unknown
}

fn env_ambiguous_override() -> Option<bool> {
    match env_lower("ZLOGIC_AMBIGUOUS_WIDE").as_str() {
        "1" | "true" | "yes" => Some(true),
        "0" | "false" | "no" => Some(false),
        _ => None,
    }
}

/// Pre-probe heuristic only — `refine_with_probe` replaces the guesses below with the
/// measured CSI 6n answer on unix (default Terminal.app measures NARROW and gets the
/// block glyphs; only an actually-wide config keeps the ASCII downgrade).
fn detect_ambiguous_wide(legacy_windows_console: bool) -> bool {
    if legacy_windows_console {
        return true;
    }

    if let Some(v) = env_ambiguous_override() {
        return v;
    }

    // Bare conhost keeps the GDI renderer even with VT colours enabled — glyph
    // fallback is weak and CJK codepages render ambiguous-width chars wide, so stay
    // conservative (this also cascades to ASCII icons). Not probed on Windows.
    if windows_bare_conhost() {
        return true;
    }

    // macOS Terminal.app guess — CJK profiles often enable "ambiguous are wide".
    // Overridden by the probe when it answers.
    env_lower("TERM_PROGRAM") == "apple_terminal"
}

/// Bare conhost (cmd.exe / legacy PowerShell window — NOT Windows Terminal, mintty,
/// ConEmu, VS Code, …) that DID accept VT processing. Colour-capable (24-bit SGR),
/// but no DEC 2026 and the legacy renderer — so: Rich colours, sync off, no warning.
fn windows_bare_conhost() -> bool {
    if !cfg!(windows) || known_terminal().is_some() {
        return false;
    }
    // Wrappers that speak VT set TERM (msys, cygwin, ssh clients); bare cmd doesn't.
    if std::env::var_os("TERM").is_some() || std::env::var_os("TERM_PROGRAM").is_some() {
        return false;
    }
    vt_enabled_on_windows()
}

/// True legacy console: bare conhost where VT processing can't even be enabled
/// (pre-Win10-1511). Gets the harsh downgrade (ansi16 + ASCII icons + warning).
fn detect_legacy_windows_console() -> bool {
    if !cfg!(windows) || known_terminal().is_some() {
        return false;
    }
    if std::env::var_os("TERM").is_some() || std::env::var_os("TERM_PROGRAM").is_some() {
        return false;
    }
    !vt_enabled_on_windows()
}

/// Probe (and enable) ENABLE_VIRTUAL_TERMINAL_PROCESSING via crossterm. Success means
/// conhost accepts VT sequences — which on any Win10 1703+ includes 24-bit SGR.
#[cfg(windows)]
fn vt_enabled_on_windows() -> bool {
    crossterm::ansi_support::supports_ansi()
}

#[cfg(not(windows))]
fn vt_enabled_on_windows() -> bool {
    false
}

fn env_lower(key: &str) -> String {
    std::env::var_os(key)
        .and_then(|s| s.into_string().ok())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    //! `refine_with_probe` precedence only. The env-reading detect fns are not
    //! testable without racy `set_var`, so these tests construct `TermCaps`
    //! directly and assume the `ZLOGIC_*` overrides are unset in the test
    //! environment — if one IS set (dev shell), the test skips itself.

    use super::*;
    use crate::term::probe::ProbeOutcome;

    #[test]
    fn terminal_kind_distinguishes_platform_specific_mouse_shortcuts() {
        assert_eq!(
            classify_terminal("apple_terminal", "xterm-256color", false),
            TerminalKind::AppleTerminal
        );
        assert_eq!(
            classify_terminal("iterm.app", "xterm-256color", false),
            TerminalKind::ITerm2
        );
        assert_eq!(
            classify_terminal("vscode", "xterm-256color", false),
            TerminalKind::Vscode
        );
        assert_eq!(
            classify_terminal("", "xterm-256color", true),
            TerminalKind::Windows
        );
        assert_eq!(
            classify_terminal("wezterm", "xterm-256color", false),
            if cfg!(windows) {
                // On Windows every non-apple/iterm/vscode host collapses to
                // TerminalKind::Windows for the mouse-hint wording.
                TerminalKind::Windows
            } else {
                TerminalKind::XtermLike
            }
        );
    }

    /// The refine paths under test consult these env overrides; a set override
    /// legitimately changes the outcome, so skip rather than fail.
    fn env_overrides_present() -> bool {
        [
            "ZLOGIC_BACKGROUND",
            "ZLOGIC_AMBIGUOUS_WIDE",
            "ZLOGIC_SYNC",
            "ZLOGIC_ICON_TIER",
        ]
        .iter()
        .any(|key| std::env::var_os(key).is_some())
    }

    fn base_caps() -> TermCaps {
        TermCaps {
            color_tier: ColorTier::Rich,
            icon_tier: IconTier::Unicode,
            ambiguous_wide: false,
            background: Background::Unknown,
            legacy_windows_console: false,
            supports_sync: false,
        }
    }

    #[test]
    fn empty_probe_outcome_changes_nothing() {
        if env_overrides_present() {
            return;
        }
        let mut caps = base_caps();
        caps.refine_with_probe(&ProbeOutcome::default());
        assert_eq!(caps, base_caps());
    }

    #[test]
    fn probe_background_applies() {
        if env_overrides_present() {
            return;
        }
        let mut caps = base_caps();
        caps.refine_with_probe(&ProbeOutcome {
            background: Some(Background::Light),
            ..Default::default()
        });
        assert_eq!(caps.background, Background::Light);

        caps.refine_with_probe(&ProbeOutcome {
            background: Some(Background::Dark),
            ..Default::default()
        });
        assert_eq!(caps.background, Background::Dark);
    }

    #[test]
    #[cfg(unix)] // definitive in both directions only where the real terminal answers
    fn probe_ambiguous_wide_true_downgrades_icons_to_ascii() {
        if env_overrides_present() {
            return;
        }
        let mut caps = base_caps();
        caps.refine_with_probe(&ProbeOutcome {
            ambiguous_wide: Some(true),
            ..Default::default()
        });
        assert!(caps.ambiguous_wide);
        assert_eq!(caps.icon_tier, IconTier::Ascii);
    }

    #[test]
    #[cfg(windows)] // conhost/conpty cursor math may report wide without the renderer
    fn probe_ambiguous_wide_is_ignored_on_windows_when_heuristic_said_narrow() {
        // A "wide" DSR answer must never downgrade a terminal the env heuristic
        // already identified as modern (Windows Terminal renders ambiguous narrow).
        let mut caps = base_caps();
        caps.refine_with_probe(&ProbeOutcome {
            ambiguous_wide: Some(true),
            ..Default::default()
        });
        assert!(!caps.ambiguous_wide);
        assert_eq!(caps.icon_tier, IconTier::Unicode);
    }

    #[test]
    fn probe_ambiguous_narrow_restores_unicode_icons() {
        if env_overrides_present() {
            return;
        }
        // Env heuristic guessed wide (e.g. Terminal.app) → probe measured narrow.
        let mut caps = TermCaps {
            ambiguous_wide: true,
            icon_tier: IconTier::Ascii,
            ..base_caps()
        };
        caps.refine_with_probe(&ProbeOutcome {
            ambiguous_wide: Some(false),
            ..Default::default()
        });
        assert!(!caps.ambiguous_wide);
        assert_eq!(caps.icon_tier, IconTier::Unicode);
    }

    #[test]
    fn probe_sync_replaces_heuristic_in_both_directions() {
        if env_overrides_present() {
            return;
        }
        let mut caps = base_caps(); // heuristic said no sync
        caps.refine_with_probe(&ProbeOutcome {
            supports_sync: Some(true),
            ..Default::default()
        });
        assert!(caps.supports_sync, "DECRQM upgrade applies");

        caps.refine_with_probe(&ProbeOutcome {
            supports_sync: Some(false),
            ..Default::default()
        });
        assert!(!caps.supports_sync, "DECRQM downgrade applies");
    }

    #[test]
    fn legacy_console_blocks_ambiguous_and_sync_but_not_background() {
        if env_overrides_present() {
            return;
        }
        let mut caps = TermCaps {
            legacy_windows_console: true,
            ambiguous_wide: true,
            icon_tier: IconTier::Ascii,
            supports_sync: false,
            ..base_caps()
        };
        caps.refine_with_probe(&ProbeOutcome {
            background: Some(Background::Light),
            ambiguous_wide: Some(false),
            supports_sync: Some(true),
        });
        // Hard downgrade wins over the probe measurements…
        assert!(caps.ambiguous_wide, "legacy console keeps ambiguous-wide");
        assert_eq!(
            caps.icon_tier,
            IconTier::Ascii,
            "legacy console keeps ASCII"
        );
        assert!(!caps.supports_sync, "legacy console never gets DEC 2026");
        // …but the background answer is still folded in.
        assert_eq!(caps.background, Background::Light);
    }

    #[test]
    fn none_fields_leave_prior_refinements_alone() {
        if env_overrides_present() {
            return;
        }
        let mut caps = base_caps();
        caps.refine_with_probe(&ProbeOutcome {
            background: Some(Background::Dark),
            ambiguous_wide: Some(true),
            supports_sync: Some(true),
        });
        let refined = caps.clone();
        // A later probe with all-None (e.g. timeout) must not undo anything.
        caps.refine_with_probe(&ProbeOutcome::default());
        assert_eq!(caps, refined);
    }
}
