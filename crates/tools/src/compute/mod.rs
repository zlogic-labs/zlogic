//! Pure computation tools.
//! These tools touch neither the workspace nor the network. They exist instead of granting a chat
//! workspace a shell merely to learn the time or calculate a statistic: a bounded, typed operation
//! is both safer and easier for the model to call correctly than arbitrary code execution.

pub mod time;

use std::sync::Arc;

pub use time::Time;

use crate::Tool;

pub fn all() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(Time)]
}
