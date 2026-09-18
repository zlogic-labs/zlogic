//! `update(state, msg)` — pure state mutation, never touches the terminal.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use std::time::{Duration, Instant};

mod composer;
mod file_picker;
mod models;

use composer::{file_preview, messages_for_submit, validate_submit_messages};
use file_picker::{
    accept as accept_file_picker, apply_scan as apply_file_scan, enter as enter_file_picker,
    move_selection as move_file_picker, refresh as refresh_file_picker,
    remove_entered_directory as remove_file_picker_directory,
};
pub use file_picker::{
    glob_entries as glob_picker_entries, take_scan_request as take_file_scan_request,
};
use models::handle_models_key;

use super::{
    zoom_content_rows, AppState, Chip, ChipKind, ConfirmAction, ConfirmDialog, FormField, FormItem,
    FormValue, FullscreenOverlay, InputHistoryEntry, Mode, ModelTest, Msg, Notification,
    OverlayState, PasteDraft, PendingInteraction, QueuedTurn, RoundBlock, SessionDetailMode,
    SessionsOverlay, Stage, StreamingSendTarget, SuggestItem, SuggestState, TextFormat, ToolRow,
    ViewMode, WorkspacesOverlay, ZoomKind, ZoomOverlay,
};
use crate::glyph::Glyph;
use crate::i18n::Locale;
use crate::log::classify::{self, LogKind};
use crate::markdown::block;
use crate::render::history::{wrap_hist_line, HistLine};
use crate::session::dto::*;
use crate::session::CoreSession;
use crate::splash;
use crate::theme::{self, Sem};
use crate::widgets::rail;

const THINKING_TAIL: usize = 20_000;
const DIRECT_PASTE_CHARS: usize = 2_000;
const DIRECT_PASTE_LINES: usize = 3;
const PICKER_LIMIT: usize = 80;
const BOOT_DONE_HOLD_TICKS: usize = 2;
const MESSAGE_BODY_INDENT: usize = 2;
const INPUT_HISTORY_LIMIT: usize = 50;

pub fn update(state: &mut AppState, msg: Msg, session: &dyn CoreSession) {
    state.pending_stream_only = false;
    match msg {
        Msg::Tick => {
            state.anim_ticks = state.anim_ticks.wrapping_add(1);
            state.expire_notifications(Instant::now());
            state.expire_status(Instant::now());
            match state.stage {
                Stage::Boot => {
                    // Boot lines are transient (viewport only) — not flushed. On finish
                    // they're replaced by the static help block (design: hide after done).
                    if state.boot_step < splash::LOADING.len() {
                        state.boot_step += 1;
                    } else {
                        state.boot_hold_ticks += 1;
                    }
                    if state.boot_hold_ticks >= BOOT_DONE_HOLD_TICKS {
                        finish_boot(state);
                    }
                    state.dirty = true;
                }
                Stage::Ready => {
                    if state.live.active || state.model_test_running() {
                        state.spinner_frame = state.spinner_frame.wrapping_add(1);
                        state.dirty = true;
                    }
                }
                Stage::Exit => {}
            }
        }
        Msg::Resize(w, h) => state.resize(w, h),
        Msg::Mouse(m) => on_mouse(state, m, session),
        Msg::Key(k) => on_key(state, k, session),
        Msg::Paste(text) => on_paste(state, text),
        Msg::Stream(ev) => on_stream(state, ev, session),
        Msg::Bus(CoreBusEvent::MailboxChanged) => {
            state.live.mailbox = session.mailbox();
            state.dirty = true;
        }
        Msg::FileScan {
            generation,
            entries,
        } => apply_file_scan(state, generation, entries),
        Msg::TestResult { model, result } => {
            if let Some(OverlayState::Fullscreen(FullscreenOverlay::Models(overlay))) =
                &mut state.overlay
            {
                if overlay
                    .test
                    .as_ref()
                    .is_some_and(|test| test.model == model)
                {
                    overlay.test = Some(ModelTest { model, result });
                    state.dirty = true;
                }
            }
        }
    }
}

/// Wheel notch → this many list/scroll steps in the focused overlay.
const WHEEL_STEP: usize = 3;

/// Overlay-only mouse support: wheel input reuses each overlay's existing key
/// handling. Click and drag remain unhandled by the application.
fn on_mouse(state: &mut AppState, m: MouseEvent, session: &dyn CoreSession) {
    if !state.overlay_is_fullscreen() {
        return;
    }
    let key = match m.kind {
        MouseEventKind::ScrollUp => KeyCode::Up,
        MouseEventKind::ScrollDown => KeyCode::Down,
        _ => return,
    };
    for _ in 0..WHEEL_STEP {
        handle_fullscreen_key(state, KeyEvent::new(key, KeyModifiers::empty()), session);
    }
    state.dirty = true;
}

fn finish_boot(state: &mut AppState) {
    state.stage = Stage::Ready;
    state.help_shown = true; // show the static help block until first submit
    state.dirty = true;
}

fn on_key(state: &mut AppState, k: KeyEvent, session: &dyn CoreSession) {
    if k.kind == KeyEventKind::Release {
        return;
    }
    if k.code == KeyCode::Esc {
        state.esc_anchored_at = Some(Instant::now());
    }
    if matches!(k.code, KeyCode::Char(_)) && k.kind == KeyEventKind::Press {
        if k.modifiers.contains(KeyModifiers::ALT) {
            return;
        }
        let now = Instant::now();
        state.key_bursts.push_back(now);
        while state
            .key_bursts
            .front()
            .is_some_and(|t| now.duration_since(*t) > Duration::from_millis(30))
        {
            state.key_bursts.pop_front();
        }
        let esc_anchored = state
            .esc_anchored_at
            .is_some_and(|t| now.duration_since(t) <= Duration::from_millis(100));
        if state.key_bursts.len() > 2 && esc_anchored {
            return;
        }
    }
    if matches!(k.code, KeyCode::Char(_))
        && state.overlay.is_none()
        && Instant::now() < state.overlay_char_guard_until
    {
        return;
    }
    // Any key during the boot animation skips straight to Ready, then handles the key.
    if state.stage == Stage::Boot {
        finish_boot(state);
    }
    if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
        let now = Instant::now();
        let confirmed = state
            .quit_armed_at
            .is_some_and(|t| now.duration_since(t) < Duration::from_secs(2));
        if confirmed {
            state.quit_armed_at = None;
            state.esc_count = 0;
            state.clear_overlay();
            state.should_quit = true;
            state.dirty = true;
            return;
        }
        state.quit_armed_at = Some(now);
        state.dirty = true;
        return;
    }
    if k.code != KeyCode::Esc {
        state.esc_count = 0;
    }

    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let shift = k.modifiers.contains(KeyModifiers::SHIFT);

    if state.overlay_is_fullscreen() {
        handle_fullscreen_key(state, k, session);
        return;
    }
    if state.pending_interaction.is_some() {
        handle_interaction_key(state, k, session);
        return;
    }

    if state.mode == Mode::Streaming && ctrl && k.code == KeyCode::Char('t') {
        state.streaming_send_target = match state.streaming_send_target {
            StreamingSendTarget::Mailbox => StreamingSendTarget::NextTurn,
            StreamingSendTarget::NextTurn => StreamingSendTarget::Mailbox,
        };
        state.set_status(state.i18n.t(match state.streaming_send_target {
            StreamingSendTarget::Mailbox => "status-send-target-mailbox",
            StreamingSendTarget::NextTurn => "status-send-target-next",
        }));
        state.dirty = true;
        return;
    }
    if state.mode == Mode::Streaming && ctrl && k.code == KeyCode::Char('z') {
        undo_streaming_queue(state, session);
        return;
    }

    match k.code {
        KeyCode::Up if state.is_file_picker_open() => {
            move_file_picker(state, -1);
        }
        KeyCode::Down if state.is_file_picker_open() => {
            move_file_picker(state, 1);
        }
        KeyCode::Tab if state.is_file_picker_open() => {
            accept_file_picker(state);
        }
        KeyCode::Enter if state.is_file_picker_open() => {
            if !enter_file_picker(state) {
                submit(state, session);
            }
        }
        KeyCode::Up if state.is_command_suggest_open() => {
            move_suggest(state, -1);
        }
        KeyCode::Down if state.is_command_suggest_open() => {
            move_suggest(state, 1);
        }
        KeyCode::Up => {
            browse_input_history(state, -1);
        }
        KeyCode::Down => {
            browse_input_history(state, 1);
        }
        // Caret movement inside the composer. ↑/↓ stay on input history, so
        // horizontal movement is the only way to reposition the caret.
        KeyCode::Left => {
            reset_input_history_cursor(state);
            state.history_replay = None;
            state.input_cursor = state.input_cursor.saturating_sub(1);
            state.dirty = true;
        }
        KeyCode::Right => {
            reset_input_history_cursor(state);
            state.history_replay = None;
            let len = state.input.chars().count();
            state.input_cursor = (state.input_cursor + 1).min(len);
            state.dirty = true;
        }
        KeyCode::Home => {
            reset_input_history_cursor(state);
            state.history_replay = None;
            state.input_cursor = 0;
            state.dirty = true;
        }
        KeyCode::End => {
            reset_input_history_cursor(state);
            state.history_replay = None;
            state.input_cursor = state.input.chars().count();
            state.dirty = true;
        }
        KeyCode::Tab if state.is_command_suggest_open() => {
            accept_suggest(state, session);
        }
        // Newline, not submit: Shift+Enter (terminals that report it), or a trailing `\`
        // (reliable fallback — many terminals collapse Shift+Enter to plain Enter).
        KeyCode::Enter if shift => newline(state),
        KeyCode::Enter if state.input.ends_with('\\') => {
            state.input.pop(); // drop the continuation backslash
            cursor_to_end(state);
            newline(state);
        }
        KeyCode::Enter if state.is_command_suggest_open() => {
            if !submit_suggest(state, session) {
                submit(state, session);
            }
        }
        KeyCode::Enter => submit(state, session),

        // Ctrl+O = zoom the last assistant reply in the fullscreen viewer.
        KeyCode::Char('o') if ctrl => open_last_reply_zoom(state),
        // Ctrl+J = newline; Ctrl+U = clear input.
        KeyCode::Char('j') if ctrl => newline(state),
        KeyCode::Char('u') if ctrl => {
            state.input.clear();
            state.input_cursor = 0;
            state.chips.clear();
            state.history_replay = None;
            state.clear_overlay();
            state.dirty = true;
        }
        // Ignore other Ctrl combos so they don't type literal control chars.
        KeyCode::Char(_) if ctrl => {}

        KeyCode::Backspace => {
            reset_input_history_cursor(state);
            state.history_replay = None;
            if state.input_cursor < state.input.chars().count() {
                // Caret is mid-text: delete the char BEFORE it. Chips sit at the
                // tail only, so the block-removal paths below stay end-of-input only.
                if state.input_cursor > 0 {
                    let at = char_to_byte(&state.input, state.input_cursor - 1);
                    state.input.remove(at);
                    state.input_cursor -= 1;
                    prune_detached_chips(state);
                }
            } else if !remove_trailing_chip(state) && !remove_file_picker_directory(state) {
                state.input.pop();
                cursor_to_end(state);
                prune_detached_chips(state);
            }
            refresh_suggest(state, session);
            refresh_file_picker(state);
            state.dirty = true;
        }
        KeyCode::Delete => {
            reset_input_history_cursor(state);
            state.history_replay = None;
            let len = state.input.chars().count();
            if state.input_cursor < len {
                let at = char_to_byte(&state.input, state.input_cursor);
                state.input.remove(at);
                prune_detached_chips(state);
            }
            refresh_suggest(state, session);
            refresh_file_picker(state);
            state.dirty = true;
        }
        KeyCode::Char(c) => {
            reset_input_history_cursor(state);
            state.history_replay = None;
            let at = char_to_byte(&state.input, state.input_cursor);
            state.input.insert(at, c);
            state.input_cursor += 1;
            refresh_suggest(state, session);
            refresh_file_picker(state);
            state.dirty = true;
        }
        KeyCode::Esc if state.overlay_is_floating() => {
            state.clear_status();
            state.clear_overlay();
            state.clear_notifications();
            state.dirty = true;
        }
        KeyCode::Esc if state.mode == Mode::Streaming => on_streaming_esc(state, session),
        KeyCode::Esc => {
            state.clear_status();
            state.clear_overlay();
            state.clear_notifications();
            state.dirty = true;
        }
        _ => {}
    }
}

fn on_streaming_esc(state: &mut AppState, session: &dyn CoreSession) {
    state.esc_count += 1;
    if state.esc_count < 2 {
        state.set_status(state.i18n.t("status-esc-cancel-again"));
        return;
    }
    state.cancel_requested = true;
    state.esc_count = 0;
    state.set_status(state.i18n.t("status-canceling"));
    session.send(Command::TurnCancel);
}

