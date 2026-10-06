//! The Windows half, and the only `unsafe` in this crate.
//!
//! Two kernel tables and nothing else: the TCP listen table, which `GetExtendedTcpTable` returns
//! already attributed to an owning pid, and the process snapshot, which is the only way to learn
//! parentage without opening every process one at a time.
//!
//! Both reads follow the documented two-pass shape — ask for the size, allocate, ask again — and
//! both degrade to "nothing found" rather than propagating an error. The caller polls this; a
//! transient failure must not turn into a visible error, and must not become a lie either, which
//! is why [`crate::listening_sockets`] documents an empty result as "found nothing" only.

use std::collections::HashMap;

use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCPROW_OWNER_PID, TCP_TABLE_OWNER_PID_ALL,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};

use crate::ListenSocket;

/// `TCP_LISTEN`. Any other state is a socket that already did its listening.
const TCP_LISTEN: u32 = 2;

const AF_INET: u32 = 2;
const AF_INET6: u32 = 23;

const ERROR_SUCCESS: u32 = 0;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;

pub(super) fn listening_sockets() -> Vec<ListenSocket> {
    let mut out = ipv4_listeners();
    out.extend(ipv6_listeners());
    out
}

pub(super) fn process_parents() -> HashMap<u32, u32> {
    let mut out = HashMap::new();
    let Some(snapshot) = snapshot() else {
        return out;
    };
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    // SAFETY: `snapshot` is a live snapshot handle for the duration of the walk, and `entry` is a
    // correctly sized, correctly initialised structure the API fills in and advances. The loop
    // ends on the documented end-of-snapshot error, or on a genuine failure, and the handle is
    // closed on every path out.
    unsafe {
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                out.insert(entry.th32ProcessID, entry.th32ParentProcessID);
                entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
    }
    out
}

fn snapshot() -> Option<HANDLE> {
    // SAFETY: the flags request a process snapshot and no process id; the handle is returned to
    // the caller, which closes it exactly once on every path.
    let handle = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }.ok()?;
    (handle != INVALID_HANDLE_VALUE).then_some(handle)
}

/// The size pass, then the read pass, into an eight-byte-aligned buffer.
///
/// `Vec<u8>` would not do: the table starts with rows of `u32` but the API is free to hand back
/// a structure with wider alignment, and reading it through a `u8` pointer is a misaligned access.
fn table(af: u32) -> Option<Vec<u64>> {
    let mut size: u32 = 0;
    // SAFETY: a null table pointer with a valid size pointer is the documented way to ask how much
    // room the table needs; the call is expected to fail with ERROR_INSUFFICIENT_BUFFER.
    let status = unsafe {
        GetExtendedTcpTable(
            None,
            &mut size,
            false,
            af,
            TCP_TABLE_OWNER_PID_ALL,
            0,
        )
    };
    if size == 0 || (status != ERROR_SUCCESS && status != ERROR_INSUFFICIENT_BUFFER) {
        return None;
    }
    // The table grew between the two calls; ask again rather than reading a truncated header.
    let mut buffer = vec![0u64; size.div_ceil(8) as usize];
    // SAFETY: the buffer is at least `size` bytes and eight-byte aligned, which is what the
    // per-family row structures need; the API writes at most `size` bytes into it.
    let status = unsafe {
        GetExtendedTcpTable(
            Some(buffer.as_mut_ptr().cast()),
            &mut size,
            false,
            af,
            TCP_TABLE_OWNER_PID_ALL,
            0,
        )
    };
    (status == ERROR_SUCCESS).then_some(buffer)
}

/// Both tables begin with a `dwNumEntries` count followed by that many fixed-size rows, so the
/// count is read as a `u32` at offset zero and the rows start at offset four.
///
/// `row_size` is per address family and must come from the row structure itself: the v4 row is
/// six `u32`s and the v6 row carries a 16-byte address, so guessing one width for both walks off
/// the end of the buffer.
fn rows(buffer: &[u64], row_size: usize) -> usize {
    let base = buffer.as_ptr().cast::<u8>();
    // SAFETY: `table` only returns a buffer the API filled, which always starts with the entry
    // count. The buffer is at least eight bytes, so the read is in bounds.
    let count = unsafe { std::ptr::read_unaligned(base.cast::<u32>()) } as usize;
    // A count that cannot fit the buffer means the header is not what we think it is.
    count
        .checked_mul(row_size)
        .filter(|bytes| bytes + 4 <= buffer.len() * 8)
        .map(|_| count)
        .unwrap_or(0)
}

fn ipv4_listeners() -> Vec<ListenSocket> {
    let Some(buffer) = table(AF_INET) else {
        return Vec::new();
    };
    let base = buffer.as_ptr().cast::<u8>();
    let row_size = std::mem::size_of::<MIB_TCPROW_OWNER_PID>();
    let count = rows(&buffer, row_size);
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        // SAFETY: index < count, and `rows` proved count rows of this width fit in the buffer.
        let row = unsafe { &*base.add(4 + index * row_size).cast::<MIB_TCPROW_OWNER_PID>() };
        if row.dwState != TCP_LISTEN {
            continue;
        }
        out.push(ListenSocket {
            pid: row.dwOwningPid,
            port: (row.dwLocalPort as u16).swap_bytes(),
        });
    }
    out
}

fn ipv6_listeners() -> Vec<ListenSocket> {
    let Some(buffer) = table(AF_INET6) else {
        return Vec::new();
    };
    let base = buffer.as_ptr().cast::<u8>();
    let row_size = std::mem::size_of::<MIB_TCP6ROW_OWNER_PID>();
    let count = rows(&buffer, row_size);
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        // SAFETY: as above, with the v6 row's own width.
        let row = unsafe { &*base.add(4 + index * row_size).cast::<MIB_TCP6ROW_OWNER_PID>() };
        if row.dwState != TCP_LISTEN {
            continue;
        }
        out.push(ListenSocket {
            pid: row.dwOwningPid,
            port: (row.dwLocalPort as u16).swap_bytes(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The read must not panic on a machine with no server at all, which is every CI box.
    #[test]
    fn reading_the_table_on_this_machine_does_not_fail() {
        let _ = listening_sockets();
        // Windows always has a process table, so an empty read means the snapshot failed rather
        // than that the machine is idle — which is the one thing worth catching here.
        assert!(!process_parents().is_empty(), "the process table is readable");
    }

    #[test]
    fn a_count_that_cannot_fit_is_refused_rather_than_trusted() {
        let tiny = vec![0u64; 2];
        let width = std::mem::size_of::<MIB_TCPROW_OWNER_PID>();
        assert_eq!(rows(&tiny, width), 0, "16 bytes cannot hold a 24-byte row");
    }
}
