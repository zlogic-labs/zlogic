//! The render loop: single writer, message-driven, frame-coalescing.
//! It is the ONLY place that writes the tty (term::restore's raw bytes excepted).
//! Single-threaded poll model: `event::poll` reads input AND doubles as the tick timer,
//! so nothing else touches stdin. The agent stream arrives on an mpsc channel
//! (no stdin), forwarded by one helper thread.

use std::io;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crossterm::cursor::MoveTo;
use crossterm::event::{self, Event};
use crossterm::execute;
use crossterm::terminal::{self as crossterm_terminal, Clear, ClearType};
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::{Paragraph, Widget};
use ratatui::{Terminal, TerminalOptions, Viewport};

use super::update::update;
use super::{AppState, Msg};
use crate::glyph::IconTier;
use crate::i18n::Locale;
use crate::render::view::{history_tail_height, view};
use crate::session::dto::PermissionMode;
use crate::session::CoreSession;
use crate::term::{self, sync, Guard};
use crate::theme::ThemeState;

const FLUSH_STREAM_DELTA_MS: u128 = 50; // inherit TS FLUSH_STREAM_DELTA_MS
const POLL_MS: u64 = 80; // input poll + tick cadence
const SPIN_MS: u128 = 250; // spinner step
/// The terminal's visible screen is the dynamic window: normally recent history,
/// temporarily completion overlays. Older rows are real terminal scrollback.
const MIN_VIEWPORT_H: u16 = 7;

type Term = Terminal<CrosstermBackend<io::Stdout>>;

fn new_terminal(height: u16) -> io::Result<Term> {
    Terminal::with_options(
        CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(height.max(1)),
        },
    )
}

/// A second Terminal used only while a fullscreen overlay owns the alternate
/// screen. Created on entry, dropped on exit; the inline Terminal
/// stays alive (and untouched) underneath.
fn new_alt_terminal() -> io::Result<Term> {
    Terminal::with_options(
        CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Fullscreen,
        },
    )
}

fn startup_viewport_height(term_rows: u16) -> u16 {
    term_rows.max(MIN_VIEWPORT_H)
}

/// Which screen the render loop currently owns (overlay = temporary
/// alt-screen). Pure state machine — the terminal plumbing hangs off `screen_switch`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ScreenMode {
    /// Main flow: inline viewport + real scrollback. No mouse capture.
    Inline,
    /// A fullscreen overlay owns the alternate screen. Mouse capture on;
    /// `insert_before` scrollback flushing paused.
    Alt,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ScreenSwitch {
    Stay,
    EnterAlt,
    LeaveAlt,
}

fn initial_mode(plan: bool, god: bool) -> super::Mode {
    if god {
        super::Mode::God
    } else if plan {
        super::Mode::Plan
    } else {
        super::Mode::Normal
    }
}

/// The single decision point for screen transitions: the desired screen is a pure
/// function of "is a fullscreen overlay open", diffed against the current mode.
fn screen_switch(current: ScreenMode, fullscreen_overlay_open: bool) -> ScreenSwitch {
    match (current, fullscreen_overlay_open) {
        (ScreenMode::Inline, true) => ScreenSwitch::EnterAlt,
        (ScreenMode::Alt, false) => ScreenSwitch::LeaveAlt,
        _ => ScreenSwitch::Stay,
    }
}