fn handle_interaction_key(state: &mut AppState, k: KeyEvent, session: &dyn CoreSession) {
    let Some(pending) = state.pending_interaction.take() else {
        return;
    };
    match pending {
        PendingInteraction::Permission {
            id,
            action,
            target,
            reason,
            risk,
            mut selected,
        } => match k.code {
            KeyCode::Up => {
                selected = selected.saturating_sub(1);
                state.pending_interaction = Some(PendingInteraction::Permission {
                    id,
                    action,
                    target,
                    reason,
                    risk,
                    selected,
                });
                state.dirty = true;
            }
            KeyCode::Down => {
                selected = (selected + 1).min(2);
                state.pending_interaction = Some(PendingInteraction::Permission {
                    id,
                    action,
                    target,
                    reason,
                    risk,
                    selected,
                });
                state.dirty = true;
            }
            KeyCode::Enter if selected == 0 => {
                session.send(Command::Respond {
                    id,
                    answer: Answer::Allow {
                        scope: GrantScope::Once,
                    },
                });
                let status = state.i18n.t("status-interaction-allowed-once");
                resolve_interaction(state, status);
            }
            KeyCode::Enter if selected == 1 => {
                session.send(Command::Respond {
                    id,
                    answer: Answer::Allow {
                        scope: GrantScope::Session,
                    },
                });
                let status = state.i18n.t("status-interaction-allowed-session");
                resolve_interaction(state, status);
            }
            KeyCode::Enter | KeyCode::Esc => {
                session.send(Command::Respond {
                    id,
                    answer: Answer::Deny { message: None },
                });
                let status = state.i18n.t("status-interaction-denied");
                resolve_interaction(state, status);
            }
            _ => {
                state.pending_interaction = Some(PendingInteraction::Permission {
                    id,
                    action,
                    target,
                    reason,
                    risk,
                    selected,
                });
            }
        },
        PendingInteraction::Confirmation {
            id,
            message,
            mut selected,
        } => match k.code {
            KeyCode::Up | KeyCode::Down => {
                selected = if selected == 0 { 1 } else { 0 };
                state.pending_interaction = Some(PendingInteraction::Confirmation {
                    id,
                    message,
                    selected,
                });
                state.dirty = true;
            }
            KeyCode::Enter if selected == 1 => {
                session.send(Command::Respond {
                    id,
                    answer: Answer::Allow {
                        scope: GrantScope::Once,
                    },
                });
                let status = state.i18n.t("status-interaction-confirmed");
                resolve_interaction(state, status);
            }
            KeyCode::Enter | KeyCode::Esc => {
                session.send(Command::Respond {
                    id,
                    answer: Answer::Deny { message: None },
                });
                let status = state.i18n.t("status-interaction-confirm-canceled");
                resolve_interaction(state, status);
            }
            _ => {
                state.pending_interaction = Some(PendingInteraction::Confirmation {
                    id,
                    message,
                    selected,
                });
            }
        },
        PendingInteraction::Form {
            id,
            title,
            mut items,
            mut current,
        } => match k.code {
            // Enter = confirm current field, move on; on the LAST field it submits.
            KeyCode::Enter => {
                if advance_form_focus(&mut items, &mut current) {
                    restore_form(state, id, title, items, current);
                } else {
                    let values = form_values(&items);
                    session.send(Command::Respond {
                        id,
                        answer: Answer::Form { values },
                    });
                    let status = state.i18n.t("status-interaction-input-sent");
                    resolve_interaction(state, status);
                }
            }
            // Esc is two-step so a stray press can't discard typed values. on_key resets
            // esc_count on any non-Esc key, so the arm only survives consecutive Escs.
            KeyCode::Esc => {
                state.esc_count += 1;
                if state.esc_count >= 2 {
                    state.esc_count = 0;
                    session.send(Command::Respond {
                        id,
                        answer: Answer::Form {
                            values: serde_json::Map::new(),
                        },
                    });
                    let status = state.i18n.t("status-interaction-input-skipped");
                    resolve_interaction(state, status);
                } else {
                    state.set_status(state.i18n.t("status-form-skip-again"));
                    restore_form(state, id, title, items, current);
                }
            }
            // One continuous column: ↑/↓ walk options inside a focused select /
            // multi-select and cross over to the previous/next field at the edges;
            // on text / checkbox fields they move field focus directly.
            KeyCode::Up => {
                form_nav_vertical(&mut items, &mut current, -1);
                restore_form(state, id, title, items, current);
            }
            KeyCode::Down => {
                form_nav_vertical(&mut items, &mut current, 1);
                restore_form(state, id, title, items, current);
            }
            KeyCode::Tab => {
                advance_form_focus(&mut items, &mut current);
                restore_form(state, id, title, items, current);
            }
            KeyCode::BackTab => {
                retreat_form_focus(&mut items, &mut current);
                restore_form(state, id, title, items, current);
            }
            // ←/→: text caret movement; inline value cycling for select / checkbox.
            KeyCode::Left => {
                edit_current_field(&mut items, current, FormEdit::Left);
                restore_form(state, id, title, items, current);
            }
            KeyCode::Right => {
                edit_current_field(&mut items, current, FormEdit::Right);
                restore_form(state, id, title, items, current);
            }
            KeyCode::Home => {
                edit_current_field(&mut items, current, FormEdit::Home);
                restore_form(state, id, title, items, current);
            }
            KeyCode::End => {
                edit_current_field(&mut items, current, FormEdit::End);
                restore_form(state, id, title, items, current);
            }
            KeyCode::Backspace => {
                edit_current_field(&mut items, current, FormEdit::Backspace);
                restore_form(state, id, title, items, current);
            }
            KeyCode::Delete => {
                edit_current_field(&mut items, current, FormEdit::Delete);
                restore_form(state, id, title, items, current);
            }
            KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                edit_current_field(&mut items, current, FormEdit::Clear);
                restore_form(state, id, title, items, current);
            }
            KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                // Space toggles checkbox / multi-select; it types into text fields.
                if c == ' ' && toggle_current_choice(&mut items, current) {
                    restore_form(state, id, title, items, current);
                    return;
                }
                edit_current_field(&mut items, current, FormEdit::Char(c));
                restore_form(state, id, title, items, current);
            }
            _ => {
                restore_form(state, id, title, items, current);
            }
        },
    }
}

enum FormEdit {
    Char(char),
    Backspace,
    Delete,
    Left,
    Right,
    Home,
    End,
    Clear,
}

fn restore_form(
    state: &mut AppState,
    id: String,
    title: String,
    items: Vec<FormItem>,
    current: usize,
) {
    state.pending_interaction = Some(PendingInteraction::Form {
        id,
        title,
        items,
        current,
    });
    state.dirty = true;
}

fn current_field_mut(items: &mut [FormItem], current: usize) -> Option<&mut FormField> {
    let item = items.get_mut(current)?;
    item.fields.get_mut(item.focus)
}

/// Byte offset of the char-index `cursor` inside `s`.
fn char_to_byte(s: &str, cursor: usize) -> usize {
    s.char_indices()
        .nth(cursor)
        .map(|(idx, _)| idx)
        .unwrap_or(s.len())
}

/// Snap the composer caret to the end of the input. Every wholesale input
/// replacement (history recall, suggest accept, paste…) lands the caret at the
/// end, matching the pre-caret behavior.
fn cursor_to_end(state: &mut AppState) {
    state.input_cursor = state.input.chars().count();
}

/// Apply an edit to the focused field. Text fields get real caret editing; select /
/// checkbox map ←/→ to inline value cycling (no open/close modal state).
fn edit_current_field(items: &mut [FormItem], current: usize, edit: FormEdit) {
    let Some(field) = current_field_mut(items, current) else {
        return;
    };
    match &mut field.value {
        FormValue::Text { value, cursor, .. } => {
            let chars = value.chars().count();
            let at = (*cursor).min(chars);
            match edit {
                FormEdit::Char(c) => {
                    value.insert(char_to_byte(value, at), c);
                    *cursor = at + 1;
                }
                FormEdit::Backspace => {
                    if at > 0 {
                        value.remove(char_to_byte(value, at - 1));
                        *cursor = at - 1;
                    }
                }
                FormEdit::Delete => {
                    if at < chars {
                        value.remove(char_to_byte(value, at));
                        *cursor = at;
                    }
                }
                FormEdit::Left => *cursor = at.saturating_sub(1),
                FormEdit::Right => *cursor = (at + 1).min(chars),
                FormEdit::Home => *cursor = 0,
                FormEdit::End => *cursor = chars,
                FormEdit::Clear => {
                    value.clear();
                    *cursor = 0;
                }
            }
        }
        FormValue::Checkbox { checked } => {
            if matches!(edit, FormEdit::Left | FormEdit::Right) {
                *checked = !*checked;
            }
        }
        FormValue::Select { options, selected } => match edit {
            FormEdit::Left => *selected = selected.saturating_sub(1),
            FormEdit::Right => *selected = (*selected + 1).min(options.len().saturating_sub(1)),
            _ => {}
        },
        FormValue::MultiSelect { .. } => {}
    }
}

/// Space on a checkbox / multi-select toggles it. Returns false for text fields so the
/// caller types the space instead.
fn toggle_current_choice(items: &mut [FormItem], current: usize) -> bool {
    let Some(field) = current_field_mut(items, current) else {
        return false;
    };
    match &mut field.value {
        FormValue::Checkbox { checked } => {
            *checked = !*checked;
            true
        }
        FormValue::MultiSelect {
            selected, cursor, ..
        } => {
            if let Some(pos) = selected.iter().position(|idx| idx == cursor) {
                selected.remove(pos);
            } else {
                selected.push(*cursor);
                selected.sort_unstable();
            }
            true
        }
        FormValue::Select { .. } => true, // selection is direct; swallow the space
        FormValue::Text { .. } => false,
    }
}

/// ↑/↓ as one continuous column: inside a focused select / multi-select the cursor
/// walks the options and crosses over to the previous/next field at the list edges;
/// text / checkbox fields move field focus directly.
fn form_nav_vertical(items: &mut [FormItem], current: &mut usize, dir: i32) {
    let inner_moved = current_field_mut(items, *current).is_some_and(|field| {
        let (cursor, len) = match &mut field.value {
            FormValue::Select { options, selected } => (selected, options.len()),
            FormValue::MultiSelect {
                options, cursor, ..
            } => (cursor, options.len()),
            _ => return false,
        };
        if dir < 0 && *cursor > 0 {
            *cursor -= 1;
            true
        } else if dir > 0 && *cursor + 1 < len {
            *cursor += 1;
            true
        } else {
            false
        }
    });
    if !inner_moved {
        if dir < 0 {
            retreat_form_focus(items, current);
        } else {
            advance_form_focus(items, current);
        }
    }
}

fn advance_form_focus(items: &mut [FormItem], current: &mut usize) -> bool {
    let Some(item) = items.get_mut(*current) else {
        return false;
    };
    if item.focus + 1 < item.fields.len() {
        item.focus += 1;
        return true;
    }
    if *current + 1 < items.len() {
        *current += 1;
        return true;
    }
    false
}

fn retreat_form_focus(items: &mut [FormItem], current: &mut usize) -> bool {
    let Some(item) = items.get_mut(*current) else {
        return false;
    };
    if item.focus > 0 {
        item.focus -= 1;
        return true;
    }
    if *current > 0 {
        *current -= 1;
        if let Some(prev) = items.get_mut(*current) {
            prev.focus = prev.fields.len().saturating_sub(1);
        }
        return true;
    }
    false
}

fn form_values(items: &[FormItem]) -> serde_json::Map<String, serde_json::Value> {
    let mut values = serde_json::Map::new();
    if items.len() != 1 {
        values.insert(
            "items".into(),
            serde_json::Value::Array(
                items
                    .iter()
                    .map(|item| serde_json::Value::Object(form_item_values(item)))
                    .collect(),
            ),
        );
        return values;
    }
    let Some(item) = items.first() else {
        return values;
    };
    form_item_values(item)
}

fn form_item_values(item: &FormItem) -> serde_json::Map<String, serde_json::Value> {
    let mut values = serde_json::Map::new();
    for field in &item.fields {
        let value = match &field.value {
            FormValue::Text { value, .. } => serde_json::Value::String(value.clone()),
            FormValue::Checkbox { checked } => serde_json::Value::Bool(*checked),
            FormValue::Select { options, selected } => options
                .get(*selected)
                .cloned()
                .map(serde_json::Value::String)
                .unwrap_or(serde_json::Value::Null),
            FormValue::MultiSelect {
                options, selected, ..
            } => serde_json::Value::Array(
                selected
                    .iter()
                    .filter_map(|idx| options.get(*idx))
                    .cloned()
                    .map(serde_json::Value::String)
                    .collect(),
            ),
        };
        values.insert(field.name.clone(), value);
    }
    values
}

fn resolve_interaction(state: &mut AppState, status: impl Into<String>) {
    state
        .notifications
        .retain(|notification| !notification.sticky);
    state.input_before_interaction = None;
    state.set_status(status);
}

fn handle_fullscreen_key(state: &mut AppState, k: KeyEvent, session: &dyn CoreSession) {
    match state.fullscreen_overlay() {
        Some(super::FullscreenOverlay::Help(_)) => handle_help_key(state, k, session),
        Some(super::FullscreenOverlay::Models(_)) => handle_models_key(state, k, session),
        Some(super::FullscreenOverlay::Stats(_)) => handle_stats_key(state, k),
        Some(super::FullscreenOverlay::Sessions(_)) => handle_sessions_key(state, k, session),
        Some(super::FullscreenOverlay::Workspaces(_)) => handle_workspaces_key(state, k, session),
        Some(super::FullscreenOverlay::Zoom(_)) => handle_zoom_key(state, k),
        Some(super::FullscreenOverlay::Replay(_)) => handle_replay_key(state, k, session),
        _ => match k.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                state.clear_overlay();
                state.dirty = true;
            }
            _ => {}
        },
    }
}

/// Zoom viewer keys: ↑/↓ line, PgUp/PgDn page, Home/End top/bottom, Esc/q close.
/// Document viewers additionally accept g/G, and Ctrl+O's latest-reply viewer accepts
/// `c` to copy its raw Markdown. Compact session info deliberately does not. Scroll is
/// clamped against the page size derived from `term_rows` — the
/// renderer clamps again against its actual area, so the two can never disagree
/// visibly.
fn handle_zoom_key(state: &mut AppState, k: KeyEvent) {
    if matches!(k.code, KeyCode::Esc | KeyCode::Char('q')) {
        let Some(super::OverlayState::Fullscreen(super::FullscreenOverlay::Zoom(zoom))) =
            state.overlay.take()
        else {
            return;
        };
        // Put back whatever the viewer replaced (e.g. the sessions overlay).
        state.overlay = zoom
            .restore
            .map(|overlay| super::OverlayState::Fullscreen(*overlay));
        state.dirty = true;
        return;
    }
    if k.code == KeyCode::Char('c') {
        let copy_text = match state.fullscreen_overlay() {
            Some(super::FullscreenOverlay::Zoom(zoom)) => zoom.copy_text.clone(),
            _ => None,
        };
        let Some(copy_text) = copy_text else {
            return;
        };
        let copied = crate::clipboard::write_text(&copy_text);
        let feedback = if copied {
            let mut args = fluent_bundle::FluentArgs::new();
            args.set("chars", copy_text.chars().count() as i64);
            state.i18n.format("zoom-copy-success", Some(&args))
        } else {
            state.i18n.t("zoom-copy-failed")
        };
        if let Some(super::FullscreenOverlay::Zoom(zoom)) = state.fullscreen_overlay_mut() {
            zoom.copy_feedback = Some(feedback);
        }
        state.dirty = true;
        return;
    }
    let page = zoom_content_rows(state.term_rows);
    let Some(super::FullscreenOverlay::Zoom(zoom)) = state.fullscreen_overlay_mut() else {
        return;
    };
    let max_scroll = zoom.lines.len().saturating_sub(page);
    zoom.scroll = match k.code {
        KeyCode::Up => zoom.scroll.saturating_sub(1),
        KeyCode::Down => zoom.scroll.saturating_add(1).min(max_scroll),
        KeyCode::PageUp => zoom.scroll.saturating_sub(page),
        KeyCode::PageDown => zoom.scroll.saturating_add(page).min(max_scroll),
        KeyCode::Char('g') if zoom.kind == ZoomKind::Document => 0,
        KeyCode::Char('G') if zoom.kind == ZoomKind::Document => max_scroll,
        KeyCode::Home => 0,
        KeyCode::End => max_scroll,
        _ => return,
    };
    state.dirty = true;
}

/// Markdown → display lines for the zoom viewer, wrapped at OPEN time against the
/// viewer's content width (full screen minus borders + padding). Pre-wrapping keeps
/// the scroll/percent math exact; the trade-off (stale wrap after a resize until
/// reopened) is documented on `ZoomOverlay`.
fn zoom_markdown_lines(state: &AppState, text: &str) -> Vec<Line<'static>> {
    let width = (state.width as usize).saturating_sub(4).max(10);
    block::render_markdown(text, &state.theme, state.icons)
        .into_iter()
        .flat_map(|line| wrap_hist_line(&HistLine::from_line(line), width, width))
        .collect()
}

