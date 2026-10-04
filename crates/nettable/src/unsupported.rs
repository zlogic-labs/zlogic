//! Platforms with no cheap answer, saying so by answering nothing.
//!
//! Shelling out to `lsof` was rejected: it is not installed by default, its output format is not a
//! contract, and spawning it on a poll is the one cost this crate exists to avoid. An empty result
//! is the honest response, and callers are told to render it as "nothing found".

use std::collections::HashMap;

use crate::ListenSocket;

pub(super) fn listening_sockets() -> Vec<ListenSocket> {
    Vec::new()
}

pub(super) fn process_parents() -> HashMap<u32, u32> {
    HashMap::new()
}