/// Run the interactive TUI against a `CoreSession`. Blocks until quit.
pub fn run(
    session: Box<dyn CoreSession>,
    theme: ThemeState,
    icons: IconTier,
    locale: Locale,
    plan: bool,
    god: bool,
    permission: PermissionMode,
) -> io::Result<()> {
    let session: Arc<dyn CoreSession> = session.into();
    session.set_plan_mode(plan);
    let events = session.subscribe();
    let bus_events = session.subscribe_bus();
    let (tx, rx) = mpsc::channel::<Msg>();

    // Stream forwarder: CoreEvent → Msg::Stream. Reads the agent channel, never stdin.
    {
        let tx = tx.clone();
        thread::spawn(move || {
            for ev in events.iter() {
                if tx.send(Msg::Stream(ev)).is_err() {
                    break;
                }
            }
        });
    }
    // Event bus carries invalidations only. The update handler calls the relevant
    // CoreSession API on the render thread to obtain a coherent snapshot.
    {
        let tx = tx.clone();
        thread::spawn(move || {
            for ev in bus_events.iter() {
                if tx.send(Msg::Bus(ev)).is_err() {
                    break;
                }
            }
        });
    }

    let mut guard = Guard::enter_tui()?;

    // Clear the whole terminal + scrollback before entering (user request), and home
    // the cursor so the inline viewport anchors at the top of a clean screen.
    {
        let mut out = io::stdout();
        execute!(
            out,
            Clear(ClearType::All),
            Clear(ClearType::Purge),
            MoveTo(0, 0)
        )?;
    }

    let (_, rows) = crossterm_terminal::size().unwrap_or((80, 24));
    let mut viewport_h = startup_viewport_height(rows);
    let mut terminal = new_terminal(viewport_h)?;
    let width = terminal.size().map(|s| s.width).unwrap_or(80);
    let mut state = AppState::with_locale(theme, icons, width, viewport_h, rows, locale);
    state.mode = initial_mode(plan, god);
    state.plan_enabled = plan;
    state.god_enabled = god;
    state.permission_mode = permission;
    state.refresh_status_snapshot(session.as_ref());
    // Dev-only render-repro hook (see update::debug_open_overlay).
    if let Ok(cmd) = std::env::var("ZLOGIC_DEBUG_OVERLAY") {
        if !cmd.is_empty() {
            super::update::debug_open_overlay(&mut state, session.as_ref(), &cmd);
        }
    }
    let mut last_draw = Instant::now();
    let mut last_spin = Instant::now();

    // Alt-screen bookkeeping: the alt Terminal exists only while a
    // fullscreen overlay is open; `inline_stale` records a resize seen while in
    // alt, so the inline viewport is rebuilt (clear + rewrap + replay) on return.
    let mut screen = ScreenMode::Inline;
    let mut alt_terminal: Option<Term> = None;
    let mut inline_stale = false;

    // First paint. If the dev repro hook opened a fullscreen overlay already, skip
    // it — the overlay must never be drawn into the inline viewport; the switch
    // below enters alt-screen on the first loop iteration instead.
    if !state.overlay_is_fullscreen() {
        render_frame(&mut terminal, &mut state)?;
    }

    loop {
        // 1) input (poll doubles as the tick timer — no separate input thread)
        if event::poll(Duration::from_millis(POLL_MS))? {
            match event::read()? {
                Event::Key(k) => update(&mut state, Msg::Key(k), session.as_ref()),
                Event::Paste(text) => update(&mut state, Msg::Paste(text), session.as_ref()),
                // Mouse capture is only enabled while in alt-screen; gate anyway so
                // a burst raced with the overlay close cannot reach the main flow.
                Event::Mouse(m) if screen == ScreenMode::Alt => {
                    update(&mut state, Msg::Mouse(m), session.as_ref())
                }
                Event::Resize(w, h) => {
                    let (w, h) = drain_resize_events(w, h, &mut state, session.as_ref())?;
                    update(&mut state, Msg::Resize(w, h), session.as_ref());
                    if screen == ScreenMode::Alt {
                        // Rebuild only the ALT terminal now; the inline viewport +
                        // scrollback replay are rebuilt when we return (stale flag).
                        inline_stale = true;
                        if state.overlay_is_fullscreen() {
                            let alt = alt_terminal.insert(new_alt_terminal()?);
                            render_alt_frame(alt, &mut state)?;
                            last_draw = Instant::now();
                            continue;
                        }
                        // Overlay closed mid-drain → fall through; the switch below
                        // leaves alt and the stale flag triggers the inline rebuild.
                    } else {
                        let mut out = io::stdout();
                        sync::begin(&mut out)?;
                        execute!(
                            out,
                            Clear(ClearType::All),
                            Clear(ClearType::Purge),
                            MoveTo(0, 0)
                        )?;
                        viewport_h = startup_viewport_height(h);
                        terminal = new_terminal(viewport_h)?;
                        state.reset_scrollback_replay();
                        // A key handled during the drain may have OPENED a fullscreen
                        // overlay — never paint that into the inline viewport; leave
                        // the frame to the switch below (enter alt, then draw).
                        if !state.overlay_is_fullscreen() {
                            render_frame_body(&mut terminal, &mut state)?;
                            sync::end(&mut out)?;
                            last_draw = Instant::now();
                            continue;
                        }
                        sync::end(&mut out)?;
                    }
                }
                _ => {}
            }
        } else if state.needs_animation() && last_spin.elapsed().as_millis() >= SPIN_MS {
            update(&mut state, Msg::Tick, session.as_ref());
            last_spin = Instant::now();
        }

        // 2) drain the agent stream + any queued msgs → frame coalescing
        while let Ok(m) = rx.try_recv() {
            update(&mut state, m, session.as_ref());
        }
        launch_file_scan(&mut state, &tx);
        launch_test(&mut state, &session, &tx);

        if state.should_quit {
            break; // restore_terminal below leaves alt-screen / mouse capture too
        }

        // 3) overlay ↔ alt-screen transitions, before any drawing so
        // a fullscreen overlay is never painted into the inline viewport.
        match screen_switch(screen, state.overlay_is_fullscreen()) {
            ScreenSwitch::EnterAlt => {
                term::enter_alt_screen()?;
                alt_terminal = Some(new_alt_terminal()?);
                screen = ScreenMode::Alt;
                state.dirty = true;
            }
            ScreenSwitch::LeaveAlt => {
                alt_terminal = None;
                term::leave_alt_screen()?;
                screen = ScreenMode::Inline;
                if inline_stale {
                    // The terminal resized while we were in alt: the restored main
                    // screen no longer fits — run the same clear + rewrap + replay
                    // path as an inline resize.
                    let (w, h) = crossterm_terminal::size().unwrap_or((80, 24));
                    update(&mut state, Msg::Resize(w, h), session.as_ref());
                    let mut out = io::stdout();
                    sync::begin(&mut out)?;
                    execute!(
                        out,
                        Clear(ClearType::All),
                        Clear(ClearType::Purge),
                        MoveTo(0, 0)
                    )?;
                    viewport_h = startup_viewport_height(h);
                    terminal = new_terminal(viewport_h)?;
                    state.reset_scrollback_replay();
                    render_frame_body(&mut terminal, &mut state)?;
                    sync::end(&mut out)?;
                    inline_stale = false;
                    last_draw = Instant::now();
                } else {
                    // The restored main screen still contains the exact pre-overlay
                    // frame (often an open `/se` completion box). State changed while
                    // alt-screen owned the terminal, so invalidate both the physical
                    // viewport and ratatui's cached diff buffer before redrawing.
                    // Clear(All) preserves real terminal scrollback; never Purge here.
                    let mut out = io::stdout();
                    sync::begin(&mut out)?;
                    terminal.clear()?;
                    sync::end(&mut out)?;
                }
                state.dirty = true;
            }
            ScreenSwitch::Stay => {}
        }

        if !state.dirty {
            continue;
        }
        // 50ms stream throttle: a pure-stream burst waits for the next tick to flush.
        if state.pending_stream_only && last_draw.elapsed().as_millis() < FLUSH_STREAM_DELTA_MS {
            continue;
        }

        match (screen, alt_terminal.as_mut()) {
            (ScreenMode::Alt, Some(alt)) => render_alt_frame(alt, &mut state)?,
            _ => render_frame(&mut terminal, &mut state)?,
        }
        last_draw = Instant::now();
    }

    // Drain pending terminal query replies (e.g. the DSR `\x1b[..R` that ratatui's
    // inline init / `terminal.resize` elicits) BEFORE leaving raw mode, so the shell
    // doesn't zlogic the leftover as a stray `[` after we exit. Must run while raw is on.
    while event::poll(Duration::from_millis(0)).unwrap_or(false) {
        let _ = event::read();
    }

    let resume_tip = session
        .current_session_id()
        .map(|id| format!("zlogic --resume {id}"))
        .unwrap_or_else(|| "zlogic --resume <sessionId>".to_string());
    term::restore_terminal();
    guard.disarm();
    state.show_exit_splash(resume_tip);
    render_frame_body(&mut terminal, &mut state)?;
    Ok(())
}

