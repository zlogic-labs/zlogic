//! Startup terminal probes — one raw-mode session, one deadline:
//! - **OSC 11** background colour → dark/light for `theme: auto`.
//!   This is what makes auto real on macOS — Terminal.app never exports COLORFGBG,
//!   so env-only detection always landed on dark (One-Dark on a white background).
//! - **CSI 6n** ambiguous-width measurement: print one East-Asian-AMBIGUOUS char
//!   (`█`), ask for the cursor column, and see whether it advanced 1 or 2 cells.
//!   Replaces the blanket "Terminal.app → ambiguous-wide → ASCII logo/icons" guess —
//!   default Terminal.app renders ambiguous narrow and deserves the block glyphs.
//! - **DECRQM mode 2026** (`CSI ? 2026 $ p`) → does the terminal actually know DEC
//!   2026 synchronized output? Replaces the known-terminal-table guess in caps.rs —
//!   upgrades generic `xterm-256color` terminals that do support it, and disables it
//!   where the table was wrong. Ps=0 in the reply means "mode not recognized";
//!   1–4 all mean recognized. No reply → `None`, env heuristic stands.
//! Runs ONCE, BEFORE the TUI exists: briefly owns raw mode, writes both queries in
//! one flush, and reads replies off stdin with a poll deadline. Replies arrive in
//! query order and DSR (6n) is answered by effectively every terminal, so the read
//! normally ends at the DSR reply, not the timeout. No tty / timeout → `None`
//! fields, callers keep the env-based fallbacks. Typed-ahead bytes drained
//! alongside the replies are discarded — nothing has prompted the user yet.
//! **Windows**: terminals are probed exactly like Unix. Windows Terminal and other
//! modern emulators answer all three queries through ConPTY; bare conhost answers
//! DSR itself (GDI cursor math) but not OSC 11 / DECRQM, which is itself a useful
//! measurement (its wide `█` keeps the ASCII downgrade on *measured* grounds rather
//! than a blanket guess). The probe briefly enables `ENABLE_VIRTUAL_TERMINAL_INPUT`
//! on the console input so replies arrive as a VT byte stream, and restores the
//! previous mode afterwards.

use crate::term::caps::Background;

#[derive(Debug, Default, Clone, Copy)]
pub struct ProbeOutcome {
    pub background: Option<Background>,
    pub ambiguous_wide: Option<bool>,
    /// DECRQM answer for mode 2026: `Some(true)` = recognized, `Some(false)` =
    /// explicitly not recognized, `None` = no reply (keep the env heuristic).
    pub supports_sync: Option<bool>,
}

#[cfg(unix)]
pub fn run(want_background: bool) -> ProbeOutcome {
    unix::run(want_background)
}

#[cfg(windows)]
pub fn run(want_background: bool) -> ProbeOutcome {
    windows::run(want_background)
}

#[cfg(not(any(unix, windows)))]
pub fn run(_want_background: bool) -> ProbeOutcome {
    ProbeOutcome::default()
}

/// Reply parsers shared by the unix poll loop and the windows wait-loop. Each
/// parser is written to find its own reply inside a combined buffer, so the same
/// tests cover both platforms.
mod parse {
    use super::Background;

    /// Find the DSR reply `ESC [ row ; col R` and return `col`.
    pub(super) fn cursor_col(buf: &[u8]) -> Option<u16> {
        let mut search = buf;
        while let Some(start) = search.windows(2).position(|w| w == b"\x1b[") {
            let rest = &search[start + 2..];
            if let Some(end) = rest.iter().position(|&b| !b.is_ascii_digit() && b != b';') {
                if rest[end] == b'R' {
                    if let Ok(body) = std::str::from_utf8(&rest[..end]) {
                        if let Some((_row, col)) = body.split_once(';') {
                            if let Ok(col) = col.parse::<u16>() {
                                if col >= 1 {
                                    return Some(col);
                                }
                            }
                        }
                    }
                }
                search = &rest[end..];
            } else {
                return None;
            }
        }
        None
    }

    /// Find the DECRQM reply `ESC [ ? 2026 ; Ps $ y`. Ps=0 = "mode not recognized";
    /// 1–4 (set / reset / permanently set / permanently reset) all mean the terminal
    /// knows DEC 2026. Distinct from the DSR scanner: this requires the literal
    /// `?2026;` prefix and the `$y` final, so a cursor report can never match.
    pub(super) fn decrqm_2026(buf: &[u8]) -> Option<bool> {
        let start = buf.windows(8).position(|w| w == b"\x1b[?2026;")? + 8;
        let rest = &buf[start..];
        let end = rest.iter().position(|&b| !b.is_ascii_digit())?;
        if end == 0 || !rest[end..].starts_with(b"$y") {
            return None;
        }
        let ps = std::str::from_utf8(&rest[..end])
            .ok()?
            .parse::<u32>()
            .ok()?;
        Some(ps != 0)
    }

