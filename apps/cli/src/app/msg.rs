//! `Msg` — the single FIFO the render loop consumes. Every update source
//! (input reader, stream forwarder, tick) only *sends* `Msg`; nothing else writes the tty.

use crossterm::event::{KeyEvent, MouseEvent};

use super::FilePickerEntry;
use crate::session::dto::{ConnResult, CoreBusEvent, CoreEvent};

pub enum Msg {
    Key(KeyEvent),
    Paste(String),
    /// Overlay-scoped mouse input, currently used for wheel scrolling.
    Mouse(MouseEvent),
    /// Terminal resized to (cols, rows).
    Resize(u16, u16),
    /// From the agent stream (EngineSession) — 50ms-throttled in the loop.
    Stream(CoreEvent),
    /// Core event-bus invalidation; handlers refresh payloads through CoreSession APIs.
    Bus(CoreBusEvent),
    FileScan {
        generation: u64,
        entries: Vec<FilePickerEntry>,
    },
    TestResult {
        model: String,
        result: Option<ConnResult>,
    },
    /// Animation heartbeat (spinner) + stream tail flush. Only ticks while live.
    Tick,
}