fn launch_file_scan(state: &mut AppState, tx: &mpsc::Sender<Msg>) {
    let Some(request) = super::update::take_file_scan_request(state) else {
        return;
    };
    let tx = tx.clone();
    thread::spawn(move || {
        let entries = super::update::glob_picker_entries(&request.query);
        let _ = tx.send(Msg::FileScan {
            generation: request.generation,
            entries,
        });
    });
}

fn launch_test(state: &mut AppState, session: &Arc<dyn CoreSession>, tx: &mpsc::Sender<Msg>) {
    let Some(model) = state.take_test_request() else {
        return;
    };
    let session = Arc::clone(session);
    let tx = tx.clone();
    thread::spawn(move || {
        let result = session.test_connectivity(&model);
        let _ = tx.send(Msg::TestResult { model, result });
    });
}

/// One 2026-wrapped frame: flush history to scrollback, then redraw the viewport
/// (property 3 — same window, history before viewport).
fn render_frame(terminal: &mut Term, state: &mut AppState) -> io::Result<()> {
    let mut out = io::stdout();
    sync::begin(&mut out)?;
    render_frame_body(terminal, state)?;
    sync::end(&mut out)?;
    Ok(())
}

fn render_frame_body(terminal: &mut Term, state: &mut AppState) -> io::Result<()> {
    // Hide the cursor BEFORE painting. ratatui only hides/shows it at the END of
    // `draw`, so without this the previous frame's visible cursor rides along the
    // cell diffs while they paint (no DEC 2026 on e.g. Terminal.app) and flashes at
    // the right edge of the last-painted row. Hidden here, it reappears only via
    // draw's final show+move pair, which lands in one flush (effectively atomic).
    // (NOTE: a STATIC grey `[` `]` pair around the input row in macOS Terminal.app is
    // a different thing — Terminal.app's own automatic prompt-line marks, drawn on the
    // cursor row when Return/Ctrl-C is typed. Terminal feature, not our bytes; users
    // turn it off via Edit > Marks > Automatically Mark Prompt Lines.)
    terminal.hide_cursor()?;

    let size = terminal.size()?;
    let tail_rows = history_tail_height(state, size.width, size.height) as usize;
    state.queue_scrollback_until_tail(tail_rows);
    flush_history_outbox(terminal, state)?;

    let mut park: Option<ratatui::layout::Position> = None;
    terminal.draw(|f| park = view(state, f))?;

    // No visible caret this frame → the cursor stays hidden, but PARK it at the caret
    // cell anyway: terminals that render an outline for a hidden/unfocused cursor
    // then show it where a caret plausibly lives instead of wherever painting happened
    // to stop (status-bar right edge, bottom-right corner). Terminal.app's prompt
    // marks also attach to the CURSOR row on Return/Ctrl-C — parking keeps that
    // deterministic (input row) rather than frame-dependent.
    if let Some(cell) = park {
        execute!(io::stdout(), MoveTo(cell.x, cell.y))?;
    }

    state.dirty = false;
    state.pending_stream_only = false;
    Ok(())
}