/// Ctrl+O on the main screen: zoom the last settled assistant reply. The source is
/// the RAW reply text retained in `AppState::last_reply` (the transcript only keeps
/// rendered lines), re-rendered through the normal markdown pipeline.
fn open_last_reply_zoom(state: &mut AppState) {
    let Some(reply) = state.last_reply.clone() else {
        state.set_status(state.i18n.t("status-zoom-empty"));
        state.dirty = true;
        return;
    };
    let zoom = ZoomOverlay {
        title: state.i18n.t("zoom-title-last-reply"),
        lines: zoom_markdown_lines(state, &reply),
        scroll: 0,
        kind: ZoomKind::Document,
        copy_text: Some(reply),
        copy_feedback: None,
        restore: None,
    };
    state.overlay = Some(super::OverlayState::Fullscreen(
        super::FullscreenOverlay::Zoom(zoom),
    ));
    state.dirty = true;
}

/// Build a zoom viewer for the selected history entry of the sessions overlay
/// (history pane `v`). The caller stashes the sessions overlay in `restore`.
fn history_entry_zoom(state: &AppState, overlay: &SessionsOverlay) -> Option<ZoomOverlay> {
    let line = overlay.history.get(overlay.history_selected)?;
    let role = match line {
        HistoryItem::Message {
            role: MessageRole::User,
            ..
        } => state.i18n.t("sessions-history-user"),
        HistoryItem::Message {
            role: MessageRole::Assistant,
            ..
        } => state.i18n.t("sessions-history-assistant"),
        HistoryItem::Event { .. } => "event".into(),
    };
    let mut args = fluent_bundle::FluentArgs::new();
    args.set("turn", line.turn_index() as i64);
    args.set("role", role);
    Some(ZoomOverlay {
        title: state.i18n.format("zoom-title-history", Some(&args)),
        lines: zoom_markdown_lines(state, &line.preview()),
        scroll: 0,
        kind: ZoomKind::Document,
        copy_text: None,
        copy_feedback: None,
        restore: None,
    })
}

fn handle_help_key(state: &mut AppState, k: KeyEvent, session: &dyn CoreSession) {
    if matches!(k.code, KeyCode::Esc | KeyCode::Char('q')) {
        state.clear_overlay();
        state.dirty = true;
        return;
    }
    // Enter runs the highlighted command. Built-ins execute in place (open an overlay,
    // toggle a mode…); skills usually need arguments, so we close help and drop the
    // token into the composer for the user to complete — same split as accept_suggest.
    if k.code == KeyCode::Enter {
        let selected = match state.fullscreen_overlay() {
            Some(super::FullscreenOverlay::Help(help)) => help.commands.get(help.selected).cloned(),
            _ => None,
        };
        let Some(cmd) = selected else {
            return;
        };
        state.clear_overlay();
        match cmd.kind {
            CommandKind::Builtin => {
                if builtin_has_options(&cmd.name) {
                    state.input = format!("/{} ", cmd.name);
                    cursor_to_end(state);
                    refresh_suggest(state, session);
                } else {
                    let text = format!("/{}", cmd.name);
                    record_input_history(state, builtin_command_messages(&text), text.clone());
                    handle_builtin_command(state, &text, session);
                }
            }
            CommandKind::Skill => {
                state.input = format!("/{} ", cmd.name);
                cursor_to_end(state);
                state.chips.clear();
                state.history_replay = None;
            }
        }
        state.dirty = true;
        return;
    }
    let Some(super::FullscreenOverlay::Help(help)) = state.fullscreen_overlay_mut() else {
        return;
    };
    let max_scroll = help.commands.len().saturating_sub(1);
    let next = match k.code {
        KeyCode::Up => help.selected.saturating_sub(1),
        KeyCode::Down => help.selected.saturating_add(1).min(max_scroll),
        KeyCode::PageUp => help.selected.saturating_sub(5),
        KeyCode::PageDown => help.selected.saturating_add(5).min(max_scroll),
        KeyCode::Home => 0,
        KeyCode::End => max_scroll,
        _ => return,
    };
    help.selected = next;
    state.dirty = true;
}

fn handle_stats_key(state: &mut AppState, k: KeyEvent) {
    match k.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            state.clear_overlay();
            state.dirty = true;
            return;
        }
        _ => {}
    }
    let viewport = (state.term_rows as usize).saturating_sub(7);
    let page = viewport.saturating_sub(1).max(1);
    let Some(super::FullscreenOverlay::Stats(stats)) = state.fullscreen_overlay_mut() else {
        return;
    };
    // The stats body has 11 fixed/table-chrome rows, plus nine chart rows when
    // core supplied a timeline. Model rows are the only unbounded section.
    let content_rows = 11 + stats.by_model.len() + if stats.timeline.is_empty() { 0 } else { 9 };
    let max_scroll = content_rows.saturating_sub(viewport);
    let shift = k.modifiers.contains(KeyModifiers::SHIFT);
    match k.code {
        KeyCode::Right | KeyCode::Tab if !shift => {
            stats.range = stats.range.next();
            stats.scroll = 0;
        }
        KeyCode::Left | KeyCode::BackTab => {
            stats.range = stats.range.prev();
            stats.scroll = 0;
        }
        KeyCode::Tab if shift => {
            stats.range = stats.range.prev();
            stats.scroll = 0;
        }
        KeyCode::Up => stats.scroll = stats.scroll.saturating_sub(1),
        KeyCode::Down => stats.scroll = stats.scroll.saturating_add(1).min(max_scroll),
        KeyCode::PageUp => stats.scroll = stats.scroll.saturating_sub(page),
        KeyCode::PageDown => stats.scroll = stats.scroll.saturating_add(page).min(max_scroll),
        KeyCode::Home | KeyCode::Char('g') => stats.scroll = 0,
        KeyCode::End | KeyCode::Char('G') => stats.scroll = max_scroll,
        _ => return,
    }
    state.dirty = true;
}

fn handle_sessions_key(state: &mut AppState, k: KeyEvent, session: &dyn CoreSession) {
    let Some(super::OverlayState::Fullscreen(super::FullscreenOverlay::Sessions(mut overlay))) =
        state.overlay.take()
    else {
        return;
    };

    let mut close = false;
    let mut zoom = None;
    if let Some(confirm) = overlay.confirm.take() {
        handle_confirm_key(state, session, &mut overlay, confirm, k, &mut close);
    } else if overlay.renaming.is_some() {
        handle_session_rename_key(state, session, &mut overlay, k);
    } else if overlay.searching {
        handle_session_search_key(state, session, &mut overlay, k);
    } else {
        zoom = handle_session_list_key(state, session, &mut overlay, k, &mut close);
    }

    if let Some(mut zoom) = zoom {
        // The viewer REPLACES the sessions overlay (single-overlay model, no
        // stack); Esc/q in the viewer puts it back exactly as stashed here.
        zoom.restore = Some(Box::new(super::FullscreenOverlay::Sessions(overlay)));
        state.overlay = Some(super::OverlayState::Fullscreen(
            super::FullscreenOverlay::Zoom(zoom),
        ));
    } else if close {
        state.clear_overlay();
    } else {
        state.overlay = Some(super::OverlayState::Fullscreen(
            super::FullscreenOverlay::Sessions(overlay),
        ));
    }
    state.dirty = true;
}

fn handle_workspaces_key(state: &mut AppState, k: KeyEvent, session: &dyn CoreSession) {
    let Some(super::OverlayState::Fullscreen(super::FullscreenOverlay::Workspaces(mut overlay))) =
        state.overlay.take()
    else {
        return;
    };

    let mut close = false;
    if let Some(confirm) = overlay.confirm.take() {
        handle_workspaces_confirm_key(state, session, &mut overlay, confirm, k, &mut close);
    } else {
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => close = true,
            KeyCode::Right | KeyCode::Tab => {
                overlay.focus = super::WorkspacePane::Right;
                overlay.session_selected = 0;
            }
            KeyCode::Left => overlay.focus = super::WorkspacePane::Left,
            KeyCode::Up => {
                let ssel = overlay.session_selected;
                if overlay.focus == super::WorkspacePane::Left {
                    select_prev_workspace(session, &mut overlay);
                } else {
                    move_workspace_session(&mut overlay, ssel.saturating_sub(1));
                }
            }
            KeyCode::Down => {
                let ssel = overlay.session_selected;
                if overlay.focus == super::WorkspacePane::Left {
                    select_next_workspace(session, &mut overlay);
                } else {
                    move_workspace_session(&mut overlay, ssel + 1);
                }
            }
            KeyCode::PageUp => {
                let sel = overlay.selected;
                let ssel = overlay.session_selected;
                if overlay.focus == super::WorkspacePane::Left {
                    jump_workspace(session, &mut overlay, sel.saturating_sub(10));
                } else {
                    move_workspace_session(&mut overlay, ssel.saturating_sub(10));
                }
            }
            KeyCode::PageDown => {
                let sel = overlay.selected;
                let ssel = overlay.session_selected;
                if overlay.focus == super::WorkspacePane::Left {
                    jump_workspace(session, &mut overlay, sel + 10);
                } else {
                    move_workspace_session(&mut overlay, ssel + 10);
                }
            }
            KeyCode::Home => {
                if overlay.focus == super::WorkspacePane::Left {
                    jump_workspace(session, &mut overlay, 0);
                } else {
                    move_workspace_session(&mut overlay, 0);
                }
            }
            KeyCode::End => {
                if overlay.focus == super::WorkspacePane::Left {
                    jump_workspace(session, &mut overlay, usize::MAX);
                } else {
                    move_workspace_session(&mut overlay, usize::MAX);
                }
            }
            KeyCode::Enter => open_workspace_switch_confirm(state, session, &mut overlay),
            _ => {}
        }
    }

    if close {
        state.clear_overlay();
    } else {
        state.overlay = Some(super::OverlayState::Fullscreen(
            super::FullscreenOverlay::Workspaces(overlay),
        ));
    }
    state.dirty = true;
}

fn jump_workspace(session: &dyn CoreSession, overlay: &mut WorkspacesOverlay, to: usize) {
    let next = to.min(overlay.items.len().saturating_sub(1));
    if next != overlay.selected {
        overlay.selected = next;
        refresh_workspace_sessions(session, overlay);
        overlay.session_selected = 0;
    }
}

fn select_prev_workspace(session: &dyn CoreSession, overlay: &mut WorkspacesOverlay) {
    let next = overlay.selected.saturating_sub(1);
    jump_workspace(session, overlay, next);
}

fn select_next_workspace(session: &dyn CoreSession, overlay: &mut WorkspacesOverlay) {
    jump_workspace(session, overlay, overlay.selected + 1);
}

fn move_workspace_session(overlay: &mut WorkspacesOverlay, to: usize) {
    overlay.session_selected = to.min(overlay.sessions.len());
}

fn refresh_workspace_sessions(session: &dyn CoreSession, overlay: &mut WorkspacesOverlay) {
    let Some(ws) = overlay.items.get(overlay.selected) else {
        overlay.sessions.clear();
        return;
    };
    overlay.sessions = session.workspace_sessions(&ws.workspace_id.to_string());
}

fn open_workspace_switch_confirm(
    state: &mut AppState,
    session: &dyn CoreSession,
    overlay: &mut WorkspacesOverlay,
) {
    let Some(ws) = overlay.items.get(overlay.selected) else {
        return;
    };
    let workspace_id = ws.workspace_id.to_string();
    if overlay.focus == super::WorkspacePane::Left {
        if workspace_id == session.current_workspace() {
            state.set_status(state.i18n.t("workspace-status-current"));
            state.dirty = true;
            return;
        }
        let resume_id = overlay.sessions.first().map(|s| s.id.clone());
        let mut args = fluent_bundle::FluentArgs::new();
        args.set("name", ws.name.as_str());
        let message = if resume_id.is_some() {
            state.i18n.format("workspace-switch-message", Some(&args))
        } else {
            state
                .i18n
                .format("workspace-new-session-message", Some(&args))
        };
        overlay.confirm = Some(ConfirmDialog {
            title: state.i18n.t("workspace-switch-title"),
            message,
            confirm_label: state.i18n.t("workspace-switch-confirm"),
            cancel_label: state.i18n.t("common-cancel"),
            danger: false,
            selected: 1,
            action: ConfirmAction::SwitchWorkspace {
                workspace_id,
                session_id: resume_id,
            },
        });
        return;
    }
    if overlay.session_selected == 0 {
        let mut args = fluent_bundle::FluentArgs::new();
        args.set("name", ws.name.as_str());
        overlay.confirm = Some(ConfirmDialog {
            title: state.i18n.t("workspace-new-session-title"),
            message: state
                .i18n
                .format("workspace-new-session-message", Some(&args)),
            confirm_label: state.i18n.t("workspace-switch-confirm"),
            cancel_label: state.i18n.t("common-cancel"),
            danger: false,
            selected: 1,
            action: ConfirmAction::SwitchWorkspace {
                workspace_id,
                session_id: None,
            },
        });
        return;
    }
    let Some(sess) = overlay.sessions.get(overlay.session_selected - 1) else {
        return;
    };
    let mut args = fluent_bundle::FluentArgs::new();
    args.set("workspace", ws.name.as_str());
    args.set("title", sess.title.as_str());
    overlay.confirm = Some(ConfirmDialog {
        title: state.i18n.t("workspace-session-switch-title"),
        message: state
            .i18n
            .format("workspace-session-switch-message", Some(&args)),
        confirm_label: state.i18n.t("workspace-switch-confirm"),
        cancel_label: state.i18n.t("common-cancel"),
        danger: false,
        selected: 1,
        action: ConfirmAction::SwitchWorkspace {
            workspace_id,
            session_id: Some(sess.id.clone()),
        },
    });
}

fn handle_workspaces_confirm_key(
    state: &mut AppState,
    session: &dyn CoreSession,
    overlay: &mut WorkspacesOverlay,
    mut confirm: ConfirmDialog,
    k: KeyEvent,
    close: &mut bool,
) {
    match k.code {
        KeyCode::Esc => {}
        KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
            confirm.selected = if confirm.selected == 0 { 1 } else { 0 };
            overlay.confirm = Some(confirm);
        }
        KeyCode::Enter => {
            if confirm.selected == 0 {
                return; // cancel: close only the dialog, the panel stays where it is
            }
            let ConfirmAction::SwitchWorkspace {
                workspace_id,
                session_id,
            } = confirm.action
            else {
                return;
            };
            if let Some(_new_id) = session.switch_workspace(&workspace_id, session_id.as_deref()) {
                state.reset_conversation_view();
                state.refresh_status_snapshot(session);
                let name = overlay
                    .items
                    .iter()
                    .find(|w| w.workspace_id.to_string() == workspace_id)
                    .map(|w| w.name.clone())
                    .unwrap_or_default();
                if let Some(session_id) = &session_id {
                    let title = overlay
                        .sessions
                        .iter()
                        .find(|s| &s.id == session_id)
                        .map(|s| s.title.clone())
                        .unwrap_or_default();
                    let mut args = fluent_bundle::FluentArgs::new();
                    args.set("workspace", name.as_str());
                    args.set("title", title.as_str());
                    state.status = state
                        .i18n
                        .format("workspace-status-session-switched", Some(&args));
                } else {
                    let mut args = fluent_bundle::FluentArgs::new();
                    args.set("name", name.as_str());
                    state.set_status(state.i18n.format("workspace-status-switched", Some(&args)));
                }
                *close = true;
            } else {
                state.set_status(state.i18n.t("workspace-status-switch-failed"));
            }
        }
        _ => {
            overlay.confirm = Some(confirm);
        }
    }
}

