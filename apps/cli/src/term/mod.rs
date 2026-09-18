//! Terminal ownership & recovery. Recovery must NOT depend on a live
//! `Terminal`/ratatui — a panic can strike mid-render. Two `AtomicBool`s record the
//! mode; `restore_terminal()` writes raw escape bytes directly. The RAII `Guard`, the
//! panic hook, and the signal handlers all funnel through the same function.

pub mod caps;
pub mod probe;
pub mod sync;

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::cursor::Show;
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};

pub static IN_ALT_SCREEN: AtomicBool = AtomicBool::new(false);
pub static SYNC_OPEN: AtomicBool = AtomicBool::new(false);
pub static RAW_ON: AtomicBool = AtomicBool::new(false);
pub static BRACKETED_PASTE_ON: AtomicBool = AtomicBool::new(false);
/// Overlay-only mouse capture: ON while a fullscreen overlay owns the alt screen,
/// OFF the moment we return inline. Guarded like bracketed paste so terminal restore
/// can issue a balanced disable on every exit path.
pub static MOUSE_CAPTURE_ON: AtomicBool = AtomicBool::new(false);
/// Whether DEC 2026 synchronized output is supported. Conservative
/// default OFF — set once at startup from `TermCaps::supports_sync`. `sync::begin/end`
/// no-op when false, so unsupported terminals never see `?2026` (no stray `[`).
pub static SUPPORTS_SYNC: AtomicBool = AtomicBool::new(false);

const MOUSE_DISABLE: &[u8] = b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1015l\x1b[?1016l";

pub fn set_supports_sync(v: bool) {
    SUPPORTS_SYNC.store(v, Ordering::SeqCst);
}

/// Unconditional, allocation-light restore. Order: close synchronized
/// update → disable mouse capture → leave alt-screen → show cursor → disable raw
/// mode. Safe to call twice.
pub fn restore_terminal() {
    let mut out = std::io::stdout();
    if SYNC_OPEN.swap(false, Ordering::SeqCst) {
        let _ = out.write_all(b"\x1b[?2026l");
    }
    MOUSE_CAPTURE_ON.store(false, Ordering::SeqCst);
    let _ = out.write_all(MOUSE_DISABLE);
    if IN_ALT_SCREEN.swap(false, Ordering::SeqCst) {
        let _ = execute!(out, LeaveAlternateScreen);
    }
    if BRACKETED_PASTE_ON.swap(false, Ordering::SeqCst) {
        let _ = execute!(out, DisableBracketedPaste);
    }
    let _ = execute!(out, Show);
    let _ = out.flush();
    if RAW_ON.swap(false, Ordering::SeqCst) {
        let _ = disable_raw_mode();
    }
}

pub fn disable_bracketed_paste() {
    if BRACKETED_PASTE_ON.swap(false, Ordering::SeqCst) {
        let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    }
}

/// Enter the alternate screen for a fullscreen overlay and enable overlay-scoped
/// mouse capture so wheel input scrolls the overlay instead of terminal history.
/// NOTE: crossterm's `EnableMouseCapture` also enables `?1003h` (any-motion) — every
/// mouse MOVE sends a report. On ptys that fragment escape sequences (ConPTY ⇒ WSL /
/// Windows Terminal, tmux), a report's lone ESC arrives as its own read and crossterm
/// emits it as an Esc KEY, then the leftover `[M…` bytes parse as plain chars →
/// random close / key-form / test / input garbage. So we enable **only** `?1000h`
/// (click + wheel + press/release), which is all the overlay's wheel scrolling needs;
/// movement produces nothing. `DisableMouseCapture` still turns everything off.
pub fn enter_alt_screen() -> std::io::Result<()> {
    execute!(std::io::stdout(), EnterAlternateScreen)?;
    IN_ALT_SCREEN.store(true, Ordering::SeqCst);
    if !MOUSE_CAPTURE_ON.swap(true, Ordering::SeqCst) {
        let _ = std::io::stdout().write_all(b"\x1b[?1000h");
    }
    Ok(())
}

/// Leave the alternate screen (overlay closed). The terminal restores the main
/// screen + scrollback exactly as they were; mouse capture is disabled first
/// (raw ANSI — symmetric with the raw enable in enter_alt_screen).
pub fn leave_alt_screen() -> std::io::Result<()> {
    if MOUSE_CAPTURE_ON.swap(false, Ordering::SeqCst) {
        let _ = std::io::stdout().write_all(MOUSE_DISABLE);
    }
    if IN_ALT_SCREEN.swap(false, Ordering::SeqCst) {
        execute!(std::io::stdout(), LeaveAlternateScreen)?;
    }
    Ok(())
}

/// Install a panic hook that restores the terminal BEFORE printing the panic, so a
/// crash never leaves the tty in raw/alt/hidden-cursor state.
pub fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        prev(info);
    }));
}

/// RAII guard — its `Drop` calls `restore_terminal()` on every normal-exit path,
/// harder to forget than manual cleanup at each return site.
pub struct Guard {
    armed: bool,
}

impl Guard {
    /// Enter raw mode for the interactive TUI. (Inline viewport → no alt-screen for
    /// the main flow; overlays enter/leave alt-screen.)
    pub fn enter_tui() -> std::io::Result<Guard> {
        enable_raw_mode()?;
        let _ = std::io::stdout().write_all(MOUSE_DISABLE);
        let _ = execute!(std::io::stdout(), EnableBracketedPaste);
        RAW_ON.store(true, Ordering::SeqCst);
        BRACKETED_PASTE_ON.store(true, Ordering::SeqCst);
        Ok(Guard { armed: true })
    }

    pub fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if self.armed {
            restore_terminal();
        }
    }
}

/// Restore the terminal on an EXTERNAL kill: `kill`, terminal-window
/// close (SIGHUP), ssh drop. In raw mode ISIG is off, so Ctrl+C arrives as a KEY
/// EVENT — a SIGINT delivered here can only come from outside the tty, and exiting
/// is the right response. The `signal_hook::iterator` API delivers signals to a
/// normal background thread (not an async-signal handler), so calling
/// `restore_terminal()` — allocation-light but not async-signal-safe — is fine.
#[cfg(unix)]
pub fn install_signal_handlers() {
    use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};

    let Ok(mut signals) = signal_hook::iterator::Signals::new([SIGINT, SIGTERM, SIGHUP, SIGQUIT])
    else {
        return; // registration failed → panic hook + RAII Guard still cover exits
    };
    std::thread::spawn(move || {
        if let Some(signo) = signals.forever().next() {
            restore_terminal();
            // Conventional "killed by signal N" exit status.
            std::process::exit(128 + signo);
        }
    });
}

/// Windows has no POSIX signals (console close tears the process down at the OS
/// level, and signal-hook's iterator is unix-only) — keep the call site unconditional.
#[cfg(not(unix))]
pub fn install_signal_handlers() {}