    /// Find `ESC ] 11 ;` … `BEL` | `ESC \` and parse an X-style colour spec:
    /// `rgb:RRRR/GGGG/BBBB` (1–4 hex digits per channel) or `#rrggbb`.
    pub(super) fn osc11(buf: &[u8]) -> Option<(f64, f64, f64)> {
        let start = buf.windows(5).position(|w| w == b"\x1b]11;")? + 5;
        let rest = &buf[start..];
        let end = rest
            .iter()
            .position(|&b| b == 0x07)
            .or_else(|| rest.windows(2).position(|w| w == b"\x1b\\"))?;
        let payload = std::str::from_utf8(&rest[..end]).ok()?;
        color_spec(payload)
    }

    fn color_spec(s: &str) -> Option<(f64, f64, f64)> {
        if let Some(body) = s.strip_prefix("rgb:").or_else(|| s.strip_prefix("rgba:")) {
            let mut it = body.split('/');
            let r = channel(it.next()?)?;
            let g = channel(it.next()?)?;
            let b = channel(it.next()?)?;
            return Some((r, g, b));
        }
        if let Some(hex) = s.strip_prefix('#') {
            if hex.len() == 6 {
                let v = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
                return Some((
                    v(0)? as f64 / 255.0,
                    v(2)? as f64 / 255.0,
                    v(4)? as f64 / 255.0,
                ));
            }
        }
        None
    }

    /// One `rgb:` channel: 1–4 hex digits, scaled by its own width (X11 semantics).
    fn channel(s: &str) -> Option<f64> {
        if s.is_empty() || s.len() > 4 {
            return None;
        }
        let v = u32::from_str_radix(s, 16).ok()?;
        let max = (16u32.pow(s.len() as u32) - 1) as f64;
        Some(v as f64 / max)
    }

    pub(super) fn classify((r, g, b): (f64, f64, f64)) -> Background {
        // Rec.709 luma on the gamma-encoded values — plenty for a dark/light split.
        let l = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        if l > 0.5 {
            Background::Light
        } else {
            Background::Dark
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parses_16bit_reply_with_st() {
            let buf = b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\";
            let (r, g, b) = osc11(buf).expect("parse");
            assert!((r - 1.0).abs() < 1e-9 && (g - 1.0).abs() < 1e-9 && (b - 1.0).abs() < 1e-9);
        }

        #[test]
        fn parses_8bit_reply_with_bel_and_leading_noise() {
            let buf = b"junk\x1b]11;rgb:1e/1e/2e\x07";
            let (r, _, b) = osc11(buf).expect("parse");
            assert!((r - 0x1e as f64 / 255.0).abs() < 1e-9);
            assert!((b - 0x2e as f64 / 255.0).abs() < 1e-9);
        }

        #[test]
        fn classifies_dark_and_light() {
            assert_eq!(classify((0.1, 0.1, 0.12)), Background::Dark);
            assert_eq!(classify((0.97, 0.97, 0.95)), Background::Light);
        }

        #[test]
        fn incomplete_reply_is_none() {
            assert!(osc11(b"\x1b]11;rgb:ffff/ffff").is_none());
        }

        #[test]
        fn cursor_report_narrow_vs_wide() {
            assert_eq!(cursor_col(b"\x1b[24;2R"), Some(2)); // narrow █
            assert_eq!(cursor_col(b"\x1b[24;3R"), Some(3)); // wide █
            assert_eq!(cursor_col(b"\x1b[24;2"), None); // incomplete
        }

        #[test]
        fn cursor_report_found_after_osc_reply() {
            let buf = b"\x1b]11;rgb:00/00/00\x07\x1b[1;2R";
            assert_eq!(cursor_col(buf), Some(2));
            assert!(osc11(buf).is_some());
        }

        #[test]
        fn osc_st_terminator_is_not_a_cursor_report() {
            // `ESC \` inside the OSC reply must not confuse the DSR scanner.
            assert_eq!(cursor_col(b"\x1b]11;rgb:aa/bb/cc\x1b\\"), None);
        }

        #[test]
        fn decrqm_recognized_modes_are_true() {
            assert_eq!(decrqm_2026(b"\x1b[?2026;1$y"), Some(true)); // set
            assert_eq!(decrqm_2026(b"\x1b[?2026;2$y"), Some(true)); // reset
            assert_eq!(decrqm_2026(b"\x1b[?2026;4$y"), Some(true)); // perm. reset
        }

        #[test]
        fn decrqm_zero_means_not_recognized() {
            assert_eq!(decrqm_2026(b"\x1b[?2026;0$y"), Some(false));
        }

        #[test]
        fn decrqm_absent_or_incomplete_is_none() {
            assert_eq!(decrqm_2026(b""), None);
            assert_eq!(decrqm_2026(b"\x1b[1;2R"), None); // DSR reply alone
            assert_eq!(decrqm_2026(b"\x1b[?2026;1"), None); // missing `$y`
        }

        #[test]
        fn combined_buffer_all_three_replies_parse_independently() {
            // Real startup shape: OSC 11 + DECRQM + DSR in query order. Each parser
            // must find its own reply and — critically — the DECRQM reply must not
            // read as a cursor report (its `2026;2` digits look row;col-ish).
            let buf = b"\x1b]11;rgb:1e1e/1e1e/2e2e\x07\x1b[?2026;2$y\x1b[24;2R";
            assert!(osc11(buf).is_some());
            assert_eq!(decrqm_2026(buf), Some(true));
            assert_eq!(cursor_col(buf), Some(2));
            assert_eq!(cursor_col(b"\x1b[?2026;2$y"), None);
        }
    }
}

/// Build the one-flush query payload: OSC 11 (when wanted) + DECRQM 2026 + the
/// ambiguous-width probe (`█` at column 1, then DSR). DECRQM goes BEFORE the final
/// CSI 6n — DSR is the tail marker that ends the read loop, so anything queried
/// after it could be cut off.
fn query_payload(want_background: bool) -> Vec<u8> {
    let mut q: Vec<u8> = Vec::with_capacity(48);
    if want_background {
        q.extend_from_slice(b"\x1b]11;?\x1b\\");
    }
    q.extend_from_slice(b"\x1b[?2026$p");
    q.extend_from_slice("\r\x1b[2K█\x1b[6n".as_bytes());
    q
}

fn outcome_from(buf: &[u8]) -> ProbeOutcome {
    ProbeOutcome {
        background: parse::osc11(buf).map(parse::classify),
        ambiguous_wide: parse::cursor_col(buf).map(|col| col >= 3),
        supports_sync: parse::decrqm_2026(buf),
    }
}

#[cfg(unix)]
mod unix {
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::time::{Duration, Instant};