fn handle_session_search_key(
    state: &AppState,
    session: &dyn CoreSession,
    overlay: &mut SessionsOverlay,
    k: KeyEvent,
) {
    match k.code {
        KeyCode::Esc => overlay.searching = false,
        KeyCode::Enter => {
            if overlay.last_searched.as_deref() != Some(overlay.query.as_str()) {
                apply_session_search(session, overlay);
            } else {
                open_session_switch_confirm(state, overlay);
            }
        }
        KeyCode::Up => select_prev_session(session, overlay),
        KeyCode::Down => select_next_session(session, overlay),
        KeyCode::PageUp => jump_session(session, overlay, overlay.selected.saturating_sub(10)),
        KeyCode::PageDown => jump_session(session, overlay, overlay.selected + 10),
        KeyCode::Home => jump_session(session, overlay, 0),
        KeyCode::End => jump_session(session, overlay, usize::MAX),
        KeyCode::Backspace => {
            overlay.query.pop();
            overlay.last_searched = None; // the query changed, re-search on the next Enter
        }
        KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => {
            overlay.query.clear();
            overlay.last_searched = None;
        }
        KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
            overlay.query.push(c);
            overlay.last_searched = None;
        }
        _ => {}
    }
}

fn apply_session_search(session: &dyn CoreSession, overlay: &mut SessionsOverlay) {
    overlay.items = if overlay.query.is_empty() {
        session.list_sessions()
    } else {
        session.search_sessions(&overlay.query)
    };
    overlay.last_searched = Some(overlay.query.clone());
    overlay.selected = 0;
    overlay.detail_mode = SessionDetailMode::Preview;
    overlay.history_selected = 0;
}

/// Inline rename input (list pane `r`): mirrors the search-input pattern — text keys
/// edit the draft, Enter commits via `rename_session`, Esc cancels. A blank draft is
/// a no-op on the backend, so we just drop out of rename mode.
fn handle_session_rename_key(
    state: &mut AppState,
    session: &dyn CoreSession,
    overlay: &mut SessionsOverlay,
    k: KeyEvent,
) {
    let Some(draft) = overlay.renaming.as_mut() else {
        return;
    };
    match k.code {
        KeyCode::Esc => overlay.renaming = None,
        KeyCode::Enter => {
            let draft = overlay.renaming.take().unwrap_or_default();
            let title = draft.trim().to_string();
            let Some(item) = overlay.items.get(overlay.selected) else {
                return;
            };
            if !title.is_empty() && title != item.title {
                session.rename_session(&item.id, &title);
                let id = item.id.clone();
                refresh_sessions_overlay(session, overlay);
                // Keep the renamed row selected: search results can reorder/drop rows.
                if let Some(idx) = overlay.items.iter().position(|item| item.id == id) {
                    overlay.selected = idx;
                    refresh_session_history(session, overlay);
                }
                state.refresh_status_snapshot(session);
                let mut args = fluent_bundle::FluentArgs::new();
                args.set("title", title.as_str());
                state.set_status(state.i18n.format("sessions-status-renamed", Some(&args)));
            }
        }
        KeyCode::Backspace => {
            draft.pop();
        }
        KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => draft.clear(),
        KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => draft.push(c),
        _ => {}
    }
}

/// Archive the selected session (list pane `a`, no confirm: reversible in principle
/// and non-destructive — the history stays loadable). Selection moves to the item
/// that slides into the freed slot (or clamps to the new last row).
fn archive_selected_session(
    state: &mut AppState,
    session: &dyn CoreSession,
    overlay: &mut SessionsOverlay,
) {
    let Some(item) = overlay.items.get(overlay.selected) else {
        return;
    };
    let title = item.title.clone();
    session.archive_session(&item.id);
    refresh_sessions_overlay(session, overlay);
    let mut args = fluent_bundle::FluentArgs::new();
    args.set("title", title.as_str());
    state.set_status(state.i18n.format("sessions-status-archived", Some(&args)));
}

/// Returns `Some(zoom)` when the key opens the fullscreen text viewer over this
/// overlay (history pane `v`); the caller wires `restore` and swaps overlays.
fn handle_session_list_key(
    state: &mut AppState,
    session: &dyn CoreSession,
    overlay: &mut SessionsOverlay,
    k: KeyEvent,
    close: &mut bool,
) -> Option<ZoomOverlay> {
    match k.code {
        // `v` in the history pane: view the selected entry's FULL text (the list
        // row is truncated to one line) in the zoom viewer.
        KeyCode::Char('v')
            if !overlay.selection_mode
                && overlay.detail_mode == SessionDetailMode::HistoryPicker =>
        {
            return history_entry_zoom(state, overlay);
        }
        KeyCode::Esc if overlay.selection_mode => {
            overlay.selection_mode = false;
            overlay.selected_session_ids.clear();
        }
        KeyCode::Esc if overlay.detail_mode == SessionDetailMode::HistoryPicker => {
            overlay.detail_mode = SessionDetailMode::Preview;
        }
        KeyCode::Esc | KeyCode::Char('q') => *close = true,
        KeyCode::Char('/') if !overlay.selection_mode => overlay.searching = true,
        KeyCode::Left if overlay.detail_mode == SessionDetailMode::HistoryPicker => {
            overlay.detail_mode = SessionDetailMode::Preview;
        }
        KeyCode::Right
            if !overlay.selection_mode && overlay.detail_mode == SessionDetailMode::Preview =>
        {
            overlay.detail_mode = SessionDetailMode::HistoryPicker;
            overlay.history_selected = 0;
        }
        KeyCode::Up if overlay.detail_mode == SessionDetailMode::HistoryPicker => {
            overlay.history_selected = overlay.history_selected.saturating_sub(1);
        }
        KeyCode::Down if overlay.detail_mode == SessionDetailMode::HistoryPicker => {
            overlay.history_selected =
                (overlay.history_selected + 1).min(overlay.history.len().saturating_sub(1));
        }
        KeyCode::Up => select_prev_session(session, overlay),
        KeyCode::Down => select_next_session(session, overlay),
        KeyCode::PageUp => jump_session(session, overlay, overlay.selected.saturating_sub(10)),
        KeyCode::PageDown => jump_session(session, overlay, overlay.selected + 10),
        KeyCode::Home => jump_session(session, overlay, 0),
        KeyCode::End => jump_session(session, overlay, usize::MAX),
        KeyCode::Enter
            if !overlay.selection_mode
                && overlay.detail_mode == SessionDetailMode::HistoryPicker =>
        {
            open_history_action_choice(state, overlay);
        }
        KeyCode::Enter if !overlay.selection_mode => open_session_switch_confirm(state, overlay),
        KeyCode::Char('p') if !overlay.selection_mode => {
            overlay.detail_mode = SessionDetailMode::Preview
        }
        KeyCode::Char('m')
            if !overlay.selection_mode && overlay.detail_mode == SessionDetailMode::Preview =>
        {
            enter_session_selection_mode(overlay);
        }
        // Rename / archive live in the LIST pane only (not history pane, not
        // selection mode) — same scope as `m`.
        KeyCode::Char('r')
            if !overlay.selection_mode && overlay.detail_mode == SessionDetailMode::Preview =>
        {
            if let Some(item) = overlay.items.get(overlay.selected) {
                overlay.renaming = Some(item.title.clone());
            }
        }
        KeyCode::Char('a')
            if !overlay.selection_mode && overlay.detail_mode == SessionDetailMode::Preview =>
        {
            archive_selected_session(state, session, overlay);
        }
        KeyCode::Char(' ') if overlay.selection_mode => {
            toggle_session_mark(overlay);
        }
        // No per-turn history delete: removing one turn from the middle breaks LLM
        // context reconstruction (assistant tool_calls must keep their tool results).
        // Whole-session delete stays; `d` is inert while the history pane has focus.
        KeyCode::Char('d') if overlay.detail_mode != SessionDetailMode::HistoryPicker => {
            open_sessions_delete_confirm(state, overlay);
        }
        _ => {}
    }
    None
}

fn handle_confirm_key(
    state: &mut AppState,
    session: &dyn CoreSession,
    overlay: &mut SessionsOverlay,
    mut confirm: ConfirmDialog,
    k: KeyEvent,
    close: &mut bool,
) {
    match k.code {
        KeyCode::Esc => {}
        KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
            confirm.selected = if confirm.selected == 0 { 1 } else { 0 };
            overlay.confirm = Some(confirm);
        }
        KeyCode::Enter => handle_confirm_enter(state, session, overlay, confirm, close),
        _ => {
            overlay.confirm = Some(confirm);
        }
    }
}

fn handle_confirm_enter(
    state: &mut AppState,
    session: &dyn CoreSession,
    overlay: &mut SessionsOverlay,
    confirm: ConfirmDialog,
    close: &mut bool,
) {
    match confirm.action {
        ConfirmAction::ChooseHistoryAction {
            session_id,
            history_id,
        } => {
            let action = if confirm.selected == 0 {
                HistoryAction::Fork
            } else {
                HistoryAction::Rewind
            };
            perform_history_action(state, session, action, session_id, history_id);
        }
        action if confirm.selected == 0 => {
            let _ = action;
        }
        ConfirmAction::SwitchSession { id } => {
            session.reopen_session(&id);
            state.reset_conversation_view();
            state.refresh_status_snapshot(session);
            *close = true;
        }
        ConfirmAction::DeleteSessions { ids } => {
            let switched_to = session.delete_sessions(&ids);
            overlay.selection_mode = false;
            overlay.selected_session_ids.clear();
            let mut args = fluent_bundle::FluentArgs::new();
            args.set("count", ids.len() as i64);
            state.set_status(if let Some(session_id) = &switched_to {
                args.set("id", session_id.as_str());
                state
                    .i18n
                    .format("sessions-status-deleted-switched", Some(&args))
            } else {
                state.i18n.format("sessions-status-deleted", Some(&args))
            });
            refresh_sessions_overlay(session, overlay);
            if let Some(session_id) = switched_to {
                overlay.selected = overlay
                    .items
                    .iter()
                    .position(|item| item.id == session_id)
                    .unwrap_or(0);
                refresh_session_history(session, overlay);
                state.reset_conversation_view();
                state.refresh_status_snapshot(session);
            }
        }
        ConfirmAction::ForkHistory {
            session_id,
            history_id,
        } => {
            perform_history_action(state, session, HistoryAction::Fork, session_id, history_id);
        }
        ConfirmAction::RewindHistory {
            session_id,
            history_id,
        } => {
            perform_history_action(
                state,
                session,
                HistoryAction::Rewind,
                session_id,
                history_id,
            );
        }
        ConfirmAction::BindProviderKey { .. } => {}
        ConfirmAction::SwitchWorkspace { .. } => {} // only produced by the /workspace panel, the sessions panel never hits it
    }
}

fn jump_session(session: &dyn CoreSession, overlay: &mut SessionsOverlay, to: usize) {
    overlay.selected = to.min(overlay.items.len().saturating_sub(1));
    refresh_session_history(session, overlay);
}

fn select_prev_session(session: &dyn CoreSession, overlay: &mut SessionsOverlay) {
    overlay.selected = overlay.selected.saturating_sub(1);
    refresh_session_history(session, overlay);
}

fn select_next_session(session: &dyn CoreSession, overlay: &mut SessionsOverlay) {
    overlay.selected = (overlay.selected + 1).min(overlay.items.len().saturating_sub(1));
    refresh_session_history(session, overlay);
}

enum HistoryAction {
    Fork,
    Rewind,
}

fn toggle_session_mark(overlay: &mut SessionsOverlay) {
    let Some(session) = overlay.items.get(overlay.selected) else {
        return;
    };
    if !overlay.selected_session_ids.insert(session.id.clone()) {
        overlay.selected_session_ids.remove(&session.id);
    }
}

fn enter_session_selection_mode(overlay: &mut SessionsOverlay) {
    overlay.detail_mode = SessionDetailMode::Preview;
    overlay.selection_mode = true;
    if let Some(session) = overlay.items.get(overlay.selected) {
        overlay.selected_session_ids.insert(session.id.clone());
    }
}

fn open_sessions_delete_confirm(state: &AppState, overlay: &mut SessionsOverlay) {
    let ids = if !overlay.selection_mode || overlay.selected_session_ids.is_empty() {
        overlay
            .items
            .get(overlay.selected)
            .map(|session| vec![session.id.clone()])
            .unwrap_or_default()
    } else {
        overlay.selected_session_ids.iter().cloned().collect()
    };
    if ids.is_empty() {
        return;
    }
    let mut args = fluent_bundle::FluentArgs::new();
    args.set("count", ids.len() as i64);
    overlay.confirm = Some(ConfirmDialog {
        title: state.i18n.format("sessions-delete-title", Some(&args)),
        message: state.i18n.format("sessions-delete-message", Some(&args)),
        confirm_label: state.i18n.t("common-delete"),
        cancel_label: state.i18n.t("common-cancel"),
        danger: true,
        selected: 0,
        action: ConfirmAction::DeleteSessions { ids },
    });
}

fn open_session_switch_confirm(state: &AppState, overlay: &mut SessionsOverlay) {
    let Some(session) = overlay.items.get(overlay.selected) else {
        return;
    };
    let mut args = fluent_bundle::FluentArgs::new();
    args.set("title", session.title.as_str());
    overlay.confirm = Some(ConfirmDialog {
        title: state.i18n.t("sessions-switch-title"),
        message: state.i18n.format("sessions-switch-message", Some(&args)),
        confirm_label: state.i18n.t("sessions-switch-confirm"),
        cancel_label: state.i18n.t("common-cancel"),
        danger: false,
        selected: 1,
        action: ConfirmAction::SwitchSession {
            id: session.id.clone(),
        },
    });
}

fn open_history_action_choice(state: &AppState, overlay: &mut SessionsOverlay) {
    let Some(session_item) = overlay.items.get(overlay.selected) else {
        return;
    };
    let Some(history) = overlay.history.get(overlay.history_selected) else {
        return;
    };
    let mut args = fluent_bundle::FluentArgs::new();
    args.set("turn", history.turn_index() as i64);
    overlay.confirm = Some(ConfirmDialog {
        title: state.i18n.t("sessions-history-action-title"),
        message: state
            .i18n
            .format("sessions-history-action-message", Some(&args)),
        confirm_label: state.i18n.t("sessions-history-action-rewind"),
        cancel_label: state.i18n.t("sessions-history-action-fork"),
        danger: false,
        selected: 0,
        action: ConfirmAction::ChooseHistoryAction {
            session_id: session_item.id.clone(),
            history_id: history.id().to_string(),
        },
    });
}

