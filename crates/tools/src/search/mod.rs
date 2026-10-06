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
mod ignores;
pub mod list_dir;

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use zlogic_protocol::settings::ToolsConfig;

pub use glob::Glob;
pub use grep::Grep;
pub use list_dir::ListDir;

use crate::{Tool, ToolRegistry};

/// The wall-clock ceiling the three search tools apply to one call.
///
/// Public because it is configuration, not an internal detail: the engine reads
/// `tools.search_timeout_secs` from the global config and from a workspace's
/// `.zlogic/settings.yaml` and hands the merged result back through
/// [`SearchTools::with_budgets`]. Shaped like [`crate::ShellBudgets`] for the same reason — the
/// tool never has to know where configuration came from, and a test can reach the timeout path
/// with a millisecond budget.
#[derive(Debug, Clone, Copy)]
pub struct SearchBudgets {
    /// 0 = no ceiling.
    pub timeout: Duration,
}

impl Default for SearchBudgets {
    fn default() -> Self {
        Self::from(&ToolsConfig::default())
    }
}

impl From<&ToolsConfig> for SearchBudgets {
    fn from(cfg: &ToolsConfig) -> Self {
        Self {
            timeout: Duration::from_secs(cfg.search_timeout_secs),
        }
    }
}

impl SearchBudgets {
    /// The instant this call stops, or `None` when the budget is off.
    ///
    /// Zero is off rather than "stop immediately", matching every other timeout in the config — and
    /// `Instant::now().checked_add(ZERO)` would otherwise return a deadline already in the past.
    /// `checked_add` rather than `+` for the same reason as above: a budget large enough to
    /// overflow the monotonic clock is a configuration mistake, and "no ceiling" is the reading that
    /// cannot hang.
    pub fn deadline(&self) -> Option<std::time::Instant> {
        if self.timeout.is_zero() {
            return None;
        }
        std::time::Instant::now().checked_add(self.timeout)
    }
}

/// The three walking tools as one value, so a caller can rebudget all of them together.
///
/// They are bundled because they share the failure this budget exists for: a walk over a tree with
/// no cap on entries visited and no cap on time. `list_dir` is bounded by depth and count and
/// `glob` by a visited-entry ceiling, but neither bounds the *cost* of one entry, and `grep` has
/// neither — a pattern that matches nothing reads every file in the repository.
#[derive(Debug, Clone)]
pub struct SearchTools {
    pub list_dir: ListDir,
    pub glob: Glob,
    pub grep: Grep,
}

impl SearchTools {
    pub fn with_budgets(budgets: SearchBudgets) -> Self {
        Self {
            list_dir: ListDir::with_budgets(budgets),
            glob: Glob::with_budgets(budgets),
            grep: Grep::with_budgets(budgets),
        }
    }

    /// Registers all three under their own names, replacing whatever was there.
    ///
    /// `add` rather than a constructor that builds a registry: the registry is also where a
    /// workspace's override lands, and the built-in source has already put a default-budget copy
    /// of each tool in it.
    pub fn register(&self, registry: &mut ToolRegistry) {
        registry.add(Arc::new(self.list_dir.clone()));
        registry.add(Arc::new(self.glob.clone()));
        registry.add(Arc::new(self.grep.clone()));
    }
}

impl Default for SearchTools {
    fn default() -> Self {
        Self::with_budgets(SearchBudgets::default())
    }
}

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
    let t = SearchTools::default();
    vec![Arc::new(t.list_dir), Arc::new(t.glob), Arc::new(t.grep)]
}