    use crossterm::tty::IsTty;

    use super::{outcome_from, parse, query_payload, ProbeOutcome};

    /// Total reply budget. Local terminals answer in single-digit milliseconds; the
    /// margin is for ssh. Only paid in full when DSR never answers (rare).
    const TIMEOUT: Duration = Duration::from_millis(150);

    pub fn run(want_background: bool) -> ProbeOutcome {
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout();
        if !stdin.is_tty() || !stdout.is_tty() {
            return ProbeOutcome::default();
        }
        let term = std::env::var("TERM").unwrap_or_default();
        if term.is_empty() || term == "dumb" || term == "linux" {
            return ProbeOutcome::default();
        }

        // Raw mode so replies arrive unbuffered and un-zlogiced. Restore on EVERY path —
        // the TUI's own Guard isn't armed yet.
        let was_raw = crossterm::terminal::is_raw_mode_enabled().unwrap_or(false);
        if !was_raw && crossterm::terminal::enable_raw_mode().is_err() {
            return ProbeOutcome::default();
        }
        let buf = (|| {
            stdout.write_all(&query_payload(want_background)).ok()?;
            stdout.flush().ok()?;
            let buf = read_replies(stdin.as_raw_fd());
            // Erase the probe char no matter what came back.
            let _ = stdout.write_all(b"\r\x1b[2K");
            let _ = stdout.flush();
            buf
        })();
        if !was_raw {
            let _ = crossterm::terminal::disable_raw_mode();
        }
        let Some(buf) = buf else {
            return ProbeOutcome::default();
        };
        outcome_from(&buf)
    }

    /// Poll stdin until the DSR cursor report (always last, near-universally
    /// supported) parses, the deadline passes, or the buffer gets implausibly large.
    fn read_replies(fd: i32) -> Option<Vec<u8>> {
        let deadline = Instant::now() + TIMEOUT;
        let mut buf: Vec<u8> = Vec::with_capacity(64);
        let mut chunk = [0u8; 64];
        loop {
            if parse::cursor_col(&buf).is_some() {
                return Some(buf);
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                // Timeout: return what arrived (an OSC 11 reply alone still counts).
                return (!buf.is_empty()).then_some(buf);
            };
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let rc = unsafe { libc::poll(&mut pfd, 1, remaining.as_millis() as i32) };
            if rc <= 0 {
                return (!buf.is_empty()).then_some(buf);
            }
            let n = std::io::stdin().read(&mut chunk).ok()?;
            if n == 0 {
                return (!buf.is_empty()).then_some(buf);
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.len() > 4096 {
                return Some(buf);
            }
        }
    }
}

#[cfg(windows)]
mod windows {
    use std::io::Write;
    use std::time::{Duration, Instant};