fn perform_history_action(
    state: &mut AppState,
    session: &dyn CoreSession,
    action: HistoryAction,
    session_id: String,
    history_id: String,
) {
    match action {
        HistoryAction::Fork => {
            let new_id = session
                .fork_history(&session_id, &history_id)
                .unwrap_or_else(|| format!("{session_id}-fork"));
            session.reopen_session(&new_id);
            state.reset_conversation_view();
            state.refresh_status_snapshot(session);
            let mut args = fluent_bundle::FluentArgs::new();
            args.set("id", new_id.as_str());
            state.set_status(state.i18n.format("sessions-status-forked", Some(&args)));
        }
        HistoryAction::Rewind => {
            session.rewind_history(&session_id, &history_id);
            let mut args = fluent_bundle::FluentArgs::new();
            args.set("id", history_id.as_str());
            state.set_status(state.i18n.format("sessions-status-rewind", Some(&args)));
        }
    }
}

fn refresh_sessions_overlay(session: &dyn CoreSession, overlay: &mut SessionsOverlay) {
    overlay.items = if overlay.query.is_empty() {
        session.list_sessions()
    } else {
        session.search_sessions(&overlay.query)
    };
    overlay.last_searched = Some(overlay.query.clone());
    overlay.selected = overlay.selected.min(overlay.items.len().saturating_sub(1));
    refresh_session_history(session, overlay);
}

fn refresh_session_history(session: &dyn CoreSession, overlay: &mut SessionsOverlay) {
    overlay.history = overlay
        .items
        .get(overlay.selected)
        .map(|item| HistoryItem::messages_only(session.session_history(&item.id)))
        .unwrap_or_default();
    overlay.detail_mode = SessionDetailMode::Preview;
    overlay.history_selected = 0;
}

fn newline(state: &mut AppState) {
    // Cap growth at MAX_INPUT_LINES logical lines; beyond that the box scrolls
    // internally. Extra blank continuation is still allowed via typing.
    let at = char_to_byte(&state.input, state.input_cursor);
    state.input.insert(at, '\n');
    state.input_cursor += 1;
    state.clear_overlay();
    state.dirty = true;
}

fn refresh_suggest(state: &mut AppState, session: &dyn CoreSession) {
    if let Some((command, query)) = command_argument_context(&state.input) {
        state.set_command_suggest(SuggestState {
            items: command_argument_suggestions(state, command, query),
            selected: 0,
            query: query.to_string(),
        });
        return;
    }
    let Some((marker, query)) = active_token(&state.input) else {
        if state.is_command_suggest_open() {
            state.clear_overlay();
        }
        return;
    };
    if marker != '/' {
        if state.is_command_suggest_open() {
            state.clear_overlay();
        }
        return;
    }
    state.set_command_suggest(SuggestState {
        items: command_suggestions(state, query, session),
        selected: 0,
        query: query.to_string(),
    });
}

fn command_argument_context(input: &str) -> Option<(&str, &str)> {
    let input = input.strip_prefix('/')?;
    if input.contains('\n') {
        return None;
    }
    let (command, query) = input.split_once(char::is_whitespace)?;
    let query = query.trim();
    matches!(command, "plan" | "approval" | "lang" | "theme" | "view").then_some((command, query))
}

fn command_argument_suggestions(state: &AppState, command: &str, query: &str) -> Vec<SuggestItem> {
    if command == "theme" {
        let query = query.to_ascii_lowercase();
        return theme::all()
            .into_iter()
            .filter(|theme| {
                query.is_empty()
                    || theme.id.to_ascii_lowercase().contains(&query)
                    || theme.label.to_ascii_lowercase().contains(&query)
            })
            .map(|theme| SuggestItem::Argument {
                command: command.into(),
                value: theme.id.into(),
                description: theme.label.into(),
            })
            .collect();
    }
    let options: &[(&str, &str)] = match command {
        "plan" => &[
            ("on", "command-plan-on"),
            ("off", "command-plan-off"),
            ("toggle", "command-plan-toggle"),
        ],
        "approval" => &[
            ("auto", "command-approval-auto"),
            ("deny", "command-approval-deny"),
            ("all", "command-approval-all"),
        ],
        "lang" => &[
            ("zh-CN", "command-lang-zh-cn"),
            ("en-US", "command-lang-en-us"),
        ],
        "view" => &[
            ("minimal", "command-view-minimal"),
            ("normal", "command-view-normal"),
            ("verbose", "command-view-verbose"),
        ],
        _ => return Vec::new(),
    };
    let query = query.to_ascii_lowercase();
    options
        .iter()
        .filter(|(value, description)| {
            query.is_empty()
                || value.to_ascii_lowercase().contains(&query)
                || state
                    .i18n
                    .t(description)
                    .to_ascii_lowercase()
                    .contains(&query)
        })
        .map(|(value, description)| SuggestItem::Argument {
            command: command.into(),
            value: (*value).into(),
            description: state.i18n.t(description),
        })
        .collect()
}

fn active_token(input: &str) -> Option<(char, &str)> {
    if input.ends_with(char::is_whitespace) {
        return None;
    }
    let token = input
        .split(char::is_whitespace)
        .next_back()
        .unwrap_or_default();
    let marker = token.chars().next()?;
    if marker == '/' || marker == '@' {
        Some((marker, &token[marker.len_utf8()..]))
    } else {
        None
    }
}

fn command_suggestions(
    state: &AppState,
    query: &str,
    session: &dyn CoreSession,
) -> Vec<SuggestItem> {
    let q = query.to_lowercase();
    let mut commands: Vec<_> = session
        .command_catalog()
        .into_iter()
        .map(|mut cmd| {
            if cmd.kind == CommandKind::Builtin {
                cmd.description = builtin_command_description(state, &cmd.name);
            }
            cmd
        })
        .filter(|cmd| {
            let name = cmd.name.to_lowercase();
            let desc = cmd.description.to_lowercase();
            q.is_empty() || name.contains(&q) || desc.contains(&q)
        })
        .collect();
    commands.sort_by_key(|cmd| {
        let name = cmd.name.to_lowercase();
        let rank = if q.is_empty() || name.starts_with(&q) {
            0
        } else if name.contains(&q) {
            1
        } else {
            2
        };
        (rank, cmd.kind != CommandKind::Builtin, cmd.name.clone())
    });
    commands.into_iter().map(SuggestItem::Command).collect()
}

fn move_suggest(state: &mut AppState, delta: isize) {
    if let Some(suggest) = state.command_suggest_mut() {
        let len = suggest.items.len();
        if len == 0 {
            return;
        }
        suggest.selected = if delta < 0 {
            suggest.selected.saturating_sub(1)
        } else {
            (suggest.selected + 1).min(len - 1)
        };
        state.dirty = true;
    }
}

fn accept_suggest(state: &mut AppState, session: &dyn CoreSession) -> bool {
    accept_suggest_with_action(state, session, false)
}

fn submit_suggest(state: &mut AppState, session: &dyn CoreSession) -> bool {
    accept_suggest_with_action(state, session, true)
}

fn accept_suggest_with_action(
    state: &mut AppState,
    session: &dyn CoreSession,
    submit_argument: bool,
) -> bool {
    let Some(suggest) = state.command_suggest() else {
        return false;
    };
    let Some(item) = suggest.items.get(suggest.selected).cloned() else {
        state.dirty = true;
        return false;
    };
    match item {
        SuggestItem::Command(cmd) if cmd.kind == CommandKind::Builtin => {
            if builtin_has_options(&cmd.name) {
                state.input = format!("/{} ", cmd.name);
                cursor_to_end(state);
                state.clear_overlay();
                refresh_suggest(state, session);
            } else {
                let text = format!("/{}", cmd.name);
                state.input.clear();
                cursor_to_end(state);
                state.clear_overlay();
                record_input_history(state, builtin_command_messages(&text), text.clone());
                handle_builtin_command(state, &text, session);
            }
        }
        SuggestItem::Command(cmd) => {
            replace_active_token(state, &format!("/{} ", cmd.name));
            state.clear_overlay();
        }
        SuggestItem::Argument { command, value, .. } => {
            let text = format!("/{command} {value}");
            if submit_argument {
                state.input.clear();
                cursor_to_end(state);
                state.clear_overlay();
                record_input_history(state, builtin_command_messages(&text), text.clone());
                handle_builtin_command(state, &text, session);
            } else {
                state.input = format!("{text} ");
                cursor_to_end(state);
                state.clear_overlay();
            }
        }
    }
    state.dirty = true;
    true
}

fn builtin_has_options(name: &str) -> bool {
    matches!(name, "plan" | "approval" | "lang" | "theme" | "view")
}

fn replace_active_token(state: &mut AppState, replacement: &str) {
    let start = state
        .input
        .char_indices()
        .rev()
        .find_map(|(i, ch)| ch.is_whitespace().then_some(i + ch.len_utf8()))
        .unwrap_or(0);
    state.input.truncate(start);
    state.input.push_str(replacement);
    cursor_to_end(state);
}

fn on_paste(state: &mut AppState, text: String) {
    if state.stage == Stage::Boot {
        finish_boot(state);
    }
    let text = normalize_paste(text);
    state.history_replay = None;
    let chars = text.chars().count();
    let lines = paste_line_count(&text);
    if chars <= DIRECT_PASTE_CHARS && lines <= DIRECT_PASTE_LINES {
        state.input.push_str(&text);
        cursor_to_end(state);
        append_input_space(state);
        let mut args = fluent_bundle::FluentArgs::new();
        args.set("chars", chars as i64);
        state.set_status(state.i18n.format("status-pasted-direct", Some(&args)));
        state.dirty = true;
        return;
    }

    let mut args = fluent_bundle::FluentArgs::new();
    args.set("glyph", Glyph::Paste.render(state.icons));
    args.set("lines", lines as i64);
    args.set("chars", chars as i64);
    let display = format!(
        "{}#{}",
        state.i18n.format("paste-display-inline", Some(&args)),
        state.chips.len() + 1
    );
    state.input.push_str(&display);
    cursor_to_end(state);
    state.chips.push(Chip::paste(PasteDraft {
        text,
        display,
        lines,
        chars,
    }));
    state.set_status(state.i18n.t("status-paste-full-render"));
    append_input_space(state);
    state.dirty = true;
}

fn normalize_paste(text: String) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn paste_line_count(text: &str) -> usize {
    text.split('\n').count().max(1)
}

fn append_input_space(state: &mut AppState) {
    if !state.input.ends_with(char::is_whitespace) {
        state.input.push(' ');
        cursor_to_end(state);
    }
}

fn reset_input_history_cursor(state: &mut AppState) {
    state.input_history.cursor = None;
    state.input_history.draft = None;
}

fn browse_input_history(state: &mut AppState, delta: isize) {
    let len = state.input_history.entries.len();
    if len == 0 {
        return;
    }
    if state.input_history.cursor.is_none() {
        state.input_history.draft = Some(state.input.clone());
    }
    let current = state.input_history.cursor.unwrap_or(len);
    let next = if delta < 0 {
        current.saturating_sub(1)
    } else if current + 1 >= len {
        state.input_history.cursor = None;
        state.input = state.input_history.draft.take().unwrap_or_default();
        cursor_to_end(state);
        state.history_replay = None;
        state.chips.clear();
        state.clear_overlay();
        state.dirty = true;
        return;
    } else {
        current + 1
    };
    state.input_history.cursor = Some(next);
    if let Some(entry) = state.input_history.entries.get(next) {
        let display = entry.display.clone();
        let messages = entry.messages.clone();
        state.input = display;
        cursor_to_end(state);
        state.history_replay = Some(messages);
        state.chips.clear();
    }
    state.clear_overlay();
    state.dirty = true;
}

fn record_input_history(state: &mut AppState, messages: Vec<Message>, display: String) {
    let display = display.trim().to_string();
    if display.is_empty() || messages.is_empty() {
        return;
    }
    state
        .input_history
        .entries
        .retain(|entry| entry.display != display);
    state
        .input_history
        .entries
        .push(InputHistoryEntry { messages, display });
    if state.input_history.entries.len() > INPUT_HISTORY_LIMIT {
        let drop = state.input_history.entries.len() - INPUT_HISTORY_LIMIT;
        state.input_history.entries.drain(0..drop);
    }
    reset_input_history_cursor(state);
    save_input_history(&state.input_history.entries);
}

#[cfg(not(test))]
fn save_input_history(entries: &[InputHistoryEntry]) {
    let path = super::input_history_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(entries) {
        let _ = std::fs::write(path, json);
    }
}

#[cfg(test)]
fn save_input_history(_entries: &[InputHistoryEntry]) {}

fn prune_detached_chips(state: &mut AppState) {
    state.chips.retain(|chip| match &chip.kind {
        ChipKind::Paste(paste) => state.input.contains(&paste.display),
        ChipKind::File(file) => state.input.contains(&file.token),
    });
}

/// Backspace at the end of an attachment removes the whole composer block.
/// Accepted chips carry a trailing presentation space, so ignore horizontal
/// whitespace while matching and remove it together with the chip.
fn remove_trailing_chip(state: &mut AppState) -> bool {
    let trimmed = state.input.trim_end_matches([' ', '\t']);
    let Some((chip_index, token_start)) =
        state
            .chips
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, chip)| {
                let token = match &chip.kind {
                    ChipKind::Paste(paste) => paste.display.as_str(),
                    ChipKind::File(file) => file.token.as_str(),
                };
                trimmed
                    .strip_suffix(token)
                    .map(|prefix| (index, prefix.len()))
            })
    else {
        return false;
    };

    state.input.truncate(token_start);
    state.chips.remove(chip_index);
    true
}

fn submit(state: &mut AppState, session: &dyn CoreSession) {
    let text = state.input.trim().to_string();
    state.clear_overlay();
    prune_detached_chips(state);
    let chips = std::mem::take(&mut state.chips);
    let replay = state
        .history_replay
        .take()
        .map(|messages| validate_submit_messages(messages, session));
    if text.is_empty() && chips.is_empty() && replay.is_none() {
        return;
    }

    // Streaming has two explicit destinations: core mailbox injection into the
    // current turn, or a CLI-local follow-up turn started after summary settle.
    if state.mode == Mode::Streaming {
        let messages = replay.unwrap_or_else(|| messages_for_submit(&text, &chips, session));
        record_input_history(state, messages.clone(), text.clone());
        let display = messages_preview(&messages);
        match state.streaming_send_target {
            StreamingSendTarget::Mailbox => {
                if session.send(Command::MailboxEnqueue { messages }) {
                    state.set_status(state.i18n.t("status-mailbox-queued"));
                } else {
                    state.set_status(state.i18n.t("status-mailbox-rejected"));
                }
            }
            StreamingSendTarget::NextTurn => {
                state
                    .next_turn_queue
                    .push_back(QueuedTurn { messages, display });
                state.set_status(state.i18n.t("status-next-turn-queued"));
            }
        }
        state.input.clear();
        cursor_to_end(state);
        state.dirty = true;
        return;
    }

    // Slash built-ins are handled locally both when freshly typed and when restored
    // from structured input history. An unchanged replay must not fall through to
    // core merely because it already carries a `Part::Command` payload.
    let replay_is_builtin = replay.as_deref().is_some_and(messages_are_builtin_command);
    if chips.is_empty()
        && (replay.is_none() || replay_is_builtin)
        && handle_builtin_command(state, &text, session)
    {
        let messages = replay.unwrap_or_else(|| builtin_command_messages(&text));
        record_input_history(state, messages, text.clone());
        state.input.clear();
        cursor_to_end(state);
        state.dirty = true;
        return;
    }

    let messages = replay.unwrap_or_else(|| messages_for_submit(&text, &chips, session));
    // User message settles immediately into scrollback from the exact structured
    // payload sent to core, so Part ordering and type-specific markers stay intact.
    push_user_history(state, &messages);
    record_input_history(state, messages.clone(), text.clone());
    try_start_turn(state, session, messages);

    state.input.clear();
    cursor_to_end(state);
    state.chips.clear();
    state.help_shown = false; // first submission ends the splash help
    state.dirty = true;
}

