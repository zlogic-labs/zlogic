//! DEC 2026 synchronized update. Wrap each "erase viewport →
//! insert_before → redraw" in one 2026 window so the terminal shows no intermediate
//! frame. Sent unconditionally — terminals that don't know ?2026 ignore it. Only ever
//! wraps a single frame, never compute/IO (terminals force-render after ~100–150ms).

use std::io::Write;
use std::sync::atomic::Ordering;

use super::{SUPPORTS_SYNC, SYNC_OPEN};

pub fn begin<W: Write>(w: &mut W) -> std::io::Result<()> {
    // Gated on capability: terminals that don't support DEC 2026
    // (Terminal.app, conhost, TTY, unknown) must NOT receive `?2026` — they leak the
    // intro as a stray `[`. No-op leaves SYNC_OPEN false, so restore skips `?2026l` too.
    if !SUPPORTS_SYNC.load(Ordering::SeqCst) {
        return Ok(());
    }
    SYNC_OPEN.store(true, Ordering::SeqCst);
    w.write_all(b"\x1b[?2026h")
}

pub fn end<W: Write>(w: &mut W) -> std::io::Result<()> {
    if !SUPPORTS_SYNC.load(Ordering::SeqCst) {
        w.flush().ok(); // still flush the frame's own bytes
        return Ok(());
    }
    let r = w.write_all(b"\x1b[?2026l");
    w.flush().ok();
    SYNC_OPEN.store(false, Ordering::SeqCst);
    r
}
