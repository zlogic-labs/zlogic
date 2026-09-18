//! Semantic glyph registry, three tiers. Components reference a semantic
//! name, never a raw char — the same discipline as theme tokens.
//! Unicode + ascii ship today. Nerd Font (`icon_set: nerd`) comes later; it needs the
//! `render_width` override (a width caveat) so it falls back to unicode here.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconTier {
    Unicode,
    Nerd,
    Ascii,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Glyph {
    ModeNormal,
    ModeGod,
    ModePlan,
    Prompt,
    Ok,
    Fail,
    Thinking,
    Session,
    Branch,
    Tool,
    Subagent,
    Compaction,
    TokensIn,
    TokensOut,
    Image,
    Paste,
    Rail,
    Quote,
    Steering,
    Notice,
    Permission,
    ResultBranch,
    Blocked,
    Vision,
    Dir,
    File,
    Round,
    ProgressFull,
    ProgressEmpty,
    RuleHorizontal,
    RuleVertical,
    HintBullet,
    ListBullet,
    SelectionInactive,
    Truncation,
    ChartSeries,
}

impl Glyph {
    /// (unicode, ascii). Nerd Font reuses unicode for now.
    fn pair(self) -> (&'static str, &'static str) {
        match self {
            Glyph::ModeNormal => ("●", "*"),
            Glyph::ModeGod => ("⚡", "!"),
            Glyph::ModePlan => ("▲", "^"),
            Glyph::Prompt => ("›", ">"),
            Glyph::Ok => ("✓", "+"),
            Glyph::Fail => ("✗", "x"),
            Glyph::Thinking => ("✦", "*"),
            Glyph::Session => ("◇", "#"),
            Glyph::Branch => ("⎇", "br"),
            Glyph::Tool => ("→", ">"),
            Glyph::Subagent => ("◆", "+"),
            Glyph::Compaction => ("⊟", "#"),
            Glyph::Round => ("◆", "+"),
            Glyph::ProgressFull => ("█", "#"),
            Glyph::ProgressEmpty => ("░", "-"),
            Glyph::RuleHorizontal => ("─", "-"),
            Glyph::RuleVertical => ("│", "|"),
            Glyph::HintBullet => ("•", "."),
            Glyph::ListBullet => ("•", "-"),
            Glyph::SelectionInactive => ("·", "."),
            Glyph::Truncation => ("⋯⋯", "..."),
            Glyph::ChartSeries => ("■", "#"),
            Glyph::TokensIn => ("↑", "^"),
            Glyph::TokensOut => ("↓", "v"),
            Glyph::Image => ("▣", "[img]"),
            Glyph::Paste => ("▤", "[txt]"),
            Glyph::Rail => ("▎", "|"),
            Glyph::Quote => ("▏", "|"),
            Glyph::Steering => ("↪", "->"),
            Glyph::Notice => ("⚑", "!"),
            Glyph::Permission => ("▲", "!"),
            Glyph::ResultBranch => ("⎿", "`"),
            Glyph::Blocked => ("⊘", "!"),
            Glyph::Vision => ("◉", "v"),
            Glyph::Dir => ("▸", ">"),
            Glyph::File => ("▪", "-"),
        }
    }

    pub fn render(self, tier: IconTier) -> &'static str {
        let (uni, ascii) = self.pair();
        match tier {
            IconTier::Ascii => ascii,
            IconTier::Unicode | IconTier::Nerd => uni,
        }
    }
}

pub fn spinner_frame(tier: IconTier, frame: usize) -> &'static str {
    const UNICODE: [&str; 4] = ["◐", "◓", "◑", "◒"];
    const ASCII: [&str; 4] = ["-", "\\", "|", "/"];
    let frames = match tier {
        IconTier::Ascii => &ASCII,
        IconTier::Unicode | IconTier::Nerd => &UNICODE,
    };
    frames[frame % frames.len()]
}

/// Braille sweep spinner: dots sweeping around the 2×4 braille cell — the classic
/// CLI braille spinner (⠋ → ⠙ → ⠹ → ⠸ → ⠼ → ⠾ → ⠶ → ⠦ → ⠧ → ⠇ → ⠏).
/// ASCII falls back to the plain barber-pole.
pub fn braille_spinner_frame(tier: IconTier, frame: usize) -> &'static str {
    const UNICODE: [&str; 11] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠾", "⠶", "⠦", "⠧", "⠇", "⠏"];
    const ASCII: [&str; 4] = ["-", "\\", "|", "/"];
    let frames: &[&str] = match tier {
        IconTier::Ascii => &ASCII,
        IconTier::Unicode | IconTier::Nerd => &UNICODE,
    };
    frames[frame % frames.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_glyphs_have_ascii_fallbacks() {
        assert_eq!(Glyph::ModeNormal.render(IconTier::Ascii), "*");
        assert_eq!(Glyph::ModeGod.render(IconTier::Ascii), "!");
        assert_eq!(Glyph::ModePlan.render(IconTier::Ascii), "^");
        assert_eq!(Glyph::Prompt.render(IconTier::Ascii), ">");
        assert_eq!(Glyph::Thinking.render(IconTier::Unicode), "✦");
        assert_eq!(Glyph::Thinking.render(IconTier::Ascii), "*");
        assert_eq!(Glyph::Tool.render(IconTier::Unicode), "→");
        assert_eq!(Glyph::Tool.render(IconTier::Ascii), ">");
        assert_eq!(Glyph::Round.render(IconTier::Unicode), "◆");
        assert_eq!(Glyph::Ok.render(IconTier::Unicode), "✓");
        assert_eq!(Glyph::ProgressFull.render(IconTier::Ascii), "#");
        assert_eq!(Glyph::ProgressEmpty.render(IconTier::Unicode), "░");
        assert_eq!(Glyph::RuleHorizontal.render(IconTier::Ascii), "-");
        assert_eq!(Glyph::RuleVertical.render(IconTier::Unicode), "│");
        assert_eq!(Glyph::HintBullet.render(IconTier::Ascii), ".");
        assert_eq!(spinner_frame(IconTier::Ascii, 1), "\\");
        assert_eq!(braille_spinner_frame(IconTier::Ascii, 1), "\\");
        assert_eq!(braille_spinner_frame(IconTier::Unicode, 0), "⠋");
        assert_eq!(braille_spinner_frame(IconTier::Unicode, 9), "⠇");
        assert_eq!(braille_spinner_frame(IconTier::Unicode, 10), "⠏");
        assert_eq!(braille_spinner_frame(IconTier::Unicode, 11), "⠋"); // wraps
    }
}
