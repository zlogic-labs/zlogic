//! The Linux half, read out of `/proc`.
//!
//! Two files answer the two questions. `/proc/net/tcp{,6}` lists every socket with its state and
//! the inode that owns it; `/proc/<pid>/fd/` is the only place a socket inode is tied back to a
//! process. Parentage is one line per process in `/proc/<pid>/stat`.
//!
//! The inode walk touches every file descriptor on the machine, so it is bounded two ways: it
//! stops as soon as every listening socket has an owner, and it gives up on a process whose `fd`
//! directory it cannot open. A kernel thread has no `fd` directory at all, and treating that as an
//! error would be wrong — those processes hold no sockets by definition.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use crate::ListenSocket;

/// `TCP_LISTEN` in the `st` column.
const LISTEN: &str = "0A";

pub(super) fn listening_sockets() -> Vec<ListenSocket> {
    let inodes = listen_inodes();
    if inodes.is_empty() {
        return Vec::new();
    }
    let owners = inode_owners(&inodes);
    inodes
        .into_iter()
        .filter_map(|(inode, port)| owners.get(&inode).map(|&pid| ListenSocket { pid, port }))
        .collect()
}

pub(super) fn process_parents() -> HashMap<u32, u32> {
    let mut out = HashMap::new();
    for entry in fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = pid_of(&entry.file_name()) else {
            continue;
        };
        if let Some(parent) = parent_of(pid) {
            out.insert(pid, parent);
        }
    }
    out
}

fn pid_of(name: &std::ffi::OsStr) -> Option<u32> {
    name.to_str()?.parse().ok()
}

/// Listening sockets as `(socket inode, port)`, from both address families.
fn listen_inodes() -> HashMap<u64, u16> {
    let mut out = HashMap::new();
    for file in ["/proc/net/tcp", "/proc/net/tcp6"] {
        for line in fs::read_to_string(file).unwrap_or_default().lines().skip(1) {
            let Some((inode, port)) = parse_listen_line(line) else {
                continue;
            };
            out.entry(inode).or_insert(port);
        }
    }
    out
}

/// One row of `/proc/net/tcp`: `sl local rem st … uid timeout inode`.
///
/// The local address is `addr:port` with the port in host order already, so no byte swapping is
/// needed here — that is a Windows-only quirk of the API.
fn parse_listen_line(line: &str) -> Option<(u64, u16)> {
    let mut fields = line.split_whitespace();
    let _sl = fields.next()?;
    let local = fields.next()?;
    let _remote = fields.next()?;
    if fields.next()? != LISTEN {
        return None;
    }
    let port = local.rsplit_once(':')?.1.parse::<u16>().ok()?;
    let inode = fields.nth(5)?.parse::<u64>().ok()?;
    Some((inode, port))
}

/// The ppid field of `/proc/<pid>/stat`.
///
/// Splitting the line on whitespace does not work: the second field is the executable name in
/// parentheses and may itself contain spaces and parentheses. Everything after the final `)` is
/// positionally stable, which is what this reads.
fn parent_of(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

fn inode_owners(wanted: &HashSet<u64>) -> HashMap<u64, u32> {
    let mut remaining = wanted.clone();
    let mut out = HashMap::new();
    for entry in fs::read_dir("/proc").into_iter().flatten().flatten() {
        if remaining.is_empty() {
            break;
        }
        let Some(pid) = pid_of(&entry.file_name()) else {
            continue;
        };
        let Ok(fds) = fs::read_dir(Path::new("/proc").join(pid.to_string()).join("fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            // The link is the only place the inode appears; a socket shows up as `socket:[N]`.
            let Ok(target) = fs::read_link(fd.path()) else {
                continue;
            };
            let Some(text) = target.to_str() else {
                continue;
            };
            let Some(inode) = text
                .strip_prefix("socket:[")
                .and_then(|rest| rest.strip_suffix(']'))
                .and_then(|digits| digits.parse::<u64>().ok())
            else {
                continue;
            };
            if remaining.remove(&inode) {
                out.insert(inode, pid);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kernel's own documentation of a listening socket, verbatim apart from the header.
    #[test]
    fn a_listening_row_yields_its_port_and_inode() {
        let row = "   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000 \
                   0        0 12345 1 0000000000000000 100 0 0 10 0";
        assert_eq!(parse_listen_line(row), Some((12345, 8080)));
    }

    /// An established or closing socket is somebody else's history, not a service.
    #[test]
    fn other_states_are_skipped() {
        let established = "   1: 0100007F:1F90 0100007F:BEEF 01 00000000:00000000 00:00000000 \
                            00000000     0        0 12346 1";
        assert_eq!(parse_listen_line(established), None);
    }

    #[test]
    fn a_malformed_row_is_dropped_not_guessed() {
        assert_eq!(parse_listen_line("   0: nonsense"), None);
        assert_eq!(parse_listen_line(""), None);
    }
}
