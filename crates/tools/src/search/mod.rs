//! Search tools — finding things rather than changing them.
//! Three ways to locate something, matched to what the caller already knows:
//! | Tool | Given | Answers |
//! |---|---|---|
//! | [`ListDir`] | a directory | what is in it, and how it is laid out |
//! | [`Glob`] | a path shape | which paths match, most recently modified first |
//! | [`Grep`] | a piece of text | which lines contain it, and what those lines *are* |
//! They are grouped together because they share the question that actually decides their output:
//! **what does the caller not want to see**. `node_modules`, `target`, `.git`, whatever the
//! project's `.gitignore` lists. That policy lives once, in [`ignores`], instead of three times
//! with three slightly different lists.
//! All three are read-only, so the approval pipeline can allow them anywhere — including outside
//! the workspace, where reading is a perfectly ordinary request and only the *content* (a
//! credential path) decides whether to refuse.

pub mod glob;
pub mod grep;
pub(crate) mod ignores;
pub mod list_dir;

use std::sync::Arc;

use serde::Deserialize;

pub use glob::Glob;
pub use grep::Grep;
pub use list_dir::ListDir;

use crate::Tool;

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CaseMode {
    #[default]
    Smart,
    Sensitive,
    Insensitive,
}

impl CaseMode {
    pub(crate) fn is_sensitive(self, pattern: &str) -> bool {
        match self {
            Self::Smart => pattern.chars().any(char::is_uppercase),
            Self::Sensitive => true,
            Self::Insensitive => false,
        }
    }
}

pub fn all() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(ListDir), Arc::new(Glob), Arc::new(Grep)]
}
