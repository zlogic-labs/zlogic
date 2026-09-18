//! Semantic colour tokens — the ONLY way components ask for colour.
//! Grep the codebase and you should find no `Color::Rgb(...)` outside `themes/*.rs`.

/// The semantic tokens. Components pass one of these to `Theme::color`; they never
/// name a concrete colour. `Chart(i)` cycles the chart-series palette. (Content-type
/// tokens file/dir/paste/image were removed — use the role tokens instead.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sem {
    // structure
    Accent,
    AccentSoft,
    Border,
    Muted,
    // status
    Success,
    Error,
    Warning,
    Info,
    // roles
    Thinking,
    ToolRunning,
    /// The i-th chart-series colour (cycles the palette).
    Chart(usize),
}