fn no_usable_model_status(state: &AppState, session: &dyn CoreSession) -> String {
    let mut args = fluent_bundle::FluentArgs::new();
    args.set("models_file", session.models_config().unwrap_or_default());
    state.i18n.format("error-no-usable-model", Some(&args))
}

fn undo_streaming_queue(state: &mut AppState, session: &dyn CoreSession) {
    if !state.input.is_empty() || !state.chips.is_empty() || state.history_replay.is_some() {
        state.set_status(state.i18n.t("status-queue-undo-draft"));
        state.dirty = true;
        return;
    }
    let removed = match state.streaming_send_target {
        StreamingSendTarget::NextTurn => state
            .next_turn_queue
            .pop_back()
            .map(|queued| (queued.messages, queued.display)),
        StreamingSendTarget::Mailbox => state.live.mailbox.last().cloned().and_then(|entry| {
            let display = entry.preview();
            session
                .remove_mailbox(&entry.id)
                .then_some((entry.messages, display))
        }),
    };
    if let Some((messages, display)) = removed {
        state.input = display;
        state.history_replay = Some(messages);
        state.set_status(state.i18n.t("status-queue-undone"));
    } else {
        state.set_status(state.i18n.t("status-queue-empty"));
    }
    state.dirty = true;
}

/// Dev-only escape hatch: `ZLOGIC_DEBUG_OVERLAY=session` (or `model`, `help`, …)
/// opens the named overlay right after startup. Exists to drive rendering repros
/// inside REAL terminals where key injection is permission-gated (Terminal.app via
/// osascript needs Accessibility rights) — not a user feature.
pub fn debug_open_overlay(state: &mut AppState, session: &dyn CoreSession, cmd: &str) {
    let text = format!("/{}", cmd.trim_start_matches('/'));
    if handle_builtin_command(state, &text, session) {
        state.dirty = true;
    }
}

fn handle_builtin_command(state: &mut AppState, text: &str, session: &dyn CoreSession) -> bool {
    let Some(name) = text.strip_prefix('/') else {
        return false;
    };
    let name = name.split_whitespace().next().unwrap_or_default();
    let Some(cmd) = session
        .command_catalog()
        .into_iter()
        .find(|cmd| cmd.name == name && cmd.kind == CommandKind::Builtin)
    else {
        return false;
    };

    clear_composer_after_command(state);

    match cmd.name.as_str() {
        "plan" => set_plan_mode(state, text, session),
        "approval" => set_approval_mode(state, text),
        "lang" => set_language(state, text, session),
        "help" => {
            let commands = session
                .command_catalog()
                .into_iter()
                .map(|mut command| {
                    if command.kind == CommandKind::Builtin {
                        command.description = builtin_command_description(state, &command.name);
                    }
                    command
                })
                .collect();
            state.set_help_overlay(commands);
        }
        "stats" => {
            let usage = session.usage_overview();
            let by_model = session.usage_by_model();
            let timeline = session.usage_timeline();
            state.set_stats_overlay(usage, by_model, timeline);
        }
        "info" => show_session_info(state, session),
        "session" => {
            open_sessions_overlay(state, session);
        }
        "replay" => {
            open_replay_overlay(state, session);
        }
        "workspace" => {
            open_workspaces_overlay(state, session);
        }
        "model" => {
            let query = text.split_whitespace().nth(1).unwrap_or("").to_string();
            let providers = session.provider_catalog();
            let models = session.model_catalog();
            let (providers, models) = crate::app::filter_catalog(&providers, &models, &query);
            state.set_models_overlay(
                providers,
                models,
                session.key_catalog(),
                session.models_config(),
                query,
            );
        }
        "theme" => set_theme(state, text),
        "view" => set_view_mode(state, text),
        "new" => start_new_session(state, session),
        "compact" => start_compaction(state, session),
        _ => return false,
    }
    true
}

fn clear_composer_after_command(state: &mut AppState) {
    state.input.clear();
    state.chips.clear();
    state.history_replay = None;
    state.file_scan_generation = state.file_scan_generation.wrapping_add(1);
    state.file_scan_request = None;
    reset_input_history_cursor(state);
    state.dirty = true;
}

fn start_compaction(state: &mut AppState, session: &dyn CoreSession) {
    if session.send(Command::CompactContext) {
        state.set_status(state.i18n.t("status-compacting"));
    } else {
        state.set_status(state.i18n.t("status-compact-rejected"));
    }
    state.dirty = true;
}

/// `/new` — create + switch to a fresh session and reset the per-session UI state,
/// mirroring the session-switch path (transcript/live reset + status message).
fn start_new_session(state: &mut AppState, session: &dyn CoreSession) {
    let Some(_id) = session.new_session() else {
        state.set_status(state.i18n.t("status-new-failed"));
        return;
    };
    state.reset_conversation_view();
    state.refresh_status_snapshot(session);
    state.set_status(state.i18n.t("status-new-session"));
    state.dirty = true;
}

fn builtin_command_description(state: &AppState, name: &str) -> String {
    let key = match name {
        "help" => "command-help-desc",
        "model" => "command-model-desc",
        "key" => "command-key-desc",
        "theme" => "command-theme-desc",
        "view" => "command-view-desc",
        "session" => "command-session-desc",
        "replay" => "command-replay-desc",
        "workspace" => "command-workspace-desc",
        "new" => "command-new-desc",
        "compact" => "command-compact-desc",
        "stats" => "command-stats-desc",
        "info" => "command-info-desc",
        "plan" => "command-plan-desc",
        "approval" => "command-approval-desc",
        "lang" => "command-lang-desc",
        _ => return String::new(),
    };
    state.i18n.t(key)
}

fn show_session_info(state: &mut AppState, session: &dyn CoreSession) {
    state.refresh_status_snapshot(session);
    let snapshot = &state.status_snapshot;
    let info = SessionInfoView {
        cwd: snapshot.cwd.clone(),
        session_id: snapshot
            .session_id
            .clone()
            .unwrap_or_else(|| "unknown".into()),
        session_title: snapshot
            .session_title
            .clone()
            .unwrap_or_else(|| "untitled".into()),
        model: snapshot
            .model_identity()
            .unwrap_or_else(|| "unknown".into()),
        mode: match idle_mode(state) {
            Mode::God => "god",
            Mode::Plan => "plan",
            Mode::Normal | Mode::Streaming => "normal",
        }
        .into(),
        approval: match state.permission_mode {
            PermissionMode::Auto => "auto",
            PermissionMode::Deny => "deny",
            PermissionMode::ApproveAll => "all",
        }
        .into(),
        total_tokens: snapshot.total_tokens,
        context_used: snapshot.context_used_tokens,
        context_limit: snapshot.context_limit_tokens,
    };
    let overlay = ZoomOverlay {
        title: state.i18n.t("session-info-title"),
        lines: session_info_lines(state, &info),
        scroll: 0,
        kind: ZoomKind::SessionInfo,
        copy_text: None,
        copy_feedback: None,
        restore: None,
    };
    state.overlay = Some(super::OverlayState::Fullscreen(
        super::FullscreenOverlay::Zoom(overlay),
    ));
    state.dirty = true;
}

struct SessionInfoView {
    cwd: String,
    session_id: String,
    session_title: String,
    model: String,
    mode: String,
    approval: String,
    total_tokens: u64,
    context_used: u64,
    context_limit: u64,
}

fn session_info_lines(state: &AppState, info: &SessionInfoView) -> Vec<Line<'static>> {
    let th = &state.theme;
    let width = (state.width as usize).saturating_sub(4).max(10);
    let rule = Glyph::RuleHorizontal
        .render(state.icons)
        .repeat(width.min(48));
    let section = |key: &str| {
        Line::from(Span::styled(
            state.i18n.t(key),
            th.style(Sem::AccentSoft).add_modifier(Modifier::BOLD),
        ))
    };
    let badge = |label: &str, value: &str| {
        vec![
            Span::styled(format!(" {label} "), th.style(Sem::Muted)),
            Span::styled(
                format!(" {value} "),
                th.style(Sem::Info).add_modifier(Modifier::BOLD),
            ),
        ]
    };
    let hint = |command: &str, key: &str| {
        vec![
            Span::styled(
                format!("{} ", Glyph::HintBullet.render(state.icons)),
                th.style(Sem::Muted),
            ),
            Span::styled(command.to_string(), th.style(Sem::AccentSoft)),
            Span::styled(format!("  {}", state.i18n.t(key)), th.style(Sem::Muted)),
        ]
    };

    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!("{} ", Glyph::Session.render(state.icons)),
                th.style(Sem::Accent),
            ),
            Span::styled(
                info.session_title.clone(),
                th.style(Sem::Accent).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("session ", th.style(Sem::Muted)),
            Span::styled(info.session_id.clone(), th.style(Sem::Info)),
        ]),
        Line::from(Span::styled(rule, th.style(Sem::Border))),
        Line::from(String::new()),
        section("session-info-workspace"),
        Line::from(vec![
            Span::styled("cwd  ", th.style(Sem::Muted)),
            Span::styled(info.cwd.clone(), th.style(Sem::Info)),
        ]),
        Line::from(String::new()),
        section("session-info-model"),
        Line::from({
            let mut spans = vec![Span::styled(
                info.model.clone(),
                th.style(Sem::Info).add_modifier(Modifier::BOLD),
            )];
            spans.push(Span::raw("   "));
            spans.extend(hint("/model", "session-info-hint-model"));
            spans
        }),
        Line::from(String::new()),
        section("session-info-runtime"),
    ];
    let mut runtime = badge("mode", &info.mode);
    runtime.push(Span::raw("   "));
    runtime.extend(hint("/plan", "session-info-hint-plan"));
    lines.push(Line::from(runtime));
    let mut approval = badge("approval", &info.approval);
    approval.push(Span::raw("   "));
    approval.extend(hint("/approval", "session-info-hint-approval"));
    lines.push(Line::from(approval));
    lines.push(Line::from(String::new()));
    lines.push(section("session-info-usage"));
    lines.push(Line::from(vec![
        Span::styled("tokens  ", th.style(Sem::Muted)),
        Span::styled(info.total_tokens.to_string(), th.style(Sem::Info)),
    ]));
    lines.push(context_usage_line(state, info));

    lines
        .into_iter()
        .flat_map(|line| wrap_hist_line(&HistLine::from_line(line), width, width))
        .collect()
}

fn context_usage_line(state: &AppState, info: &SessionInfoView) -> Line<'static> {
    let th = &state.theme;
    if info.context_limit == 0 {
        return Line::from(vec![
            Span::styled("context ", th.style(Sem::Muted)),
            Span::styled("unknown", th.style(Sem::Muted)),
        ]);
    }
    const BAR_WIDTH: usize = 20;
    let ratio = (info.context_used as f64 / info.context_limit as f64).clamp(0.0, 1.0);
    let filled = (ratio * BAR_WIDTH as f64).round() as usize;
    let on = Glyph::ProgressFull.render(state.icons);
    let off = Glyph::ProgressEmpty.render(state.icons);
    let bar = format!("{}{}", on.repeat(filled), off.repeat(BAR_WIDTH - filled));
    Line::from(vec![
        Span::styled("context ", th.style(Sem::Muted)),
        Span::styled(bar, th.style(Sem::Accent)),
        Span::styled(
            format!(
                "  {:.0}%  {} / {}",
                ratio * 100.0,
                info.context_used,
                info.context_limit
            ),
            th.style(Sem::Info),
        ),
    ])
}

fn builtin_command_messages(text: &str) -> Vec<Message> {
    let Some(rest) = text.strip_prefix('/') else {
        return Vec::new();
    };
    let mut parts = rest.splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or_default().to_string();
    if name.is_empty() {
        return Vec::new();
    }
    let text = parts.next().unwrap_or_default().trim().to_string();
    vec![Message::from_parts(vec![Part::Command {
        command: "builtin".into(),
        name,
        text,
    }])]
}

fn messages_are_builtin_command(messages: &[Message]) -> bool {
    matches!(
        messages,
        [Message { parts }]
            if matches!(
                parts.as_slice(),
                [Part::Command { command, .. }] if command == "builtin"
            )
    )
}

fn open_replay_overlay(state: &mut AppState, session: &dyn CoreSession) {
    let Some(id) = session.current_session_id() else {
        state.set_status(state.i18n.t("replay-empty"));
        return;
    };
    let turns = session.session_turns(&id, None, None, None);
    if turns.is_empty() {
        state.set_status(state.i18n.t("replay-empty"));
        return;
    }
    let total = turns.len() as u64;
    state.set_replay_overlay(turns, total);
    state.dirty = true;
}

fn open_replay_detail(state: &mut AppState, session: &dyn CoreSession, turn_seq: u32) {
    let Some(id) = session.current_session_id() else {
        return;
    };
    let entries = session.session_turn_entries(&id, turn_seq);
    let rounds = crate::render::replay::detail_rounds(state, &entries);
    let failed = entries.is_empty();
    let (lines, head_at) = crate::app::flatten_rounds(&rounds);
    if let Some(super::OverlayState::Fullscreen(super::FullscreenOverlay::Replay(overlay))) =
        state.overlay.as_mut()
    {
        overlay.detail = Some(super::ReplayDetail {
            turn_seq,
            rounds,
            lines,
            head_at,
            selected_round: 0,
            scroll: 0,
            failed,
        });
    }
    state.dirty = true;
}

