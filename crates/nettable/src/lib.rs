//! What the operating system thinks is listening, and which processes are whose children.
//!
//! Nothing else in this workspace can answer either question. `std` knows how to *make* a socket
//! and how to read one it already owns; it has no way to ask the kernel "which process is
//! listening on 5173, and is it under the pid I started?". A dev server started through
//! `bash -c "npm run dev"` binds its port several generations below the process the shell tool
//! actually spawned, so the listen table is the only place the real answer is written down.
//!
//! Reading it is cheap — both calls are single syscalls' worth of kernel work — which is what
//! makes it safe to call on a poll. Deliberately narrow in two ways:
//!
//! * **Listen state only.** Established and TIME_WAIT sockets are somebody else's history.
//! * **No addressing detail beyond the port.** A loopback server is reachable at `localhost:port`
//!   whatever it bound, and reporting `0.0.0.0` versus `127.0.0.1` as a difference would be
//!   telling the caller something it cannot act on.
//!
//! Platform coverage is uneven, and [`listening_sockets`] returning nothing is a supported answer
//! rather than a failure. Windows and Linux are implemented; macOS has no `/proc` and no
//! documented safe enumeration path, so it reports nothing. A caller that renders "no port" must
//! therefore mean "nothing was found", never "the lookup failed".

use std::collections::HashMap;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as platform;

#[cfg(target_os = "windows")]
mod win;
#[cfg(target_os = "windows")]
use win as platform;

/// Everywhere else — macOS today. There is no `/proc` and no documented enumeration call, so this
/// reports nothing. See the module docs: an empty result means "nothing was found", which on this
/// platform is really "this platform cannot answer".
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
mod unsupported;
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
use unsupported as platform;

/// A socket in the listen state, attributed to the process that owns it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ListenSocket {
    pub pid: u32,
    pub port: u16,
}

/// Every socket currently in the listen state, across IPv4 and IPv6.
///
/// Duplicate `(pid, port)` pairs are collapsed: a server bound to both families on the same port
/// is one service, and reporting it twice would only make the caller deduplicate.
pub fn listening_sockets() -> Vec<ListenSocket> {
    let mut found = platform::listening_sockets();
    found.sort_unstable();
    found.dedup();
    found
}

/// The live process tree as `pid -> parent pid`.
///
/// A pid the caller does not recognise is not an error: processes come and go while this is being
/// read, and a row whose parent had already exited is simply a root. Callers should treat the map
/// as a snapshot, not a guarantee.
pub fn process_parents() -> HashMap<u32, u32> {
    platform::process_parents()
}

/// Every process below each root, mapped back to the root it descends from.
///
/// A server is almost never the process that was spawned — `bash -c "npm run dev"` puts at least a
/// shell, a package runner and the runtime itself in between — so a lookup keyed on the root pid
/// alone finds nothing. Walking down from the roots is what makes it work.
///
/// Each process is attributed to exactly one root, because the process tree is a forest: a pid
/// descends from one parent and therefore from one root. Overlapping roots are still handled, in
/// the only way a map can express it — the first root in the list wins, so callers that care
/// should pass roots in the order they want to claim.
///
/// Walking is iterative and marks as it goes, so a tree that has somehow acquired a cycle
/// terminates instead of hanging a poll.
pub fn descendant_roots(parents: &HashMap<u32, u32>, roots: &[u32]) -> HashMap<u32, u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (&pid, &parent) in parents {
        children.entry(parent).or_default().push(pid);
    }
    let mut out: HashMap<u32, u32> = HashMap::with_capacity(roots.len());
    let mut stack: Vec<(u32, u32)> = roots.iter().map(|&root| (root, root)).collect();
    while let Some((pid, root)) = stack.pop() {
        // `entry` refuses an existing key, which is also what makes an overlapping root lose.
        if out.contains_key(&pid) {
            continue;
        }
        out.insert(pid, root);
        if let Some(kids) = children.get(&pid) {
            stack.extend(kids.iter().map(|&kid| (kid, root)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The point of the whole module: a listener several generations down is still found.
    #[test]
    fn a_listener_is_found_under_a_root_that_did_not_bind_it() {
        let parents = HashMap::from([
            (100, 1), // the wrapper the shell tool spawned
            (200, 100),
            (300, 200),
        ]);
        let tree = descendant_roots(&parents, &[100]);
        assert_eq!(tree.get(&300), Some(&100), "the grandchild is in the tree");
        assert_eq!(tree.get(&100), Some(&100), "the root is its own");
        assert!(!tree.contains_key(&999), "a stranger is not");
    }

    /// Two services in one conversation: each port must name the run that owns it, not "a run".
    #[test]
    fn sibling_roots_are_kept_apart() {
        let parents = HashMap::from([(200, 100), (201, 101), (300, 200), (301, 201)]);
        let tree = descendant_roots(&parents, &[100, 101]);
        assert_eq!(tree.get(&300), Some(&100));
        assert_eq!(tree.get(&301), Some(&101));
    }

    /// A process tree is a forest, and a cycle would hang the walk.
    #[test]
    fn unrelated_branches_stay_out_and_a_cycle_terminates() {
        let parents = HashMap::from([(100, 1), (200, 100), (7, 7), (8, 7)]);
        assert_eq!(descendant_roots(&parents, &[100]).len(), 2);
        assert_eq!(descendant_roots(&parents, &[7]).len(), 2, "the self-parent root");
    }

    #[test]
    fn an_empty_root_list_finds_nothing() {
        assert!(descendant_roots(&HashMap::from([(2, 1)]), &[]).is_empty());
    }
}