    use crossterm::tty::IsTty;
    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
    use windows_sys::Win32::Storage::FileSystem::ReadFile;
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_VIRTUAL_TERMINAL_INPUT,
        STD_INPUT_HANDLE,
    };
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    use super::{outcome_from, parse, query_payload, ProbeOutcome};

    /// Console/conpty round trips are slightly slower than a unix tty; the DSR reply
    /// still normally arrives in single-digit milliseconds under Windows Terminal.
    const TIMEOUT: Duration = Duration::from_millis(300);

    pub fn run(want_background: bool) -> ProbeOutcome {
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout();
        if !stdin.is_tty() || !stdout.is_tty() {
            return ProbeOutcome::default();
        }
        if std::env::var("TERM").map(|t| t == "dumb").unwrap_or(false) {
            return ProbeOutcome::default();
        }

        let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return ProbeOutcome::default();
        }

        let was_raw = crossterm::terminal::is_raw_mode_enabled().unwrap_or(false);
        if !was_raw && crossterm::terminal::enable_raw_mode().is_err() {
            return ProbeOutcome::default();
        }

        // Enable VT input so replies arrive as one atomic escape stream instead of
        // individual console input records. Restore the previous mode on every path.
        let mut old_mode = 0u32;
        let vt_input_ready = unsafe { GetConsoleMode(handle, &mut old_mode) } != 0
            && unsafe { SetConsoleMode(handle, old_mode | ENABLE_VIRTUAL_TERMINAL_INPUT) } != 0;

        let buf = if vt_input_ready {
            let r = (|| {
                stdout.write_all(&query_payload(want_background)).ok()?;
                stdout.flush().ok()?;
                read_replies(handle)
            })();
            // Restore the input mode and erase the probe char no matter what came back.
            unsafe { SetConsoleMode(handle, old_mode) };
            let _ = stdout.write_all(b"\r\x1b[2K");
            let _ = stdout.flush();
            r
        } else {
            None
        };

        if !was_raw {
            let _ = crossterm::terminal::disable_raw_mode();
        }
        let Some(buf) = buf else {
            return ProbeOutcome::default();
        };
        outcome_from(&buf)
    }

    /// Wait on the input handle (console buffers and conpty pipes are both waitable)
    /// until the DSR cursor report parses, the deadline passes, or the buffer gets
    /// implausibly large. On timeout, drain whatever arrived so late replies cannot
    /// leak into the TUI's input stream.
    fn read_replies(handle: HANDLE) -> Option<Vec<u8>> {
        let deadline = Instant::now() + TIMEOUT;
        let mut buf: Vec<u8> = Vec::with_capacity(64);
        let mut chunk = [0u8; 64];
        loop {
            if parse::cursor_col(&buf).is_some() {
                return Some(buf);
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                drain_ready(handle, &mut buf, &mut chunk);
                // Timeout: return what arrived (an OSC 11 reply alone still counts).
                return (!buf.is_empty()).then_some(buf);
            };
            let ms = remaining.as_millis().min(u32::MAX as u128) as u32;
            if unsafe { WaitForSingleObject(handle, ms) } != WAIT_OBJECT_0 {
                drain_ready(handle, &mut buf, &mut chunk);
                return (!buf.is_empty()).then_some(buf);
            }
            let mut n = 0u32;
            let ok = unsafe {
                ReadFile(
                    handle,
                    chunk.as_mut_ptr(),
                    chunk.len() as u32,
                    &mut n,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 || n == 0 {
                drain_ready(handle, &mut buf, &mut chunk);
                return (!buf.is_empty()).then_some(buf);
            }
            buf.extend_from_slice(&chunk[..n as usize]);
            if buf.len() > 4096 {
                return Some(buf);
            }
        }
    }

    /// Non-blocking `WaitForSingleObject(…, 0)` + read, for the timeout paths.
    fn drain_ready(handle: HANDLE, buf: &mut Vec<u8>, chunk: &mut [u8; 64]) {
        while unsafe { WaitForSingleObject(handle, 0) } == WAIT_OBJECT_0 {
            let mut n = 0u32;
            let ok = unsafe {
                ReadFile(
                    handle,
                    chunk.as_mut_ptr(),
                    chunk.len() as u32,
                    &mut n,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 || n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n as usize]);
        }
    }
}