/// One 2026-wrapped frame on the ALT screen. Unlike `render_frame_body`, this
/// NEVER flushes history to scrollback (`insert_before` writes to the main screen
/// story — paused while the overlay owns the alt screen; pending rows flush on the
/// first inline frame after return) and never parks the cursor (no caret in
/// overlays).
fn render_alt_frame(terminal: &mut Term, state: &mut AppState) -> io::Result<()> {
    let mut out = io::stdout();
    sync::begin(&mut out)?;
    terminal.hide_cursor()?;
    terminal.draw(|f| {
        view(state, f);
    })?;
    sync::end(&mut out)?;
    state.dirty = false;
    state.pending_stream_only = false;
    Ok(())
}

fn flush_history_outbox(terminal: &mut Term, state: &mut AppState) -> io::Result<()> {
    let lines = std::mem::take(&mut state.history_outbox);
    if lines.is_empty() {
        return Ok(());
    }
    let height = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    terminal.insert_before(height, |buf: &mut Buffer| {
        for (index, line) in lines.into_iter().enumerate() {
            let y = index as u16;
            if y >= buf.area.height {
                break;
            }
            let area = Rect {
                x: buf.area.x,
                y: buf.area.y + y,
                width: buf.area.width,
                height: 1,
            };
            Paragraph::new(line).render(area, buf);
        }
    })?;
    Ok(())
}

fn drain_resize_events(
    mut w: u16,
    mut h: u16,
    state: &mut AppState,
    session: &dyn CoreSession,
) -> io::Result<(u16, u16)> {
    while event::poll(Duration::from_millis(0))? {
        match event::read()? {
            Event::Resize(next_w, next_h) => {
                w = next_w;
                h = next_h;
            }
            Event::Key(k) => update(state, Msg::Key(k), session),
            Event::Paste(text) => update(state, Msg::Paste(text), session),
            _ => {}
        }
    }
    Ok((w, h))
}

/// Install the panic hook and signal handlers (terminal restore) before entering raw
/// mode — only the interactive TUI pays for the recovery thread.
pub fn arm_recovery() {
    term::install_panic_hook();
    term::install_signal_handlers();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Mode;

    #[test]
    fn inline_enters_alt_only_when_a_fullscreen_overlay_opens() {
        assert_eq!(
            screen_switch(ScreenMode::Inline, true),
            ScreenSwitch::EnterAlt
        );
        assert_eq!(screen_switch(ScreenMode::Inline, false), ScreenSwitch::Stay);
    }

    #[test]
    fn alt_leaves_only_when_the_overlay_closes() {
        assert_eq!(
            screen_switch(ScreenMode::Alt, false),
            ScreenSwitch::LeaveAlt
        );
        assert_eq!(screen_switch(ScreenMode::Alt, true), ScreenSwitch::Stay);
    }

    #[test]
    fn startup_flags_select_plan_and_god_modes() {
        assert_eq!(initial_mode(false, false), Mode::Normal);
        assert_eq!(initial_mode(true, false), Mode::Plan);
        assert_eq!(initial_mode(false, true), Mode::God);
        assert_eq!(initial_mode(true, true), Mode::God);
    }
}
