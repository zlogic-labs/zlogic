//! Network tools — reaching outside this machine.
//! The domain is worth separating for one reason: everything here can fail in ways nothing else
//! can. A file either exists or does not; a URL can hang, redirect, return HTML where JSON was
//! promised, or send a hundred megabytes. So these tools all carry a deadline, a size cap and a
//! content-type check, and none of them is allowed to decide how much memory the process uses.

//! The two are a pair and the split is deliberate: `web_search` finds *which* pages matter and
//! returns excerpts, `web_fetch` reads one page whole. Collapsing them would mean one tool that
//! either searches when asked to read or reads when asked to search.

pub mod web_fetch;
pub mod web_search;

use std::sync::Arc;

pub use web_fetch::WebFetch;
pub use web_search::{SearchKeySource, SearchProvider, WebSearch, WebSearchSettings};

use crate::Tool;

pub fn all() -> Vec<Arc<dyn Tool>> {
    // `WebSearch::default` reads keys from the environment, which is the zero-configuration path.
    // A deployment with configured keys (keyring, its own endpoint) registers its own instance
    // over this one — see `ToolRegistry::add`.
    vec![Arc::new(WebFetch), Arc::new(WebSearch::default())]
}