fn handle_replay_key(state: &mut AppState, k: KeyEvent, session: &dyn CoreSession) {
    let in_detail = matches!(
        state.fullscreen_overlay(),
        Some(super::FullscreenOverlay::Replay(o)) if o.detail.is_some()
    );
    if matches!(k.code, KeyCode::Esc | KeyCode::Char('q')) {
        if in_detail {
            if let Some(super::OverlayState::Fullscreen(super::FullscreenOverlay::Replay(o))) =
                state.overlay.as_mut()
            {
                o.detail = None;
            }
            state.dirty = true;
            return;
        }
        state.clear_overlay();
        state.dirty = true;
        return;
    }

    let page = zoom_content_rows(state.term_rows).max(1);

    if in_detail {
        if let Some(super::OverlayState::Fullscreen(super::FullscreenOverlay::Replay(o))) =
            state.overlay.as_mut()
        {
            let Some(detail) = o.detail.as_mut() else {
                return;
            };
            let count = detail.rounds.len();
            match k.code {
                KeyCode::Up => detail.selected_round = detail.selected_round.saturating_sub(1),
                KeyCode::Down => {
                    detail.selected_round = (detail.selected_round + 1).min(count.saturating_sub(1))
                }
                KeyCode::PageUp => {
                    detail.selected_round = detail.selected_round.saturating_sub(page)
                }
                KeyCode::PageDown => {
                    detail.selected_round =
                        (detail.selected_round + page).min(count.saturating_sub(1))
                }
                KeyCode::Enter | KeyCode::Char(' ') => {
                    let target = detail.selected_round;
                    if let Some(round) = detail.rounds.get_mut(target) {
                        round.open = !round.open;
                    }
                    let (lines, head_at) = crate::app::flatten_rounds(&detail.rounds);
                    detail.lines = lines;
                    detail.head_at = head_at;
                }
                _ => {}
            }
            detail_scroll_to(detail, state.term_rows);
        }
        state.dirty = true;
        return;
    }

    let mut enter: Option<u32> = None;
    if let Some(super::OverlayState::Fullscreen(super::FullscreenOverlay::Replay(o))) =
        state.overlay.as_mut()
    {
        let count = o.turns.len();
        match k.code {
            KeyCode::Up => o.selected = o.selected.saturating_sub(1),
            KeyCode::Down => o.selected = (o.selected + 1).min(count.saturating_sub(1)),
            KeyCode::PageUp => o.selected = o.selected.saturating_sub(page),
            KeyCode::PageDown => o.selected = (o.selected + page).min(count.saturating_sub(1)),
            KeyCode::Enter => enter = o.turns.get(o.selected).map(|t| t.turn_seq),
            _ => {}
        }
        if matches!(
            k.code,
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown
        ) {
            list_scroll_to(o, state.term_rows);
        }
    }
    if let Some(turn_seq) = enter {
        open_replay_detail(state, session, turn_seq);
    }
    state.dirty = true;
}

fn list_scroll_to(overlay: &mut super::ReplayOverlay, term_rows: u16) {
    let view = (term_rows as usize).saturating_sub(4).max(1);
    if overlay.selected < overlay.scroll {
        overlay.scroll = overlay.selected;
    } else if overlay.selected >= overlay.scroll + view {
        overlay.scroll = overlay.selected + 1 - view;
    }
}

fn detail_scroll_to(detail: &mut super::ReplayDetail, term_rows: u16) {
    let view = (term_rows as usize).saturating_sub(4).max(1);
    let anchor = detail
        .head_at
        .get(detail.selected_round)
        .copied()
        .unwrap_or(0);
    if anchor < detail.scroll {
        detail.scroll = anchor;
    } else if anchor >= detail.scroll + view {
        detail.scroll = anchor + 1 - view;
    }
}

fn open_sessions_overlay(state: &mut AppState, session: &dyn CoreSession) {
    let items = session.list_sessions();
    let history = items
        .first()
        .map(|item| HistoryItem::messages_only(session.session_history(&item.id)))
        .unwrap_or_default();
    state.set_sessions_overlay(SessionsOverlay {
        items,
        selected: 0,
        query: String::new(),
        searching: false,
        last_searched: None,
        renaming: None,
        history,
        detail_mode: SessionDetailMode::Preview,
        history_selected: 0,
        selection_mode: false,
        selected_session_ids: Default::default(),
        confirm: None,
    });
}

fn open_workspaces_overlay(state: &mut AppState, session: &dyn CoreSession) {
    let items = session.list_workspaces();
    let current = session.current_workspace();
    let selected = items
        .iter()
        .position(|ws| ws.workspace_id.to_string() == current)
        .unwrap_or(0);
    let sessions = items
        .get(selected)
        .map(|ws| session.workspace_sessions(&ws.workspace_id.to_string()))
        .unwrap_or_default();
    state.set_workspaces_overlay(WorkspacesOverlay {
        items,
        selected,
        current,
        sessions,
        focus: super::WorkspacePane::Left,
        session_selected: 0,
        confirm: None,
    });
}

fn set_plan_mode(state: &mut AppState, text: &str, session: &dyn CoreSession) {
    let requested = match text.split_whitespace().nth(1) {
        Some("on") => true,
        Some("off") => false,
        Some("toggle") | None => !state.plan_enabled,
        Some(_) => {
            state.set_status(state.i18n.t("status-plan-usage"));
            return;
        }
    };
    state.plan_enabled = session.set_plan_mode(requested);
    state.mode = idle_mode(state);
    state.set_status(state.i18n.t(if state.plan_enabled {
        "status-plan-on"
    } else {
        "status-plan-off"
    }));
}

fn set_theme(state: &mut AppState, text: &str) {
    let Some(id) = text.split_whitespace().nth(1) else {
        state.set_status(state.i18n.t("status-theme-usage"));
        return;
    };
    let Some(theme) = theme::by_id(id) else {
        state.set_status(state.i18n.t("status-theme-usage"));
        return;
    };
    let label = theme.label;
    state.theme.theme = theme;
    let mut args = fluent_bundle::FluentArgs::new();
    args.set("theme", label);
    state.set_status(state.i18n.format("status-theme-updated", Some(&args)));
}

fn set_view_mode(state: &mut AppState, text: &str) {
    let Some(arg) = text.split_whitespace().nth(1) else {
        let mut args = fluent_bundle::FluentArgs::new();
        args.set("mode", view_mode_label(state, state.view_mode));
        state.set_status(state.i18n.format("status-view-usage", Some(&args)));
        return;
    };
    let mode = match arg {
        "minimal" => ViewMode::Minimal,
        "normal" => ViewMode::Normal,
        "verbose" => ViewMode::Verbose,
        _ => {
            let mut args = fluent_bundle::FluentArgs::new();
            args.set("mode", view_mode_label(state, state.view_mode));
            state.set_status(state.i18n.format("status-view-usage", Some(&args)));
            return;
        }
    };
    state.view_mode = mode;
    save_view_mode(mode);
    let mut args = fluent_bundle::FluentArgs::new();
    args.set("mode", view_mode_label(state, mode));
    state.set_status(state.i18n.format("status-view-set", Some(&args)));
    state.dirty = true;
}

fn view_mode_label(state: &AppState, mode: ViewMode) -> String {
    state.i18n.t(match mode {
        ViewMode::Minimal => "view-mode-minimal",
        ViewMode::Normal => "view-mode-normal",
        ViewMode::Verbose => "view-mode-verbose",
    })
}

#[cfg(not(test))]
fn save_view_mode(mode: ViewMode) {
    let path = super::view_mode_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string(&mode) {
        let _ = std::fs::write(path, json);
    }
}

#[cfg(test)]
fn save_view_mode(_mode: ViewMode) {}

fn set_approval_mode(state: &mut AppState, text: &str) {
    let arg = text.split_whitespace().nth(1);
    state.permission_mode = match arg {
        Some("auto") | Some("ask") => PermissionMode::Auto,
        Some("deny") | Some("read-only") | Some("readonly") => PermissionMode::Deny,
        Some("all") | Some("approve-all") | Some("bypass") => PermissionMode::ApproveAll,
        Some(_) => {
            state.set_status(state.i18n.t("status-approval-usage"));
            return;
        }
        None => match state.permission_mode {
            PermissionMode::Auto => PermissionMode::Deny,
            PermissionMode::Deny => PermissionMode::ApproveAll,
            PermissionMode::ApproveAll => PermissionMode::Auto,
        },
    };
    let mut args = fluent_bundle::FluentArgs::new();
    args.set("mode", permission_label(state.permission_mode));
    state.set_status(state.i18n.format("status-approval-mode", Some(&args)));
}

fn set_language(state: &mut AppState, text: &str, session: &dyn CoreSession) {
    let Some(arg) = text.split_whitespace().nth(1) else {
        state.set_status(state.i18n.t("status-lang-usage"));
        return;
    };
    let Ok(locale) = Locale::parse(arg) else {
        state.set_status(state.i18n.t("status-lang-usage"));
        return;
    };
    if session.set_language(locale.tag()) {
        state.set_locale(locale);
        let mut args = fluent_bundle::FluentArgs::new();
        args.set("locale", locale.tag());
        state.set_status(state.i18n.format("status-lang-updated", Some(&args)));
    } else {
        let mut args = fluent_bundle::FluentArgs::new();
        args.set("locale", locale.tag());
        state.set_status(state.i18n.format("status-lang-failed", Some(&args)));
    }
}

fn permission_label(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Auto => "auto",
        PermissionMode::Deny => "deny",
        PermissionMode::ApproveAll => "all",
    }
}

fn permission_message(state: &AppState, target: &str, reason: &str, risk: Option<Risk>) -> String {
    let risk = risk
        .map(|risk| match risk {
            Risk::Low => "low",
            Risk::Medium => "medium",
            Risk::High => "high",
        })
        .unwrap_or("unknown");
    let shortcuts = state.i18n.t("permission-shortcuts");
    if reason.is_empty() {
        format!("{target} · risk {risk} · {shortcuts}")
    } else {
        format!("{target} · {reason} · risk {risk} · {shortcuts}")
    }
}

fn opt_hint(hint: String) -> Option<String> {
    let hint = hint.trim().to_string();
    (!hint.is_empty()).then_some(hint)
}

fn parse_text_format(raw: &str) -> TextFormat {
    match raw.trim().to_ascii_lowercase().as_str() {
        "number" | "num" | "float" | "decimal" => TextFormat::Number,
        "integer" | "int" => TextFormat::Integer,
        _ => TextFormat::Any,
    }
}

fn form_field_from_spec(spec: FormFieldSpec) -> FormField {
    match spec {
        FormFieldSpec::Text {
            name,
            label,
            placeholder,
            value,
            hint,
            required,
            format,
        } => FormField {
            name,
            label,
            value: FormValue::Text {
                cursor: value.chars().count(),
                value,
                placeholder,
                format: parse_text_format(&format),
            },
            hint: opt_hint(hint),
            required,
        },
        FormFieldSpec::Checkbox {
            name,
            label,
            checked,
            hint,
        } => FormField {
            name,
            label,
            value: FormValue::Checkbox { checked },
            hint: opt_hint(hint),
            required: false,
        },
        FormFieldSpec::Select {
            name,
            label,
            options,
            selected,
            hint,
        } => FormField {
            name,
            label,
            value: FormValue::Select {
                selected: selected.min(options.len().saturating_sub(1)),
                options,
            },
            hint: opt_hint(hint),
            required: false,
        },
        FormFieldSpec::MultiSelect {
            name,
            label,
            options,
            selected,
            hint,
            required,
        } => FormField {
            name,
            label,
            value: FormValue::MultiSelect {
                cursor: selected
                    .first()
                    .copied()
                    .unwrap_or(0)
                    .min(options.len().saturating_sub(1)),
                selected: selected
                    .into_iter()
                    .filter(|idx| *idx < options.len())
                    .collect(),
                options,
            },
            hint: opt_hint(hint),
            required,
        },
    }
}

