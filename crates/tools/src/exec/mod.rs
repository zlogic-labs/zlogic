//! Execution tools — running someone else's code.
//! One tool, and the domain exists because what makes it distinctive is not "it runs a process"
//! but everything that follows from running one: a child that must be reachable by a signal, a
//! deadline, two pipes that deadlock if they are not drained, and an environment that has to be
//! filtered before it is handed over. None of that is shared with the file or search tools, and
//! all of it will be shared with whatever runs a process next.

pub mod shell;

use std::sync::Arc;

pub use shell::{Shell, ShellDialect, ShellPreference};

use crate::Tool;

pub fn all() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(Shell::default())]
}
