//! `/` command catalog: built-in commands + dynamically-loaded skills.
//! Built-ins are a static Rust table (they toggle overlays / modes); skills come from
//! core's runtime skill catalog (the real catalog op is not wired yet — EngineSession
//! returns the built-ins).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CommandKind {
    /// Rust-side action (open overlay, switch mode). Never leaves the frontend.
    Builtin,
    /// core skill — submitting it produces a command `Part`.
    Skill,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSpec {
    pub name: String,
    pub description: String,
    pub kind: CommandKind,
}

impl CommandSpec {
    pub fn builtin(name: &str, description: &str) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            kind: CommandKind::Builtin,
        }
    }
    pub fn skill(name: &str, description: &str) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            kind: CommandKind::Skill,
        }
    }
}

/// Frontend built-in commands. These trigger overlays / mode switches,
/// not core turns. Kept here so `command_catalog()` can merge them with core skills.
pub fn builtins() -> Vec<CommandSpec> {
    vec![
        CommandSpec::builtin("help", "Show help"),
        CommandSpec::builtin("model", "Switch / manage models"),
        CommandSpec::builtin("theme", "Switch theme"),
        CommandSpec::builtin("session", "Open session history"),
        CommandSpec::builtin("replay", "Review a past turn's rounds"),
        CommandSpec::builtin("workspace", "List / switch workspaces"),
        CommandSpec::builtin("new", "New session"),
        CommandSpec::builtin("compact", "Summarise older context now"),
        CommandSpec::builtin("stats", "Usage statistics"),
        CommandSpec::builtin("info", "Current session information"),
        CommandSpec::builtin("plan", "Toggle Plan mode"),
        CommandSpec::builtin("approval", "Switch approval mode"),
        CommandSpec::builtin("lang", "Switch UI language"),
        CommandSpec::builtin("view", "Switch live view detail (minimal/normal/verbose)"),
    ]
}