fn on_stream(state: &mut AppState, ev: CoreEvent, session: &dyn CoreSession) {
    state.dirty = true;
    match ev {
        CoreEvent::TurnStart { turn_id, proactive } => {
            if proactive {
                state.live = super::Live {
                    active: false,
                    turn_id: Some(turn_id),
                    ..Default::default()
                };
            } else {
                state.live = super::Live {
                    active: true,
                    turn_id: Some(turn_id),
                    ..Default::default()
                };
                state.mode = Mode::Streaming;
                ensure_assistant_history_header(state);
            }
        }
        CoreEvent::RoundStart { turn_id, round_id } => {
            if state.live.turn_id.as_deref() != Some(turn_id.as_str()) {
                state.live = super::Live {
                    active: true,
                    turn_id: Some(turn_id),
                    ..Default::default()
                };
                state.mode = Mode::Streaming;
                ensure_assistant_history_header(state);
            }
            state.live.start_round(round_id);
        }
        CoreEvent::ThinkingStart { .. } => {
            state.live.current_round_mut().start_thinking();
        }
        CoreEvent::ThinkingDelta { text, .. } => {
            let thinking = state.live.current_round_mut().thinking_mut();
            thinking.text.push_str(&text);
            if thinking.text.chars().count() > THINKING_TAIL {
                let tail: String = thinking
                    .text
                    .chars()
                    .rev()
                    .take(THINKING_TAIL)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                thinking.text = tail;
            }
            state.pending_stream_only = true;
        }
        CoreEvent::ThinkingEnd { .. } => {
            state.live.current_round_mut().end_thinking();
        }
        CoreEvent::ToolCallStart { id, name, args } => {
            let arg = args
                .get("path")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_default();
            state
                .live
                .current_round_mut()
                .blocks
                .push(RoundBlock::Tool(ToolRow {
                    id,
                    name,
                    arg,
                    params: Some(args.to_string()),
                    ok: None,
                    result: None,
                }));
        }
        CoreEvent::ToolCallEnd { id, ok, summary } => {
            if let Some(row) = state
                .live
                .rounds
                .iter_mut()
                .flat_map(|round| round.blocks.iter_mut())
                .filter_map(|block| match block {
                    RoundBlock::Tool(row) => Some(row),
                    _ => None,
                })
                .find(|row| row.id == id)
            {
                row.ok = Some(ok);
                row.result = Some(summary);
            }
        }
        CoreEvent::TextDelta { text, .. } => {
            state.live.current_round_mut().append_text(text);
            state.pending_stream_only = true;
        }
        CoreEvent::SubAgentStart {
            agent_name, task, ..
        } => {
            state
                .live
                .current_round_mut()
                .blocks
                .push(RoundBlock::Note(format!(
                    "{} {agent_name} · {task}",
                    Glyph::Subagent.render(state.icons)
                )));
        }
        CoreEvent::SubAgentEnd { summary, .. } => {
            state
                .live
                .current_round_mut()
                .blocks
                .push(RoundBlock::Note(format!(
                    "  {} {summary}",
                    Glyph::ResultBranch.render(state.icons)
                )));
        }
        CoreEvent::SessionTitleUpdate { title } => {
            state.status_snapshot.session_title = Some(title.clone());
        }
        CoreEvent::Compaction {
            replaces,
            summary,
            summary_tokens,
        } => {
            push_compaction_history(state, replaces, &summary);
            state.status_snapshot.context_is_live = true;
            if let Some(summary_tokens) = summary_tokens {
                state.status_snapshot.context_used_tokens = summary_tokens;
            }
        }
        CoreEvent::BusNotification {
            level,
            title,
            message,
            source,
        } => {
            state.push_notification(Notification {
                level,
                title,
                message,
                source,
                sticky: false,
            });
        }
        CoreEvent::Mailbox { entry } => {
            // Stream and event-bus channels are independent. Remove the id now
            // to avoid one frame showing it as both pending and consumed; the
            // following MailboxChanged still replaces the snapshot from core.
            state.live.mailbox.retain(|pending| pending.id != entry.id);
            state
                .live
                .current_round_mut()
                .blocks
                .push(RoundBlock::Mailbox(entry));
        }
        CoreEvent::Usage { snapshot } => {
            state.status_snapshot.total_tokens = snapshot.total_tokens;
            state.status_snapshot.context_used_tokens = snapshot.context_used_tokens;
            state.status_snapshot.context_limit_tokens = snapshot.context_limit_tokens;
            state.status_snapshot.context_is_live = true;
        }

        CoreEvent::PermissionRequest {
            id,
            action,
            target,
            reason,
            risk,
            ..
        } => {
            state.pending_interaction = Some(PendingInteraction::Permission {
                id,
                action: action.clone(),
                target: target.clone(),
                reason: reason.clone(),
                risk,
                selected: 0,
            });
            let mut title_args = fluent_bundle::FluentArgs::new();
            title_args.set("action", action.as_str());
            state.push_notification(Notification {
                level: NotificationLevel::Warning,
                title: state
                    .i18n
                    .format("notification-permission-title", Some(&title_args)),
                message: permission_message(state, &target, &reason, risk),
                source: Some("turn".into()),
                sticky: true,
            });
        }
        CoreEvent::ConfirmationRequest { id, message } => {
            state.pending_interaction = Some(PendingInteraction::Confirmation {
                id,
                message: message.clone(),
                selected: 0,
            });
            state.push_notification(Notification {
                level: NotificationLevel::Info,
                title: state.i18n.t("notification-confirm-title"),
                message,
                source: Some("turn".into()),
                sticky: true,
            });
        }
        CoreEvent::InputRequest { id, prompt } => {
            restore_form(
                state,
                id,
                state.i18n.t("notification-input-title"),
                vec![FormItem {
                    title: prompt.clone(),
                    fields: vec![FormField {
                        name: "text".into(),
                        label: prompt.clone(),
                        value: FormValue::Text {
                            value: String::new(),
                            placeholder: String::new(),
                            cursor: 0,
                            format: TextFormat::Any,
                        },
                        hint: None,
                        required: false,
                    }],
                    focus: 0,
                }],
                0,
            );
            state.push_notification(Notification {
                level: NotificationLevel::Info,
                title: state.i18n.t("notification-input-title"),
                message: prompt,
                source: Some("turn".into()),
                sticky: true,
            });
        }
        CoreEvent::FormRequest {
            id,
            title,
            fields,
            items,
        } => {
            let form_items = if items.is_empty() {
                vec![FormItem {
                    title: title.clone(),
                    fields: fields.into_iter().map(form_field_from_spec).collect(),
                    focus: 0,
                }]
            } else {
                items
                    .into_iter()
                    .map(|item| FormItem {
                        title: item.title,
                        fields: item.fields.into_iter().map(form_field_from_spec).collect(),
                        focus: 0,
                    })
                    .collect()
            };
            restore_form(state, id, title.clone(), form_items, 0);
            state.push_notification(Notification {
                level: NotificationLevel::Info,
                title,
                message: state.i18n.t("notification-form-message"),
                source: Some("turn".into()),
                sticky: true,
            });
        }

        CoreEvent::TurnDone { turn_id } => {
            if state.cancel_requested {
                settle(state, true, session);
                state.set_status(state.i18n.t("status-canceled"));
                state.refresh_status_snapshot(session);
            } else {
                state.live.awaiting_summary = true;
                session.send(Command::RequestTurnSummary { turn_id });
            }
        }
        CoreEvent::TurnSummaryLoaded { summary } => {
            if state.live.turn_id.as_deref() == Some(summary.turn_id.as_str()) {
                state.status_snapshot.context_used_tokens = summary.context_used_tokens;
                state.status_snapshot.context_limit_tokens = summary.context_limit_tokens;
                state.status_snapshot.context_is_live = true;
                state.live.summary = Some(summary);
                settle(state, false, session);
                state.refresh_status_snapshot(session);
            }
        }
        CoreEvent::Error { message } => {
            state.push_notification(Notification {
                level: NotificationLevel::Error,
                title: state.i18n.t("notification-api-error-title"),
                message: message.clone(),
                source: Some("core".into()),
                sticky: false,
            });
            push_classified_history(state, LogKind::Error, format!("error: {message}"));
            settle(state, true, session);
            state.refresh_status_snapshot(session);
        }
    }
}

/// Round complete → flush a summary + the reply into scrollback, clear live.
fn settle(state: &mut AppState, errored: bool, session: &dyn CoreSession) {
    if !state.live.active {
        state.cancel_requested = false;
        state.pending_stream_only = false;
        start_next_queued_turn(state, session);
        state.dirty = true;
        return;
    }
    if !errored {
        ensure_assistant_history_header(state);
        let rounds = std::mem::take(&mut state.live.rounds);
        let answer = rounds.iter().rev().find_map(|round| {
            round.blocks.iter().rev().find_map(|block| match block {
                RoundBlock::Text(text) if !text.trim().is_empty() => Some(text.clone()),
                _ => None,
            })
        });
        if let Some(text) = answer {
            let text = text.trim().to_string();
            if !text.is_empty() {
                push_message_markdown_history(state, &text);
                state.last_reply = Some(text);
            }
        }
    }
    state.live = super::Live::default();
    state.mode = idle_mode(state);
    state.cancel_requested = false;
    state.esc_count = 0;
    state.dirty = true;
    state.pending_stream_only = false;
    start_next_queued_turn(state, session);
}

fn try_start_turn(state: &mut AppState, session: &dyn CoreSession, messages: Vec<Message>) -> bool {
    let started = session.send(Command::TurnStart {
        messages,
        model: None,
        cwd: None,
        plan: state.plan_enabled,
        permission: state.permission_mode,
    });
    if started {
        state.mode = Mode::Streaming;
        state.live = super::Live {
            active: true,
            ..Default::default()
        };
        state.clear_status();
    } else if !session.has_usable_model() {
        state.mode = idle_mode(state);
        state.live = super::Live::default();
        state.set_status_persistent(no_usable_model_status(state, session));
    } else {
        state.mode = idle_mode(state);
        state.live = super::Live::default();
        state.set_status(state.i18n.t("status-turn-not-started"));
    }
    started
}

fn start_next_queued_turn(state: &mut AppState, session: &dyn CoreSession) {
    let Some(queued) = state.next_turn_queue.pop_front() else {
        return;
    };
    push_user_history(state, &queued.messages);
    state.streaming_send_target = StreamingSendTarget::NextTurn;
    if try_start_turn(state, session, queued.messages) {
        state.set_status(state.i18n.t("status-next-turn-started"));
    }
    state.dirty = true;
}

fn ensure_assistant_history_header(state: &mut AppState) {
    if !state.live.assistant_header_emitted {
        push_classified_history(state, LogKind::Assistant, "assistant");
        state.live.assistant_header_emitted = true;
    }
}

fn idle_mode(state: &AppState) -> Mode {
    if state.god_enabled {
        Mode::God
    } else if state.plan_enabled {
        Mode::Plan
    } else {
        Mode::Normal
    }
}

fn push_classified_history(state: &mut AppState, kind: LogKind, text: impl Into<String>) {
    let line = classify::line(kind, text, &state.theme, state.icons);
    state.push_history_line(HistLine::from_line(line));
}

fn push_compaction_history(state: &mut AppState, replaces: (u32, u32), summary: &str) {
    for line in compaction_history_lines(state, replaces, summary) {
        state.push_history_line(line);
    }
    state.dirty = true;
}

pub(crate) fn compaction_history_lines(
    state: &AppState,
    replaces: (u32, u32),
    summary: &str,
) -> Vec<HistLine> {
    classify::compaction_lines(
        compaction_marker(state, replaces),
        summary,
        compaction_tail(state, summary),
        &state.theme,
        state.icons,
    )
    .into_iter()
    .map(HistLine::from_line)
    .collect()
}

fn compaction_marker(state: &AppState, (from, to): (u32, u32)) -> String {
    let mut args = fluent_bundle::FluentArgs::new();
    args.set("from", i64::from(from));
    args.set("to", i64::from(to));
    state.i18n.format("compaction-marker", Some(&args))
}

fn compaction_tail(state: &AppState, summary: &str) -> Option<String> {
    let lines = summary.lines().filter(|l| !l.trim().is_empty()).count();
    let hidden = lines.saturating_sub(classify::SUMMARY_LINES);
    (hidden > 0).then(|| {
        let mut args = fluent_bundle::FluentArgs::new();
        args.set("count", hidden as i64);
        state.i18n.format("compaction-more", Some(&args))
    })
}

fn push_user_history(state: &mut AppState, messages: &[Message]) {
    push_classified_history(state, LogKind::User, "user");
    for message in messages {
        for line in crate::render::timeline::message::user_lines(state, message) {
            state.push_history_line(HistLine::from_line(line));
        }
    }
}

fn push_markdown_history(state: &mut AppState, text: &str) {
    for line in block::render_markdown(text, &state.theme, state.icons) {
        state.push_history_line(HistLine::from_line(line));
    }
}

fn push_message_markdown_history(state: &mut AppState, text: &str) {
    for line in block::render_markdown(text, &state.theme, state.icons) {
        state.push_history_line(HistLine::from_line(indent_line(line, MESSAGE_BODY_INDENT)));
    }
}

fn push_message_notice_history(state: &mut AppState, text: impl Into<String>) {
    let line = classify::line(LogKind::Notice, text, &state.theme, state.icons);
    state.push_history_line(HistLine::from_line(indent_line(line, MESSAGE_BODY_INDENT)));
}

fn push_code_history(state: &mut AppState, text: &str) {
    let lines: Vec<Line<'static>> = text
        .split('\n')
        .map(|line| Line::from(Span::raw(line.to_string())))
        .collect();
    for line in rail::rail_lines(&lines, state.theme.style(Sem::Muted), state.icons) {
        state.push_history_line(HistLine::from_line(line));
    }
}

fn push_message_code_history(state: &mut AppState, text: &str) {
    let lines: Vec<Line<'static>> = text
        .split('\n')
        .map(|line| Line::from(Span::raw(line.to_string())))
        .collect();
    for line in rail::rail_lines(&lines, state.theme.style(Sem::Muted), state.icons) {
        state.push_history_line(HistLine::from_line(indent_line(line, MESSAGE_BODY_INDENT)));
    }
}

fn indent_line(mut line: Line<'static>, spaces: usize) -> Line<'static> {
    if spaces > 0 {
        line.spans.insert(0, Span::raw(" ".repeat(spaces)));
    }
    line
}

// NOTE: the UI-behavior test modules (`update/tests.rs`, `update/composer_tests.rs`,
// `update/zoom_tests.rs`) were removed together with the `--mock` backend they ran
// against; the suite now covers the real engine path only (`session/engine.rs`).
#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::dto::CoreEvent;
    use crate::session::CoreSession;
    use crate::theme::{themes, ColorTier, ThemeState};

    struct NullSession;

    impl CoreSession for NullSession {
        fn subscribe(&self) -> std::sync::mpsc::Receiver<CoreEvent> {
            let (_tx, rx) = std::sync::mpsc::channel();
            rx
        }
        fn send(&self, _cmd: Command) -> bool {
            true
        }
    }

    /// Records the one command these tests care about.
    struct CancelAwareSession(std::sync::atomic::AtomicBool);

    impl CancelAwareSession {
        fn cancelled(&self) -> bool {
            self.0.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl CoreSession for CancelAwareSession {
        fn subscribe(&self) -> std::sync::mpsc::Receiver<CoreEvent> {
            let (_tx, rx) = std::sync::mpsc::channel();
            rx
        }
        fn send(&self, cmd: Command) -> bool {
            if matches!(cmd, Command::TurnCancel) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            true
        }
    }

    fn state() -> AppState {
        let mut s = AppState::new(
            ThemeState {
                theme: themes::dark::theme(),
                tier: ColorTier::Rich,
            },
            crate::glyph::IconTier::Ascii,
            80,
            24,
            24,
        );
        s.mode = Mode::Normal;
        s
    }

    #[test]
    fn ime_commit_burst_is_kept_entirely() {
        let mut s = state();
        let session = NullSession;
        for ch in ['i', 'n', 'p', 'u', 't'] {
            on_key(
                &mut s,
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                &session,
            );
        }
        assert_eq!(
            s.input, "input",
            "a character burst committed by the IME must not be caught by the burst guard"
        );
    }

    #[test]
    fn unescaped_burst_after_esc_is_still_dropped() {
        let mut s = state();
        let session = NullSession;
        on_key(
            &mut s,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &session,
        );
        for ch in ['i', 'n', 'p', 'u', 't'] {
            on_key(
                &mut s,
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                &session,
            );
        }
        assert_eq!(
            s.input, "in",
            "a burst inside the Esc anchor window is still dropped as escape-sequence residue"
        );
    }

    #[test]
    fn burst_after_esc_window_expires_is_kept() {
        let mut s = state();
        let session = NullSession;
        on_key(
            &mut s,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &session,
        );
        s.esc_anchored_at = Some(Instant::now() - Duration::from_millis(200));
        for ch in ['i', 'n', 'p'] {
            on_key(
                &mut s,
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                &session,
            );
        }
        assert_eq!(
            s.input, "inp",
            "a burst after the anchor expires is unaffected by the burst guard"
        );
    }

    #[test]
    fn esc_cancels_a_turn_that_started_from_the_stream() {
        let mut s = state();
        let session = CancelAwareSession(std::sync::atomic::AtomicBool::new(false));
        on_stream(
            &mut s,
            CoreEvent::TurnStart {
                turn_id: "t1".into(),
                proactive: false,
            },
            &session,
        );
        assert_eq!(
            s.mode,
            Mode::Streaming,
            "a turn the CLI did not start itself (mailbox / steering) is streaming all the same"
        );
        for _ in 0..2 {
            on_key(
                &mut s,
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                &session,
            );
        }
        assert!(session.cancelled(), "the second Esc must reach the engine");
    }

    #[test]
    fn settled_history_keeps_only_the_final_answer() {
        let mut s = state();
        s.live.active = true;
        s.live.start_round("r1".into());
        s.live.current_round_mut().append_text("first round".into());
        s.live.start_round("r2".into());
        s.live
            .current_round_mut()
            .append_text("second round".into());
        settle(&mut s, false, &NullSession);

        let lines: Vec<String> = s
            .transcript
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.text.as_str())
                    .collect::<String>()
            })
            .collect();
        let joined = lines.join("\n");
        assert!(
            joined.contains("second round"),
            "the final answer (the last text across all rounds) must land in the history: {lines:?}"
        );
        assert!(
            !joined.contains("first round"),
            "intermediate round bodies no longer land in the history (only in live / replay): {lines:?}"
        );
        assert!(
            !joined.contains("round 1") && !joined.contains("round 2"),
            "round summary headers no longer land in the scrollback: {lines:?}"
        );
    }
}
