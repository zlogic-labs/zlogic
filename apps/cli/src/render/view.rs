//! `view(&AppState, &mut Frame)` — full viewport redraw, a pure function of state.
//! ratatui does cell-level diffing, so full redraw ≠ full terminal
//! rewrite. Today it renders: live area + input box + status line. Overlays come later.

mod models;

use models::render_models_overlay;

use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Wrap};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{
    AppState, ChipKind, FormField, FormValue, FullscreenOverlay, HelpOverlay, Mode,
    ModelOverlayMode, ModelsOverlay, PendingInteraction, SessionDetailMode, Stage, StatsOverlay,
    StatsRange, StreamingSendTarget, SuggestItem, ZoomKind, ZoomOverlay, MAX_INPUT_LINES,
};
use crate::glyph::{braille_spinner_frame, spinner_frame, Glyph, IconTier};
use crate::render::history::{wrap_hist_line, wrap_line, HistLine};
use crate::render::timeline;
use crate::session::dto::{
    CommandKind, KeyStatus, ModelEntry, ModelUsage, NotificationLevel, TurnUsage,
};
use crate::splash;
use crate::term::caps::TerminalKind;
use crate::theme::Sem;
use crate::widgets::chart;
use crate::widgets::table::{self, Align, Column};

/// Columns consumed by the leading `› ` prompt / continuation indent.
const GUTTER: usize = 2;

fn selection_marker(state: &AppState, selected: bool) -> String {
    if selected {
        format!("{} ", Glyph::Prompt.render(state.icons))
    } else {
        "  ".to_string()
    }
}

fn spinner(state: &AppState, frame: usize) -> &'static str {
    spinner_frame(state.icons, frame)
}

/// Braille sweep spinner: composer title bar + live timeline running rows (thinking
fn braille_spinner(state: &AppState) -> &'static str {
    braille_spinner_frame(state.icons, state.spinner_frame)
}

fn overlay_border_type(state: &AppState) -> BorderType {
    match state.icons {
        IconTier::Ascii => BorderType::Plain,
        IconTier::Unicode | IconTier::Nerd => BorderType::Rounded,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OverlayKind {
    None,
    Floating,
    Fullscreen,
}

/// Draw the frame. Returns the cell to PARK the (hidden) hardware cursor at when this
/// frame did not place a visible cursor — some terminals still render a hollow outline
/// for a hidden/unfocused cursor, and without parking it rests wherever cell painting
/// happened to end (e.g. the status bar's right edge). Parking at the caret cell keeps
/// that outline where a caret plausibly lives. `None` = the frame set a visible cursor
/// (or we're in a fullscreen/exit view where parking is moot).
pub fn view(state: &AppState, f: &mut Frame) -> Option<Position> {
    let area = f.area();
    if state.stage == Stage::Exit {
        f.render_widget(Clear, area);
        let lines = exit_lines(state, area.width, area.height);
        f.render_widget(Paragraph::new(lines), area);
        return None;
    }

    if state.overlay_is_fullscreen() {
        f.render_widget(Clear, area);
        render_fullscreen_overlay(state, f, area);
        return None;
    }

    let (wrapped, content_lines, input_h) = input_layout(state, area.width);
    let queue_h = queue_card_height(state);

    let notify = notification_bar_line(state, area.width);
    let notify_h = u16::from(notify.is_some());

    // Toolbar: two fixed layers under the input — the collapsed
    // indicator row (only when it has content) directly above the status line.
    let indicator = toolbar_indicator_line(state);
    let indicator_h = u16::from(indicator.is_some());
    let chunks = Layout::vertical([
        Constraint::Min(0),              // dynamic window: history tail or overlays
        Constraint::Length(queue_h),     // current injection + local follow-up queues
        Constraint::Length(notify_h),    // notification bar: newest toast above input
        Constraint::Length(input_h),     // input box (dynamic)
        Constraint::Length(indicator_h), // toolbar layer 1: collapsed indicators
        Constraint::Length(1),           // toolbar layer 2: status / hint
    ])
    .split(area);

    if queue_h > 0 {
        render_queue_card(state, f, chunks[1]);
    }
    if let Some(line) = notify {
        f.render_widget(Paragraph::new(line), chunks[2]);
    }
    let input_caret = render_input(state, f, chunks[3], &wrapped, content_lines);
    if let Some(line) = indicator {
        f.render_widget(Paragraph::new(line), chunks[4]);
    }
    render_status(state, f, chunks[5]);
    let form_caret = render_dynamic_window(state, f, chunks[0]);

    // A focused form text field owns the hardware cursor (IME anchors to it); else the
    // main input owns it while editable; else hand the cell back for parking.
    if let Some(cell) = form_caret {
        f.set_cursor_position(cell);
        return None;
    }
    if caret_visible(state) {
        f.set_cursor_position(input_caret);
        return None;
    }
    Some(input_caret)
}

pub fn history_tail_height(state: &AppState, width: u16, height: u16) -> u16 {
    let (_, _, input_h) = input_layout(state, width);
    let indicator_h = u16::from(toolbar_indicator_line(state).is_some());
    let notify_h = u16::from(notification_bar_line(state, width).is_some());
    let queue_h = queue_card_height(state);
    height
        .saturating_sub(input_h)
        .saturating_sub(1)
        .saturating_sub(indicator_h)
        .saturating_sub(queue_h)
        .saturating_sub(notify_h)
}

fn queue_card_height(state: &AppState) -> u16 {
    let count = state.live.mailbox.len() + state.next_turn_queue.len();
    if count == 0 {
        0
    } else {
        (count as u16).min(4) + 2
    }
}

fn render_queue_card(state: &AppState, f: &mut Frame, area: Rect) {
    let th = &state.theme;
    let target = state.i18n.t(match state.streaming_send_target {
        StreamingSendTarget::Mailbox => "queue-current",
        StreamingSendTarget::NextTurn => "queue-next",
    });
    let mut hint_args = fluent_bundle::FluentArgs::new();
    hint_args.set("target", target.clone());
    let title = format!(
        " {} · {} ",
        state.i18n.t("queue-title"),
        state.i18n.format("queue-hint", Some(&hint_args))
    );
    let border_sem = match state.streaming_send_target {
        StreamingSendTarget::Mailbox => Sem::Warning,
        StreamingSendTarget::NextTurn => Sem::Info,
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(overlay_border_type(state))
        .border_style(th.style(border_sem))
        .padding(Padding::horizontal(1))
        .title(Span::styled(
            title,
            th.style(border_sem).add_modifier(Modifier::BOLD),
        ));
    let width = block.inner(area).width.saturating_sub(2) as usize;
    let mut lines = Vec::new();
    for entry in &state.live.mailbox {
        lines.push(Line::from(vec![
            Span::styled(
                format!(
                    "{} {} · ",
                    Glyph::Steering.render(state.icons),
                    state.i18n.t("queue-current")
                ),
                th.style(Sem::Warning).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                truncate_display(&entry.preview(), width.saturating_sub(12)),
                th.style(Sem::Muted),
            ),
        ]));
    }
    for queued in &state.next_turn_queue {
        lines.push(Line::from(vec![
            Span::styled(
                format!(
                    "{} {} · ",
                    Glyph::Prompt.render(state.icons),
                    state.i18n.t("queue-next")
                ),
                th.style(Sem::Info).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                truncate_display(&queued.display, width.saturating_sub(10)),
                th.style(Sem::Muted),
            ),
        ]));
    }
    let visible = lines
        .into_iter()
        .rev()
        .take(area.height.saturating_sub(2) as usize)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>();
    f.render_widget(Paragraph::new(visible).block(block), area);
}

/// Toolbar layer 1: collapsed indicator badges, rendered ONLY when at
/// least one source has content. Today the only AppState-backed source is the
/// notification queue (`⚑ N`). Mailbox entries have their own rows immediately
/// above the composer and therefore do not collapse into this indicator.
fn toolbar_indicator_line(state: &AppState) -> Option<Line<'static>> {
    if state.notifications.is_empty() {
        return None;
    }
    let th = &state.theme;
    let sem = if state
        .notifications
        .iter()
        .any(|n| n.level == NotificationLevel::Error)
    {
        Sem::Error
    } else if state
        .notifications
        .iter()
        .any(|n| n.level == NotificationLevel::Warning)
    {
        Sem::Warning
    } else {
        Sem::Muted
    };
    let mut args = fluent_bundle::FluentArgs::new();
    args.set("glyph", Glyph::Notice.render(state.icons));
    args.set("count", state.notifications.len() as i64);
    Some(Line::from(Span::styled(
        state.i18n.format("toolbar-notifications", Some(&args)),
        th.style(sem),
    )))
}

fn notification_bar_line(state: &AppState, width: u16) -> Option<Line<'static>> {
    let th = &state.theme;
    if let Some(armed) = state.quit_armed_at {
        if std::time::Instant::now().duration_since(armed) < std::time::Duration::from_secs(2) {
            let text = state.i18n.t("status-quit-confirm");
            let text = truncate_display(&text, (width as usize).saturating_sub(6));
            return Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    format!("{} ", Glyph::Notice.render(state.icons)),
                    th.style(Sem::Warning).add_modifier(Modifier::BOLD),
                ),
                Span::styled(text, th.style(Sem::Warning)),
            ])
            .into();
        }
    }
    let status_text = if !state.status.is_empty() {
        Some(state.status.clone())
    } else if state.mode == Mode::Normal
        && state.pending_interaction.is_none()
        && state.last_reply.is_some()
    {
        Some(state.i18n.t("status-zoom-hint"))
    } else {
        None
    };
    if let Some(text) = status_text {
        let text = truncate_display(&text, (width as usize).saturating_sub(6));
        return Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("{} ", Glyph::Notice.render(state.icons)),
                th.style(Sem::Info).add_modifier(Modifier::BOLD),
            ),
            Span::styled(text, th.style(Sem::Info)),
        ])
        .into();
    }
    let notification = state.notifications.back()?;
    if state.pending_interaction.is_some() {
        return None;
    }
    let sem = match notification.level {
        NotificationLevel::Info => Sem::Info,
        NotificationLevel::Success => Sem::Success,
        NotificationLevel::Warning => Sem::Warning,
        NotificationLevel::Error => Sem::Error,
    };
    let title = if let Some(source) = &notification.source {
        format!("{} · {}", source, notification.title)
    } else {
        notification.title.clone()
    };
    let mut text = title;
    if !notification.message.is_empty() {
        text.push_str("  ");
        text.push_str(&notification.message);
    }
    let text = truncate_display(&text, (width as usize).saturating_sub(6));
    Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("{} ", Glyph::Notice.render(state.icons)),
            th.style(sem).add_modifier(Modifier::BOLD),
        ),
        Span::styled(text, th.style(sem)),
    ])
    .into()
}

/// Columns of chrome the input box eats before text: 2 borders + 2 horizontal padding.
const INPUT_CHROME: usize = 4;

fn input_layout(state: &AppState, width: u16) -> (Vec<String>, u16, u16) {
    // Input grows with content (cap 4 lines, then scroll internally).
    // Height must be known before the vertical split, so wrap against the full width now.
    let text_w = (width as usize)
        .saturating_sub(GUTTER + INPUT_CHROME)
        .max(1);
    let wrapped = wrap_line(&state.input, text_w);
    let content_lines = (wrapped.len() as u16).clamp(1, MAX_INPUT_LINES);
    let input_h = content_lines + 2; // + rounded borders
    (wrapped, content_lines, input_h)
}

fn render_dynamic_window(
    state: &AppState,
    f: &mut Frame,
    area: ratatui::layout::Rect,
) -> Option<Position> {
    let overlay = overlay_kind(state);

    if state.stage == Stage::Boot {
        let lines = boot_lines(state, area.width, area.height);
        f.render_widget(Paragraph::new(lines), area);
        return None;
    }

    // History tail is ALREADY wrapped (wrapped_history_lines). Live/streaming lines are
    // built fresh here, so wrap them the SAME way, then render 1:1 with NO Paragraph
    // re-wrap. Re-wrapping pre-wrapped lines used to inflate blank/indented rows (each
    // message-indented blank line re-wrapped into two) and, because Paragraph is
    // top-anchored, the surplus clipped the NEWEST reply lines off the bottom — they
    // vanished behind the input box.
    let wrap_w = (area.width as usize).clamp(1, 120);
    let mut lines = state.history_tail_lines(area.height as usize);
    for line in live_lines(state) {
        lines.extend(wrap_hist_line(&HistLine::from_line(line), wrap_w, wrap_w));
    }
    lines.push(Line::from(String::new()));

    // Keep the LAST `area.height` visual rows so the newest content sits at the bottom.
    if lines.len() > area.height as usize {
        let start = lines.len().saturating_sub(area.height as usize);
        lines = lines.split_off(start);
    }

    let p = Paragraph::new(lines);
    f.render_widget(p, area);
    let form_caret = render_notification_overlay(state, f, area);

    if overlay == OverlayKind::Floating {
        render_floating_overlay(state, f, area);
    }
    form_caret
}

fn splash_top_padding(dynamic_height: u16, tip_count: usize, icons: IconTier) -> usize {
    const INPUT_AND_STATUS_ROWS: u16 = 4;
    splash::top_padding(
        dynamic_height.saturating_add(INPUT_AND_STATUS_ROWS),
        tip_count,
        INPUT_AND_STATUS_ROWS,
        icons,
    ) as usize
}

fn top_padded_lines(lines: Vec<Line<'static>>, top: usize, height: usize) -> Vec<Line<'static>> {
    let mut out = Vec::with_capacity(height.max(lines.len() + top));
    out.extend((0..top).map(|_| Line::from(String::new())));
    out.extend(lines);
    out
}

fn overlay_kind(state: &AppState) -> OverlayKind {
    if state.overlay_is_fullscreen() {
        OverlayKind::Fullscreen
    } else if state.overlay_is_floating() {
        OverlayKind::Floating
    } else {
        OverlayKind::None
    }
}

fn render_floating_overlay(state: &AppState, f: &mut Frame, area: Rect) {
    render_command_overlay(state, f, area);
    render_file_picker_overlay(state, f, area);
}

fn render_notification_overlay(state: &AppState, f: &mut Frame, area: Rect) -> Option<Position> {
    if state.pending_interaction.is_none() {
        return None;
    }
    let notification = state.notifications.back()?;
    if area.height < 3 || area.width < 20 {
        return None;
    }
    let th = &state.theme;
    let color = match notification.level {
        NotificationLevel::Info => th.color(Sem::Info),
        NotificationLevel::Success => th.color(Sem::Success),
        NotificationLevel::Warning => th.color(Sem::Warning),
        NotificationLevel::Error => th.color(Sem::Error),
    };
    let title = if let Some(source) = &notification.source {
        format!(" {} · {} ", source, notification.title)
    } else {
        format!(" {} ", notification.title)
    };
    let text_w = area.width.saturating_sub(4) as usize;
    // Body budget: the overlay caps at the dynamic window height; borders eat 2 rows.
    let max_lines = area.height.saturating_sub(2) as usize;
    let (mut lines, form_caret) = if let Some(pending) = &state.pending_interaction {
        interaction_form_lines(state, pending, text_w, max_lines)
    } else {
        let lines = wrap_line(&notification.message, text_w)
            .into_iter()
            .take(3)
            .map(|line| Line::from(Span::styled(line, th.style(Sem::Muted))))
            .collect();
        (lines, None)
    };
    if lines.is_empty() {
        lines.push(Line::from(String::new()));
    }
    let height = (lines.len() as u16 + 2).min(area.height);
    let rect = Rect::new(
        area.x,
        area.y + area.height.saturating_sub(height),
        area.width,
        height,
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(overlay_border_type(state))
        .border_style(Style::default().fg(color))
        // 1 col of breathing room inside the border on each side. `text_w` above already
        // reserves 4 cols of chrome (2 border + 2 padding), so lines fit exactly.
        .padding(Padding::horizontal(1))
        .title(Span::styled(
            title,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));

    f.render_widget(Clear, rect);
    f.render_widget(Paragraph::new(lines).block(block), rect);

    // A focused form text field anchors the hardware cursor (IME composition windows
    // follow it — same rationale as the main input). Content starts at border(1) +
    // padding(1). Drop it if the row was clipped.
    form_caret.and_then(|(line_idx, col)| {
        let x = rect.x + 2 + col;
        let y = rect.y + 1 + line_idx as u16;
        let inside = y + 1 < rect.y + rect.height && x + 1 < rect.x + rect.width;
        inside.then_some(Position::new(x, y))
    })
}

fn interaction_form_lines(
    state: &AppState,
    pending: &PendingInteraction,
    text_w: usize,
    max_lines: usize,
) -> (Vec<Line<'static>>, Option<(usize, u16)>) {
    let th = &state.theme;
    match pending {
        PendingInteraction::Permission {
            action,
            target,
            reason,
            risk,
            selected,
            ..
        } => {
            let mut lines = vec![
                form_field(state, "Action", action),
                form_field(state, "Target", target),
            ];
            if !reason.is_empty() {
                lines.push(form_field(state, "Reason", reason));
            }
            let risk = risk
                .map(|risk| match risk {
                    crate::session::dto::Risk::Low => "low",
                    crate::session::dto::Risk::Medium => "medium",
                    crate::session::dto::Risk::High => "high",
                })
                .unwrap_or("unknown");
            lines.push(form_field(state, "Risk", risk));
            lines.extend(form_options_owned(
                state,
                &[
                    state.i18n.t("form-option-allow-once"),
                    state.i18n.t("form-option-allow-session"),
                    state.i18n.t("form-option-deny"),
                ],
                *selected,
                0,
            ));
            lines.push(form_hint(state, &state.i18n.t("form-hint-permission")));
            (lines, None)
        }
        PendingInteraction::Confirmation {
            message, selected, ..
        } => {
            let mut lines: Vec<Line<'static>> = wrap_line(message, text_w)
                .into_iter()
                .take(2)
                .map(|line| Line::from(Span::styled(line, th.style(Sem::Muted))))
                .collect();
            lines.extend(form_options_owned(
                state,
                &[
                    state.i18n.t("form-option-cancel"),
                    state.i18n.t("form-option-confirm"),
                ],
                *selected,
                0,
            ));
            lines.push(form_hint(state, &state.i18n.t("form-hint-confirm")));
            (lines, None)
        }
        PendingInteraction::Form {
            title,
            items,
            current,
            ..
        } => form_lines(state, title, items, *current, text_w, max_lines),
    }
}

/// Max option rows shown for an expanded (focused) select / multi-select; the window
/// slides with the cursor and clipped edges get a "more above/below" marker row.
const FORM_OPTION_WINDOW: usize = 5;

/// The whole current step at a glance: every field is a row (`› label value`), the
/// focused one is highlighted and — for selects — expanded inline. Returns the lines
/// plus the caret (line index, column) of the focused text field, if any.
fn form_lines(
    state: &AppState,
    title: &str,
    items: &[crate::app::FormItem],
    current: usize,
    text_w: usize,
    max_lines: usize,
) -> (Vec<Line<'static>>, Option<(usize, u16)>) {
    let th = &state.theme;
    let item_count = items.len().max(1);
    let current_idx = current.min(item_count.saturating_sub(1));
    let item = items.get(current_idx);

    let mut header = vec![
        Span::styled(
            "ASK USER",
            Style::default()
                .fg(th.color(Sem::Accent))
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(title.to_string(), th.style(Sem::Muted)),
    ];
    if item_count > 1 {
        header.push(Span::styled(
            format!("  {}/{}", current_idx + 1, item_count),
            th.style(Sem::Muted),
        ));
        if let Some(item) = item {
            header.push(Span::styled(
                format!(" · {}", item.title),
                Style::default()
                    .fg(th.color(Sem::AccentSoft))
                    .add_modifier(Modifier::BOLD),
            ));
        }
    }
    let mut lines = vec![Line::from(header), Line::from(String::new())];
    let mut caret: Option<(usize, u16)> = None;

    let Some(item) = item else {
        lines.push(form_hint(state, &state.i18n.t("form-hint-submit-skip")));
        return (lines, caret);
    };

    let label_w = item
        .fields
        .iter()
        .map(|field| UnicodeWidthStr::width(field.label.as_str()))
        .max()
        .unwrap_or(4)
        .clamp(4, 16);

    // Render each field as its own group, then window whole fields around the focused
    // one so a tall form never overflows the overlay (task: long-form scrolling).
    let groups: Vec<(Vec<Line<'static>>, Option<u16>)> = item
        .fields
        .iter()
        .enumerate()
        .map(|(idx, field)| form_field_rows(state, field, idx == item.focus, label_w, text_w))
        .collect();
    let heights: Vec<usize> = groups.iter().map(|(g, _)| g.len().max(1)).collect();
    let total: usize = heights.iter().sum();

    // Budget = overlay body minus the already-pushed header/blank (top) and the
    // blank+hint appended below.
    let field_budget = max_lines.saturating_sub(lines.len() + 2).max(1);
    let (lo, hi) = if total <= field_budget {
        (0, groups.len())
    } else {
        // Grow a window outward from the focused field; reserve 2 rows for markers.
        let budget = field_budget.saturating_sub(2).max(1);
        let focus = item.focus.min(groups.len().saturating_sub(1));
        let (mut lo, mut hi, mut used) = (focus, focus + 1, heights[focus]);
        loop {
            let down = hi < groups.len() && used + heights[hi] <= budget;
            let up = lo > 0 && used + heights[lo - 1] <= budget;
            if down {
                used += heights[hi];
                hi += 1;
            } else if up {
                lo -= 1;
                used += heights[lo];
            } else {
                break;
            }
        }
        (lo, hi)
    };

    if lo > 0 {
        lines.push(form_scroll_marker(state, "form-fields-above", lo));
    }
    for (group, ccol) in &groups[lo..hi] {
        if let Some(col) = ccol {
            caret = Some((lines.len(), *col));
        }
        lines.extend(group.iter().cloned());
    }
    if hi < groups.len() {
        lines.push(form_scroll_marker(
            state,
            "form-fields-below",
            groups.len() - hi,
        ));
    }

    lines.push(Line::from(String::new()));
    let last_field = item.focus + 1 >= item.fields.len() && current_idx + 1 >= item_count;
    let enter = if last_field {
        state.i18n.t("form-enter-submit")
    } else {
        state.i18n.t("form-enter-next")
    };
    let hint = match item.fields.get(item.focus).map(|f| &f.value) {
        Some(FormValue::Text { .. }) => form_hint_with_enter(state, "form-hint-text", &enter),
        Some(FormValue::Checkbox { .. }) => {
            form_hint_with_enter(state, "form-hint-checkbox", &enter)
        }
        Some(FormValue::Select { .. }) => form_hint_with_enter(state, "form-hint-select", &enter),
        Some(FormValue::MultiSelect { .. }) => {
            form_hint_with_enter(state, "form-hint-multiselect", &enter)
        }
        None => form_hint_with_enter(state, "form-hint-default", &enter),
    };
    lines.push(form_hint(state, &hint));
    (lines, caret)
}

/// One field rendered as a self-contained line group (value row first, then any
/// expanded options and the focused field's hint/warning). Returns the group plus the
/// caret COLUMN for a focused text field (its row is always the group's first line).
/// Returning a group — rather than pushing into a shared buffer — lets `form_lines`
/// window whole fields for scrolling.
fn form_field_rows(
    state: &AppState,
    field: &FormField,
    focused: bool,
    label_w: usize,
    text_w: usize,
) -> (Vec<Line<'static>>, Option<u16>) {
    let th = &state.theme;
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut caret: Option<u16> = None;
    let marker = Span::styled(
        selection_marker(state, focused),
        Style::default()
            .fg(th.color(Sem::Accent))
            .add_modifier(Modifier::BOLD),
    );
    // A dedicated 1-col slot for the required mark keeps every field's value column
    // aligned whether or not the field is required.
    let req_mark = Span::styled(
        if field.required {
            state.i18n.t("form-required-mark")
        } else {
            " ".into()
        },
        Style::default().fg(th.color(Sem::Warning)),
    );
    let pad = label_w.saturating_sub(UnicodeWidthStr::width(field.label.as_str()));
    let label = Span::styled(
        format!("{}{}  ", field.label, " ".repeat(pad)),
        th.style(if focused { Sem::AccentSoft } else { Sem::Muted })
            .add_modifier(if focused {
                Modifier::BOLD
            } else {
                Modifier::empty()
            }),
    );
    // marker(2) + req-mark(1) + label + gap(2) columns before the value.
    let prefix_cols = 2 + 1 + label_w + 2;
    let avail = text_w.saturating_sub(prefix_cols).max(8);

    let mut row = vec![marker, req_mark, label];
    match &field.value {
        FormValue::Text {
            value,
            placeholder,
            cursor,
            ..
        } => {
            if focused {
                let (visible, caret_cols) = text_window(value, *cursor, avail);
                row.push(Span::styled(visible, th.style(Sem::Muted)));
                if value.is_empty() && !placeholder.is_empty() {
                    // Ghost hint under the caret; the first keystroke replaces it.
                    row.push(Span::styled(placeholder.clone(), th.style(Sem::Muted)));
                }
                caret = Some((prefix_cols as u16) + caret_cols);
            } else if value.is_empty() {
                let ghost = if placeholder.is_empty() {
                    state.i18n.t("form-empty")
                } else {
                    placeholder.clone()
                };
                row.push(Span::styled(ghost, th.style(Sem::Muted)));
            } else {
                let (visible, _) = text_window(value, value.chars().count(), avail);
                row.push(Span::styled(visible, th.style(Sem::Muted)));
            }
            lines.push(Line::from(row));
        }
        FormValue::Checkbox { checked } => {
            row.push(Span::styled(
                if *checked {
                    state.i18n.t("form-toggle-on")
                } else {
                    state.i18n.t("form-toggle-off")
                },
                th.style(if *checked { Sem::Success } else { Sem::Muted })
                    .add_modifier(if focused {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ));
            lines.push(Line::from(row));
        }
        FormValue::Select { options, selected } => {
            let value = options.get(*selected).cloned().unwrap_or_default();
            if focused {
                row.push(Span::styled(
                    format!("{value}  ({}/{})", selected + 1, options.len().max(1)),
                    Style::default()
                        .fg(th.color(Sem::Info))
                        .add_modifier(Modifier::BOLD),
                ));
                lines.push(Line::from(row));
                form_option_rows(state, options, Some(*selected), None, &mut lines);
            } else {
                row.push(Span::styled(
                    value,
                    Style::default().fg(th.color(Sem::Info)),
                ));
                lines.push(Line::from(row));
            }
        }
        FormValue::MultiSelect {
            options,
            selected,
            cursor,
        } => {
            let labels: Vec<&str> = selected
                .iter()
                .filter_map(|idx| options.get(*idx))
                .map(|s| s.as_str())
                .collect();
            let summary = if labels.is_empty() {
                state.i18n.t("form-none-selected")
            } else {
                let mut args = fluent_bundle::FluentArgs::new();
                args.set("labels", labels.join(", "));
                args.set("count", labels.len() as i64);
                state.i18n.format("form-selected-count", Some(&args))
            };
            row.push(Span::styled(
                summary,
                th.style(if labels.is_empty() {
                    Sem::Muted
                } else {
                    Sem::Info
                })
                .add_modifier(if focused {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
            ));
            lines.push(Line::from(row));
            if focused {
                form_option_rows(state, options, None, Some((selected, *cursor)), &mut lines);
            }
        }
    }

    // Focused field's description + advisory warning, indented under the label. Only on
    // focus to keep the form compact; the required `*` is the always-visible cue.
    if focused {
        if let Some(hint) = &field.hint {
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(hint.clone(), th.style(Sem::Muted)),
            ]));
        }
        if let Some(warn) = field.warning() {
            let key = match warn {
                crate::app::FieldWarning::Required => "form-warn-required",
                crate::app::FieldWarning::NotNumber => "form-warn-number",
                crate::app::FieldWarning::NotInteger => "form-warn-integer",
            };
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(
                    state.i18n.t(key),
                    Style::default().fg(th.color(Sem::Warning)),
                ),
            ]));
        }
    }

    (lines, caret)
}

/// Expanded option rows with a sliding window (`FORM_OPTION_WINDOW`); pass `single`
/// for a select (cursor == selection) or `multi` for a multi-select (checkmarks).
fn form_option_rows(
    state: &AppState,
    options: &[String],
    single: Option<usize>,
    multi: Option<(&[usize], usize)>,
    lines: &mut Vec<Line<'static>>,
) {
    let th = &state.theme;
    let cursor = single.or(multi.map(|(_, cursor)| cursor)).unwrap_or(0);
    let len = options.len();
    let start = cursor
        .saturating_sub(FORM_OPTION_WINDOW / 2)
        .min(len.saturating_sub(FORM_OPTION_WINDOW));
    let end = (start + FORM_OPTION_WINDOW).min(len);

    if start > 0 {
        let mut args = fluent_bundle::FluentArgs::new();
        args.set("count", start as i64);
        lines.push(Line::from(Span::styled(
            state.i18n.format("form-more-above", Some(&args)),
            th.style(Sem::Muted),
        )));
    }
    for (idx, option) in options.iter().enumerate().take(end).skip(start) {
        let is_cursor = idx == cursor;
        let mut spans = vec![
            Span::raw("      "),
            Span::styled(
                selection_marker(state, is_cursor),
                th.style(if is_cursor { Sem::Accent } else { Sem::Muted }),
            ),
        ];
        if let Some((selected, _)) = multi {
            let checked = selected.contains(&idx);
            spans.push(Span::styled(
                if checked { "[x] " } else { "[ ] " },
                th.style(if checked { Sem::AccentSoft } else { Sem::Muted }),
            ));
        }
        spans.push(Span::styled(
            option.to_string(),
            th.style(if is_cursor { Sem::Accent } else { Sem::Muted })
                .add_modifier(if is_cursor {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        ));
        lines.push(Line::from(spans));
    }
    if end < len {
        let mut args = fluent_bundle::FluentArgs::new();
        args.set("count", (len - end) as i64);
        lines.push(Line::from(Span::styled(
            state.i18n.format("form-more-below", Some(&args)),
            th.style(Sem::Muted),
        )));
    }
}

/// Clip `value` to `avail` display columns keeping the caret (char index) visible.
/// Returns the visible slice and the caret's column offset within it.
fn text_window(value: &str, cursor: usize, avail: usize) -> (String, u16) {
    let chars: Vec<char> = value.chars().collect();
    let cursor = cursor.min(chars.len());
    let width = |slice: &[char]| -> usize {
        slice
            .iter()
            .map(|c| UnicodeWidthChar::width(*c).unwrap_or(1))
            .sum()
    };
    // Slide the window start right until the caret fits (leave 1 col for the caret).
    let mut start = 0usize;
    while width(&chars[start..cursor]) > avail.saturating_sub(1) {
        start += 1;
    }
    // Extend the visible slice while it fits.
    let mut end = start;
    let mut used = 0usize;
    while end < chars.len() {
        let w = UnicodeWidthChar::width(chars[end]).unwrap_or(1);
        if used + w > avail {
            break;
        }
        used += w;
        end += 1;
    }
    let visible: String = chars[start..end].iter().collect();
    let caret_cols = width(&chars[start..cursor]) as u16;
    (visible, caret_cols)
}

fn form_field(state: &AppState, label: &str, value: &str) -> Line<'static> {
    let th = &state.theme;
    Line::from(vec![
        Span::styled(format!("{label:<7}"), th.style(Sem::Muted)),
        Span::styled(
            value.to_string(),
            Style::default().fg(th.color(Sem::AccentSoft)),
        ),
    ])
}

fn form_options_owned(
    state: &AppState,
    options: &[String],
    selected: usize,
    indent: usize,
) -> Vec<Line<'static>> {
    let th = &state.theme;
    options
        .iter()
        .enumerate()
        .map(|(idx, option)| {
            let is_selected = idx == selected;
            Line::from(vec![
                Span::raw(" ".repeat(indent)),
                Span::styled(
                    selection_marker(state, is_selected),
                    th.style(if is_selected { Sem::Accent } else { Sem::Muted }),
                ),
                Span::styled(
                    option.to_string(),
                    th.style(if is_selected { Sem::Accent } else { Sem::Muted })
                        .add_modifier(if is_selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
            ])
        })
        .collect()
}

fn form_hint(state: &AppState, text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        state.theme.style(Sem::Muted),
    ))
}

/// A "N more fields above/below" marker for the windowed form field list.
fn form_scroll_marker(state: &AppState, key: &str, count: usize) -> Line<'static> {
    let mut args = fluent_bundle::FluentArgs::new();
    args.set("count", count as i64);
    Line::from(Span::styled(
        state.i18n.format(key, Some(&args)),
        state.theme.style(Sem::Muted),
    ))
}

fn form_hint_with_enter(state: &AppState, key: &str, enter: &str) -> String {
    let mut args = fluent_bundle::FluentArgs::new();
    args.set("enter", enter);
    state.i18n.format(key, Some(&args))
}

fn render_fullscreen_overlay(state: &AppState, f: &mut Frame, area: Rect) {
    let Some(overlay) = state.fullscreen_overlay() else {
        return;
    };
    if let FullscreenOverlay::Help(help) = overlay {
        render_help_overlay(state, help, f, area);
        return;
    }
    if let FullscreenOverlay::Stats(stats) = overlay {
        render_stats_overlay(state, stats, f, area);
        return;
    }
    if let FullscreenOverlay::Sessions(sessions) = overlay {
        render_sessions_overlay(state, sessions, f, area);
        return;
    }
    if let FullscreenOverlay::Workspaces(workspaces) = overlay {
        render_workspaces_overlay(state, workspaces, f, area);
        return;
    }
    if let FullscreenOverlay::Models(models) = overlay {
        render_models_overlay(state, models, f, area);
        return;
    }
    if let FullscreenOverlay::Zoom(zoom) = overlay {
        render_zoom_overlay(state, zoom, f, area);
        return;
    }
    if let FullscreenOverlay::Replay(replay) = overlay {
        render_replay_overlay(state, replay, f, area);
        return;
    }
    unreachable!("every fullscreen overlay has a renderer");
}

fn render_replay_overlay(
    state: &AppState,
    replay: &crate::app::ReplayOverlay,
    f: &mut Frame,
    area: Rect,
) {
    let th = &state.theme;
    let detail = replay.detail.as_ref();
    let title = match detail {
        Some(detail) => {
            let mut args = fluent_bundle::FluentArgs::new();
            args.set("turn", detail.turn_seq as i64);
            args.set("rounds", detail.rounds.len() as i64);
            state.i18n.format("replay-title-detail", Some(&args))
        }
        None => {
            let mut args = fluent_bundle::FluentArgs::new();
            args.set("count", replay.total as i64);
            state.i18n.format("replay-title", Some(&args))
        }
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(overlay_border_type(state))
        .border_style(Style::default().fg(th.color(Sem::AccentSoft)))
        .padding(Padding::horizontal(1))
        .title(Span::styled(
            format!(" {title} "),
            th.style(Sem::Muted).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(Clear, area);
    f.render_widget(block, area);
    if inner.height < 2 || inner.width < 6 {
        return;
    }
    let v = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
    let body = v[0];
    let rows = body.height as usize;

    if detail.is_some_and(|d| d.failed) {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                state.i18n.t("replay-failed"),
                th.style(Sem::Warning),
            ))),
            body,
        );
    } else {
        let wrap_w = (body.width as usize).clamp(1, 120);
        let (logical, head_at, selected): (&[HistLine], &[usize], Option<usize>) = match detail {
            Some(detail) => (&detail.lines, &detail.head_at, Some(detail.selected_round)),
            None => (&[], &[], None),
        };
        let list_lines: Vec<HistLine> = if detail.is_none() {
            replay
                .turns
                .iter()
                .map(|item| crate::render::replay::turn_list_row(state, item, wrap_w))
                .collect()
        } else {
            Vec::new()
        };
        let lines_src: &[HistLine] = if detail.is_some() {
            logical
        } else {
            &list_lines
        };
        let scroll = detail.map(|d| d.scroll).unwrap_or(replay.scroll);
        let selected_head = if detail.is_some() {
            selected.and_then(|idx| head_at.get(idx).copied())
        } else {
            Some(replay.selected)
        };

        let mut wrapped: Vec<Line<'static>> = Vec::new();
        let mut wrapped_head: Vec<usize> = Vec::with_capacity(lines_src.len());
        for line in lines_src {
            wrapped_head.push(wrapped.len());
            wrapped.extend(wrap_hist_line(line, wrap_w, wrap_w));
        }
        let total = wrapped.len();
        let start = scroll.min(total.saturating_sub(rows));
        let end = (start + rows).min(total);
        let sel_range = selected_head.and_then(|idx| {
            let from = *wrapped_head.get(idx)?;
            let to = wrapped_head.get(idx + 1).copied().unwrap_or(total);
            Some((from, to))
        });
        let mut out: Vec<Line<'static>> = Vec::with_capacity(end - start);
        for i in start..end {
            let is_sel = sel_range.is_some_and(|(from, to)| i >= from && i < to);
            if is_sel {
                out.push(highlight_line(&wrapped[i]));
            } else {
                out.push(wrapped[i].clone());
            }
        }
        f.render_widget(Paragraph::new(out), body);
    }

    let hint = if detail.is_some() {
        "replay-hint-detail"
    } else {
        "replay-hint-list"
    };
    let footer = Line::from(vec![Span::styled(state.i18n.t(hint), th.style(Sem::Muted))]);
    f.render_widget(Paragraph::new(footer), v[1]);
}

fn highlight_line(line: &Line<'static>) -> Line<'static> {
    if line.spans.is_empty() {
        return Line::from(Span::styled(
            " ".to_string(),
            Style::default().add_modifier(Modifier::REVERSED),
        ));
    }
    Line::from(
        line.spans
            .iter()
            .map(|span| {
                Span::styled(
                    span.content.clone().into_owned(),
                    span.style.add_modifier(Modifier::REVERSED),
                )
            })
            .collect::<Vec<_>>(),
    )
}

/// Fullscreen text viewer (zoom): bordered card with the title, a pre-wrapped line
/// window at `zoom.scroll` (defensively re-clamped against the ACTUAL area, which
/// equals the `term_rows`-derived page size the key handler clamps with), and a
/// muted "N% · keys" footer.
fn render_zoom_overlay(state: &AppState, zoom: &ZoomOverlay, f: &mut Frame, area: Rect) {
    let th = &state.theme;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(overlay_border_type(state))
        .border_style(Style::default().fg(th.color(Sem::AccentSoft)))
        .padding(Padding::horizontal(1))
        .title(Span::styled(
            format!(" {} ", zoom.title),
            th.style(Sem::Muted).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(Clear, area);
    f.render_widget(block, area);
    if inner.height < 2 || inner.width < 4 {
        return;
    }
    let v = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);

    let content_rows = v[0].height as usize;
    let total = zoom.lines.len();
    let start = zoom.scroll.min(total.saturating_sub(content_rows));
    let end = (start + content_rows).min(total);
    f.render_widget(Paragraph::new(zoom.lines[start..end].to_vec()), v[0]);

    // Position = how much of the document the view bottom has passed.
    let percent = (end * 100).checked_div(total).unwrap_or(100);
    let mut args = fluent_bundle::FluentArgs::new();
    args.set("percent", percent as i64);
    let footer_key = match (zoom.kind, zoom.copy_text.is_some()) {
        (ZoomKind::Document, true) => "zoom-footer-copy",
        (ZoomKind::Document, false) => "zoom-footer",
        (ZoomKind::SessionInfo, _) => "zoom-footer-info",
    };
    let footer = zoom
        .copy_feedback
        .clone()
        .unwrap_or_else(|| state.i18n.format(footer_key, Some(&args)));
    f.render_widget(Paragraph::new(fullscreen_hint(state, &footer)), v[1]);
}

fn model_field_line(state: &AppState, label_key: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{:<10}", state.i18n.t(label_key)),
            state.theme.style(Sem::Muted),
        ),
        Span::styled(
            value.to_string(),
            Style::default().fg(state.theme.color(Sem::AccentSoft)),
        ),
    ])
}

/// Render a "key label · key label" hint string with the shortcut keys emphasized
/// (accent bold) and the descriptions muted — so shortcuts read differently from prose.
/// Parses the existing i18n hint strings (segments split on " · ", key = first token).
fn styled_hint(state: &AppState, text: &str) -> Line<'static> {
    let th = &state.theme;
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (i, seg) in text.split(" \u{b7} ").enumerate() {
        if i > 0 {
            spans.push(Span::styled(" \u{b7} ", th.style(Sem::Border)));
        }
        match seg.trim().split_once(' ') {
            Some((key, label)) => {
                spans.push(Span::styled(
                    key.to_string(),
                    th.style(Sem::Accent).add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::raw(" "));
                spans.push(Span::styled(label.to_string(), th.style(Sem::Muted)));
            }
            None => spans.push(Span::styled(seg.trim().to_string(), th.style(Sem::Muted))),
        }
    }
    Line::from(spans)
}

fn fullscreen_hint(state: &AppState, text: &str) -> Line<'static> {
    let selection_key = match state.terminal_kind {
        TerminalKind::AppleTerminal => "overlay-mouse-select-apple",
        TerminalKind::ITerm2 => "overlay-mouse-select-iterm",
        TerminalKind::Vscode if cfg!(target_os = "macos") => "overlay-mouse-select-vscode-mac",
        TerminalKind::Vscode => "overlay-mouse-select-vscode-other",
        TerminalKind::Windows => "overlay-mouse-select-windows",
        TerminalKind::XtermLike => "overlay-mouse-select-xterm",
    };
    styled_hint(state, &format!("{} · {text}", state.i18n.t(selection_key)))
}

/// One row in the help command list: a group label or a command (index into
/// `HelpOverlay.commands`). Grouping replaces the old per-row `cmd/skill` tag column.
enum HelpRow {
    Header(&'static str),
    Cmd(usize),
}

/// Help is a centered card sized to its content (not a full-screen box): a pinned
/// keyboard-essentials block on top, then the commands grouped into built-ins and
/// skills, scrollable around the selection.
fn render_help_overlay(state: &AppState, help: &HelpOverlay, f: &mut Frame, area: Rect) {
    let th = &state.theme;

    // Keyboard essentials. Single column → alignment needs no CJK width math.
    const KEYS: [(&str, &str); 6] = [
        ("Enter", "help-key-send"),
        ("Shift+Enter", "help-key-newline"),
        ("@", "help-key-file"),
        ("/", "help-key-command"),
        ("Esc ×2", "help-key-cancel"),
        ("Ctrl+C ×2", "help-key-quit"),
    ];
    let key_w = KEYS
        .iter()
        .map(|(key, _)| UnicodeWidthStr::width(*key))
        .max()
        .unwrap_or(0);

    // Grouped command rows: built-ins first, then skills, each under a label.
    let mut rows: Vec<HelpRow> = Vec::new();
    for (group_key, kind) in [
        ("help-group-commands", CommandKind::Builtin),
        ("help-group-skills", CommandKind::Skill),
    ] {
        let mut opened = false;
        for (idx, cmd) in help.commands.iter().enumerate() {
            if cmd.kind != kind {
                continue;
            }
            if !opened {
                rows.push(HelpRow::Header(group_key));
                opened = true;
            }
            rows.push(HelpRow::Cmd(idx));
        }
    }

    // Card sized to content, capped to the screen, centered.
    const TOP_FIXED: u16 = 1 + 1 + KEYS.len() as u16 + 1; // subtitle + kbd title + keys + divider
    let ideal_h = 2 + TOP_FIXED + rows.len() as u16 + 1; // border + top + commands + footer
    let card = centered_rect(area, 80, ideal_h, 2, 1);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(overlay_border_type(state))
        .border_style(Style::default().fg(th.color(Sem::AccentSoft)))
        .padding(Padding::horizontal(1))
        .title(Span::styled(
            format!(" {} ", state.i18n.t("help-title")),
            th.style(Sem::Muted).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(card);
    f.render_widget(Clear, card);
    f.render_widget(block, card);
    if inner.width < 8 || inner.height < 4 {
        return;
    }

    let v = Layout::vertical([
        Constraint::Length(1),                     // subtitle
        Constraint::Length(1 + KEYS.len() as u16), // keyboard block (title + keys)
        Constraint::Length(1),                     // divider
        Constraint::Min(1),                        // commands (scroll region)
        Constraint::Length(1),                     // footer hint
    ])
    .split(inner);

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            state.i18n.t("help-subtitle"),
            th.style(Sem::Muted),
        ))),
        v[0],
    );

    let mut kbd = vec![Line::from(Span::styled(
        state.i18n.t("help-shortcuts"),
        Style::default()
            .fg(th.color(Sem::Accent))
            .add_modifier(Modifier::BOLD),
    ))];
    for (key, desc) in KEYS {
        kbd.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("{key:key_w$}"),
                Style::default()
                    .fg(th.color(Sem::AccentSoft))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(state.i18n.t(desc), th.style(Sem::Muted)),
        ]));
    }
    f.render_widget(Paragraph::new(kbd), v[1]);

    render_help_commands(state, help, &rows, f, v[3]);

    f.render_widget(
        Paragraph::new(fullscreen_hint(state, &state.i18n.t("help-hint"))),
        v[4],
    );
}

fn render_help_commands(
    state: &AppState,
    help: &HelpOverlay,
    rows: &[HelpRow],
    f: &mut Frame,
    area: Rect,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let th = &state.theme;
    if help.commands.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                state.i18n.t("help-no-commands"),
                th.style(Sem::Muted),
            ))),
            area,
        );
        return;
    }

    let selected = help.selected.min(help.commands.len().saturating_sub(1));
    let viewport = area.height as usize;
    // Window the grouped rows around the selected command's display row.
    let sel_row = rows
        .iter()
        .position(|row| matches!(row, HelpRow::Cmd(idx) if *idx == selected))
        .unwrap_or(0);
    let start = if rows.len() <= viewport {
        0
    } else {
        sel_row
            .saturating_sub(viewport / 2)
            .min(rows.len() - viewport)
    };
    let end = (start + viewport).min(rows.len());

    let name_w = help
        .commands
        .iter()
        .map(|cmd| UnicodeWidthStr::width(cmd.name.as_str()) + 1)
        .max()
        .unwrap_or(1)
        .clamp(6, 16);
    let desc_w = (area.width as usize).saturating_sub(2 + name_w + 2).max(6);

    let lines: Vec<Line> = rows[start..end]
        .iter()
        .map(|row| match row {
            HelpRow::Header(key) => Line::from(Span::styled(
                state.i18n.t(key),
                th.style(Sem::Muted).add_modifier(Modifier::BOLD),
            )),
            HelpRow::Cmd(idx) => {
                let cmd = &help.commands[*idx];
                let active = *idx == selected;
                let name = truncate_display(&format!("/{}", cmd.name), name_w);
                let name_sem = if active { Sem::Accent } else { Sem::AccentSoft };
                Line::from(vec![
                    Span::styled(
                        selection_marker(state, active),
                        Style::default().fg(th.color(Sem::Accent)),
                    ),
                    Span::styled(
                        format!("{name:name_w$}"),
                        th.style(name_sem).add_modifier(if active {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                    ),
                    Span::raw("  "),
                    Span::styled(
                        truncate_display(&cmd.description, desc_w),
                        th.style(Sem::Muted),
                    ),
                ])
            }
        })
        .collect();
    f.render_widget(Paragraph::new(lines), area);
}

fn render_sessions_overlay(
    state: &AppState,
    sessions: &crate::app::SessionsOverlay,
    f: &mut Frame,
    area: Rect,
) {
    let th = &state.theme;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(overlay_border_type(state))
        .border_style(Style::default().fg(th.color(Sem::AccentSoft)))
        .title(Span::styled(
            format!(" {} ", state.i18n.t("sessions-title")),
            th.style(Sem::Muted).add_modifier(Modifier::BOLD),
        ));
    let inner = inset_rect(block.inner(area), 2, 1);
    f.render_widget(block, area);

    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(inner);
    let panes = Layout::horizontal([
        Constraint::Percentage(40),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Percentage(60),
    ])
    .split(chunks[1]);

    // The inline rename input reuses the search input's slot/style in the header —
    // exactly one of the two is active at a time (rename is only reachable from the
    // list pane, which exits search mode first).
    let search = if let Some(draft) = &sessions.renaming {
        let mut args = fluent_bundle::FluentArgs::new();
        args.set("title", draft.as_str());
        state.i18n.format("sessions-rename-active", Some(&args))
    } else if sessions.searching {
        let mut args = fluent_bundle::FluentArgs::new();
        args.set("query", sessions.query.as_str());
        state.i18n.format("sessions-search-active", Some(&args))
    } else if sessions.query.is_empty() {
        state.i18n.t("sessions-search-placeholder")
    } else {
        let mut args = fluent_bundle::FluentArgs::new();
        args.set("query", sessions.query.as_str());
        state.i18n.format("sessions-search-query", Some(&args))
    };
    let header = Line::from(vec![
        Span::styled(
            state.i18n.count("sessions-count", sessions.items.len()),
            Style::default()
                .fg(th.color(Sem::Accent))
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("   "),
        Span::styled(search, th.style(Sem::Muted)),
    ]);
    f.render_widget(
        Paragraph::new(vec![header, Line::from(String::new())]),
        chunks[0],
    );

    render_session_list(state, sessions, f, panes[0]);
    render_pane_divider(state, f, panes[2]);
    render_session_history(state, sessions, f, panes[4]);

    let hint = if sessions.renaming.is_some() {
        state.i18n.t("sessions-hint-rename")
    } else if sessions.searching {
        state.i18n.t("sessions-hint-search")
    } else if sessions.selection_mode {
        state.i18n.t("sessions-hint-selection")
    } else if sessions.detail_mode == SessionDetailMode::HistoryPicker {
        state.i18n.t("sessions-hint-history")
    } else {
        state.i18n.t("sessions-hint-list")
    };
    f.render_widget(Paragraph::new(fullscreen_hint(state, &hint)), chunks[2]);

    if let Some(confirm) = &sessions.confirm {
        render_confirm_dialog(state, confirm, f, area);
    }
}

fn render_workspaces_overlay(
    state: &AppState,
    workspaces: &crate::app::WorkspacesOverlay,
    f: &mut Frame,
    area: Rect,
) {
    let th = &state.theme;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(overlay_border_type(state))
        .border_style(Style::default().fg(th.color(Sem::AccentSoft)))
        .title(Span::styled(
            format!(" {} ", state.i18n.t("workspace-title")),
            th.style(Sem::Muted).add_modifier(Modifier::BOLD),
        ));
    let inner = inset_rect(block.inner(area), 2, 1);
    f.render_widget(block, area);

    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(inner);
    let panes = Layout::horizontal([
        Constraint::Percentage(40),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Percentage(60),
    ])
    .split(chunks[1]);

    let mut header = vec![Span::styled(
        state.i18n.count("workspace-count", workspaces.items.len()),
        Style::default()
            .fg(th.color(Sem::Accent))
            .add_modifier(Modifier::BOLD),
    )];
    if let Some(current) = workspaces
        .items
        .iter()
        .find(|w| w.workspace_id.to_string() == workspaces.current)
    {
        header.push(Span::raw("   "));
        header.push(Span::styled(
            format!("{} · {}", current.name, state.i18n.t("workspace-current")),
            th.style(Sem::Muted),
        ));
    }
    f.render_widget(
        Paragraph::new(vec![Line::from(header), Line::from(String::new())]),
        chunks[0],
    );

    render_workspace_list(state, workspaces, f, panes[0]);
    render_pane_divider(state, f, panes[2]);
    render_workspace_sessions_pane(state, workspaces, f, panes[4]);

    let hint = if workspaces.focus == crate::app::WorkspacePane::Right {
        state.i18n.t("workspace-hint-right")
    } else {
        state.i18n.t("workspace-hint-left")
    };
    f.render_widget(Paragraph::new(fullscreen_hint(state, &hint)), chunks[2]);

    if let Some(confirm) = &workspaces.confirm {
        render_confirm_dialog(state, confirm, f, area);
    }
}

fn render_workspace_list(
    state: &AppState,
    workspaces: &crate::app::WorkspacesOverlay,
    f: &mut Frame,
    area: Rect,
) {
    let th = &state.theme;
    let active_left = workspaces.focus == crate::app::WorkspacePane::Left;
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (idx, ws) in workspaces.items.iter().enumerate() {
        let selected = idx == workspaces.selected;
        let is_current = ws.workspace_id.to_string() == workspaces.current;
        let marker = if selected {
            if active_left {
                Glyph::Prompt.render(state.icons)
            } else {
                Glyph::SelectionInactive.render(state.icons)
            }
        } else {
            " "
        };
        let name_display = if is_current {
            format!("{} {}", ws.name, state.i18n.t("workspace-current"))
        } else {
            ws.name.clone()
        };
        let count = state
            .i18n
            .count("sessions-count", ws.session_count as usize);
        let row_w = area.width as usize;
        let count_w = UnicodeWidthStr::width(count.as_str());
        let name_avail = row_w.saturating_sub(3).saturating_sub(count_w).max(8);
        let name = truncate_display(&name_display, name_avail);
        let name_w = UnicodeWidthStr::width(name.as_str());
        let spacer_w = row_w
            .saturating_sub(2)
            .saturating_sub(name_w)
            .saturating_sub(count_w);
        lines.push(Line::from(vec![
            Span::styled(
                format!("{marker} "),
                th.style(if selected && active_left {
                    Sem::Accent
                } else {
                    Sem::Muted
                }),
            ),
            Span::styled(
                name,
                th.style(if selected && active_left {
                    Sem::Accent
                } else {
                    Sem::AccentSoft
                })
                .add_modifier(if selected && active_left {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
            ),
            Span::raw(" ".repeat(spacer_w)),
            Span::styled(count, th.style(Sem::Muted)),
        ]));
        let root = truncate_display(&ws.root, row_w.saturating_sub(4).max(8));
        lines.push(Line::from(vec![
            Span::raw("   "),
            Span::styled(
                root,
                th.style(if ws.exists { Sem::Muted } else { Sem::Warning }),
            ),
        ]));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            state.i18n.t("workspace-empty"),
            th.style(Sem::Muted),
        )));
    }
    let selected_row = workspaces.selected.saturating_mul(2);
    let (start, end) = visible_window(lines.len(), selected_row, area.height as usize);
    f.render_widget(
        Paragraph::new(lines[start..end].to_vec()).wrap(Wrap { trim: false }),
        area,
    );
}

fn render_workspace_sessions_pane(
    state: &AppState,
    workspaces: &crate::app::WorkspacesOverlay,
    f: &mut Frame,
    area: Rect,
) {
    let th = &state.theme;
    let active_right = workspaces.focus == crate::app::WorkspacePane::Right;
    let row_w = (area.width as usize).saturating_sub(2).max(8);
    let mut lines = vec![
        Line::from(Span::styled(
            format!(
                "{}  {}",
                state.i18n.t("workspace-sessions-title"),
                state
                    .i18n
                    .count("sessions-count", workspaces.sessions.len())
            ),
            th.style(Sem::Muted).add_modifier(Modifier::BOLD),
        )),
        Line::from(String::new()),
    ];

    let new_selected = workspaces.session_selected == 0;
    lines.push(new_session_line(state, new_selected && active_right, row_w));

    for (idx, session) in workspaces.sessions.iter().enumerate() {
        let selected = workspaces.session_selected == idx + 1;
        let meta = session.when.clone();
        let meta_max = row_w / 2;
        let meta = truncate_display(&meta, meta_max);
        let meta_w = UnicodeWidthStr::width(meta.as_str());
        let title_max = row_w.saturating_sub(1).saturating_sub(meta_w).max(4);
        let title = truncate_display(&session.title, title_max);
        let title_w = UnicodeWidthStr::width(title.as_str());
        let spacer_w = row_w.saturating_sub(title_w).saturating_sub(meta_w);
        let marker = if selected {
            if active_right {
                Glyph::Prompt.render(state.icons)
            } else {
                Glyph::SelectionInactive.render(state.icons)
            }
        } else {
            " "
        };
        let title_style = th
            .style(if selected && active_right {
                Sem::Accent
            } else {
                Sem::Muted
            })
            .add_modifier(if selected && active_right {
                Modifier::BOLD
            } else {
                Modifier::empty()
            });
        lines.push(Line::from(vec![
            Span::styled(
                format!("{marker} "),
                th.style(if selected && active_right {
                    Sem::Accent
                } else {
                    Sem::Muted
                }),
            ),
            Span::styled(title, title_style),
            Span::raw(" ".repeat(spacer_w)),
            Span::styled(meta, th.style(Sem::Muted)),
        ]));
    }
    let (start, end) = visible_window(
        lines.len(),
        2 + workspaces.session_selected,
        area.height as usize,
    );
    f.render_widget(Paragraph::new(lines[start..end].to_vec()), area);
}

fn new_session_line(state: &AppState, selected: bool, row_w: usize) -> Line<'static> {
    let th = &state.theme;
    let active = selected;
    let marker = if selected {
        Glyph::Prompt.render(state.icons)
    } else {
        " "
    };
    let label = format!("＋ {}", state.i18n.t("workspace-new-session"));
    let label = truncate_display(&label, row_w.saturating_sub(2).max(4));
    Line::from(vec![
        Span::styled(
            format!("{marker} "),
            th.style(if active { Sem::Accent } else { Sem::Muted }),
        ),
        Span::styled(
            label,
            th.style(if active { Sem::Accent } else { Sem::Info })
                .add_modifier(if active {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        ),
    ])
}

fn render_session_list(
    state: &AppState,
    sessions: &crate::app::SessionsOverlay,
    f: &mut Frame,
    area: Rect,
) {
    let th = &state.theme;
    let mut rows: Vec<(Option<usize>, Line<'static>)> = Vec::new();
    let mut last_group = "";
    for (idx, session) in sessions.items.iter().enumerate() {
        if session.group != last_group {
            if !rows.is_empty() {
                rows.push((None, Line::from(String::new())));
            }
            rows.push((
                None,
                Line::from(vec![Span::styled(
                    session.group.clone(),
                    th.style(Sem::AccentSoft),
                )]),
            ));
            last_group = &session.group;
        }
        let selected = idx == sessions.selected;
        let marked = sessions.selected_session_ids.contains(&session.id);
        let active_pane = sessions.detail_mode == SessionDetailMode::Preview;
        let marker = if selected {
            if active_pane {
                Glyph::Prompt.render(state.icons)
            } else {
                Glyph::SelectionInactive.render(state.icons)
            }
        } else {
            " "
        };
        let meta = session.when.clone();
        let selection_prefix = if sessions.selection_mode {
            4usize
        } else {
            0usize
        };
        let prefix_w = 2usize + selection_prefix;
        let meta_w = UnicodeWidthStr::width(meta.as_str());
        let row_w = area.width as usize;
        let title_w = row_w
            .saturating_sub(prefix_w)
            .saturating_sub(meta_w)
            .saturating_sub(2)
            .max(8);
        let title = truncate_display(&session.title, title_w);
        let title_display_w = UnicodeWidthStr::width(title.as_str());
        let spacer_w = row_w
            .saturating_sub(prefix_w)
            .saturating_sub(title_display_w)
            .saturating_sub(meta_w);
        let title_style = th
            .style(if selected && active_pane {
                Sem::Accent
            } else if selected {
                Sem::AccentSoft
            } else {
                Sem::Muted
            })
            .add_modifier(if selected && active_pane {
                Modifier::BOLD
            } else {
                Modifier::empty()
            });
        let mut spans = vec![Span::styled(
            format!("{marker} "),
            th.style(if active_pane { Sem::Accent } else { Sem::Muted }),
        )];
        if sessions.selection_mode {
            spans.push(Span::styled(
                if marked { "[x] " } else { "[ ] " },
                th.style(if marked { Sem::Warning } else { Sem::Muted }),
            ));
        }
        spans.extend([
            Span::styled(title, title_style),
            Span::raw(" ".repeat(spacer_w)),
            Span::styled(meta, th.style(Sem::Muted)),
        ]);
        rows.push((Some(idx), Line::from(spans)));
    }
    if rows.is_empty() {
        rows.push((
            None,
            Line::from(Span::styled(
                state.i18n.t("sessions-empty"),
                th.style(Sem::Muted),
            )),
        ));
    }
    let selected_row = rows
        .iter()
        .position(|(session_idx, _)| *session_idx == Some(sessions.selected))
        .unwrap_or(0);
    let (start, end) = visible_window(rows.len(), selected_row, area.height as usize);
    let lines = rows[start..end]
        .iter()
        .map(|(_, line)| line.clone())
        .collect::<Vec<_>>();
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn render_pane_divider(state: &AppState, f: &mut Frame, area: Rect) {
    let line = Span::styled(
        Glyph::RuleVertical.render(state.icons),
        state.theme.style(Sem::Border),
    );
    let lines = (0..area.height)
        .map(|_| Line::from(line.clone()))
        .collect::<Vec<_>>();
    f.render_widget(Paragraph::new(lines), area);
}

fn render_session_history(
    state: &AppState,
    sessions: &crate::app::SessionsOverlay,
    f: &mut Frame,
    area: Rect,
) {
    let th = &state.theme;
    let mut lines = Vec::new();
    if let Some(selected) = sessions.items.get(sessions.selected) {
        lines.push(Line::from(Span::styled(
            selected.title.clone(),
            Style::default()
                .fg(th.color(Sem::Accent))
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(Span::styled(
            format!(
                "{} · {} · ${:.2}",
                selected.when, selected.model, selected.cost
            ),
            th.style(Sem::Muted),
        )));
        lines.extend(session_stats_lines(state, selected));
        lines.push(Line::from(String::new()));
    }
    let history_active = sessions.detail_mode == SessionDetailMode::HistoryPicker;
    let history_title = if history_active {
        state.i18n.t("sessions-history-picker-title")
    } else {
        state.i18n.t("sessions-history-title")
    };
    lines.push(Line::from(vec![
        Span::styled(
            if history_active {
                selection_marker(state, true)
            } else {
                String::new()
            },
            th.style(if history_active {
                Sem::Accent
            } else {
                Sem::Muted
            }),
        ),
        Span::styled(
            history_title,
            Style::default()
                .fg(th.color(if history_active {
                    Sem::Accent
                } else {
                    Sem::AccentSoft
                }))
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    if history_active {
        lines.push(Line::from(Span::styled(
            format!(
                "{} · {}",
                state.i18n.t("sessions-history-picker-hint"),
                state.i18n.t("sessions-history-zoom-hint")
            ),
            th.style(Sem::Muted),
        )));
    }
    let mut selected_line = None;
    let indent = "  ";
    let mut last_turn: Option<usize> = None;
    for (idx, line) in sessions.history.iter().enumerate() {
        if matches!(line, crate::session::dto::HistoryItem::Event { .. }) {
            continue;
        }
        let turn_index = line.turn_index();
        if last_turn != Some(turn_index) {
            let mut turn_args = fluent_bundle::FluentArgs::new();
            turn_args.set("n", turn_index as i64);
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    state.i18n.format("sessions-history-turn", Some(&turn_args)),
                    th.style(Sem::AccentSoft).add_modifier(Modifier::BOLD),
                ),
            ]));
            last_turn = Some(turn_index);
        }
        let (icon, label, role_color, text_color) = match line {
            crate::session::dto::HistoryItem::Message {
                role: crate::session::dto::MessageRole::User,
                ..
            } => (
                Glyph::Session.render(state.icons),
                state.i18n.t("sessions-history-user"),
                Sem::Muted,
                Sem::Muted,
            ),
            crate::session::dto::HistoryItem::Message {
                role: crate::session::dto::MessageRole::Assistant,
                ..
            } => (
                Glyph::Subagent.render(state.icons),
                state.i18n.t("sessions-history-assistant"),
                Sem::AccentSoft,
                Sem::AccentSoft,
            ),
            crate::session::dto::HistoryItem::Event { .. } => (
                Glyph::Tool.render(state.icons),
                "event".into(),
                Sem::Muted,
                Sem::Muted,
            ),
        };
        let selected = history_active && idx == sessions.history_selected;
        let role = format!("{icon} {label}");
        let role = pad_display_width(&role, 7);
        let prefix_width = 2 + UnicodeWidthStr::width(indent) + 7 + 1;
        let text_width = (area.width as usize).saturating_sub(prefix_width);
        if selected {
            selected_line = Some(lines.len());
        }
        lines.push(Line::from(vec![
            Span::styled(
                selection_marker(state, selected),
                th.style(if selected { Sem::Accent } else { Sem::Muted }),
            ),
            Span::styled(indent, th.style(Sem::Muted)),
            Span::styled(
                role,
                th.style(if selected { Sem::Accent } else { role_color })
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                truncate_display_ascii_ellipsis(&line.preview(), text_width),
                th.style(if selected { Sem::Accent } else { text_color })
                    .add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
        ]));
    }
    let (start, end) = if let Some(selected_line) = selected_line {
        visible_window(lines.len(), selected_line, area.height as usize)
    } else {
        (0, lines.len().min(area.height as usize))
    };
    let lines = lines[start..end].to_vec();
    f.render_widget(Paragraph::new(lines), area);
}

fn visible_window(total: usize, selected: usize, height: usize) -> (usize, usize) {
    if height == 0 || total == 0 {
        return (0, 0);
    }
    if total <= height {
        return (0, total);
    }
    let half = height / 2;
    let start = selected
        .saturating_sub(half)
        .min(total.saturating_sub(height));
    (start, start + height)
}

fn viewport_scroll_start(scroll: usize, total: usize, viewport: usize) -> usize {
    scroll.min(total.saturating_sub(viewport))
}

fn session_stats_lines(
    state: &AppState,
    session: &crate::session::dto::SessionSummary,
) -> Vec<Line<'static>> {
    let th = &state.theme;
    let ctx_pct = if session.context_limit_tokens == 0 {
        0.0
    } else {
        session.context_used_tokens as f64 / session.context_limit_tokens as f64 * 100.0
    };
    let total = session.input_tokens + session.output_tokens;
    let cache_total = session.cache_read_tokens + session.cache_write_tokens;
    let cache_pct = if total == 0 {
        0.0
    } else {
        cache_total as f64 / total as f64 * 100.0
    };
    let mut lines = Vec::new();
    if session.context_limit_tokens > 0 {
        lines.push(Line::from(vec![
            Span::styled("ctx ", th.style(Sem::Muted)),
            Span::styled(
                format!(
                    "{} / {} ({ctx_pct:.0}%)",
                    compact_u64(session.context_used_tokens),
                    compact_u64(session.context_limit_tokens)
                ),
                Style::default().fg(if ctx_pct >= 85.0 {
                    th.color(Sem::Warning)
                } else {
                    th.color(Sem::AccentSoft)
                }),
            ),
        ]));
    }
    lines.push(Line::from(vec![
        Span::styled("tok ", th.style(Sem::Muted)),
        Span::styled(
            format!(
                "in {} · out {}",
                compact_u64(session.input_tokens),
                compact_u64(session.output_tokens)
            ),
            th.style(Sem::Muted),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled("cache ", th.style(Sem::Muted)),
        Span::styled(
            format!(
                "read {} · write {} · {:.0}%",
                compact_u64(session.cache_read_tokens),
                compact_u64(session.cache_write_tokens),
                cache_pct
            ),
            Style::default().fg(th.color(Sem::Info)),
        ),
    ]));
    lines
}

fn render_confirm_dialog(
    state: &AppState,
    confirm: &crate::app::ConfirmDialog,
    f: &mut Frame,
    area: Rect,
) {
    let th = &state.theme;
    let rect = centered_rect(area, 76, 7, 0, 0);
    let color = if confirm.danger {
        th.color(Sem::Error)
    } else {
        th.color(Sem::AccentSoft)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(overlay_border_type(state))
        .border_style(Style::default().fg(color))
        .padding(Padding::horizontal(2))
        .title(Span::styled(
            format!(" {} ", confirm.title),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
    let options = [confirm.cancel_label.clone(), confirm.confirm_label.clone()];
    // Word-aware wrap: `wrap_line` is char-width based and splits ASCII words
    // ("key" → "k"/"ey"), which reads badly in a modal. Padding is already inside
    // `block.inner`, so the wrap budget matches the rendered text column width.
    let wrap_w = block.inner(rect).width.saturating_sub(1) as usize;
    let mut lines = wrap_words(&confirm.message, wrap_w)
        .into_iter()
        .take(2)
        .map(|line| Line::from(Span::styled(line, th.style(Sem::Muted))))
        .collect::<Vec<_>>();
    lines.push(Line::from(String::new()));
    lines.push(Line::from(vec![
        confirm_option_span(state, &options[0], confirm.selected == 0, false),
        Span::raw("   "),
        confirm_option_span(state, &options[1], confirm.selected == 1, confirm.danger),
    ]));
    f.render_widget(Clear, rect);
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

/// Word-aware wrap for dialog messages: breaks at whitespace when possible so ASCII
/// words never split mid-word; a single token longer than the line still char-wraps.
fn wrap_words(text: &str, max: usize) -> Vec<String> {
    if max == 0 {
        return vec![text.to_string()];
    }
    let mut out: Vec<String> = Vec::new();
    for logical in text.split('\n') {
        let mut cur = String::new();
        let mut w = 0usize;
        for word in logical.split_whitespace() {
            let word_w = UnicodeWidthStr::width(word);
            let gap = usize::from(!cur.is_empty());
            if w + gap + word_w > max && !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
                w = 0;
            }
            if word_w > max {
                for line in wrap_line(word, max) {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                    cur = line;
                }
                w = UnicodeWidthStr::width(cur.as_str());
                continue;
            }
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(word);
            w += gap + word_w;
        }
        out.push(cur);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn confirm_option_span(
    state: &AppState,
    label: &str,
    selected: bool,
    danger: bool,
) -> Span<'static> {
    let sem = if danger { Sem::Error } else { Sem::Accent };
    Span::styled(
        format!(
            "{} {}",
            if selected {
                Glyph::Prompt.render(state.icons)
            } else {
                " "
            },
            label
        ),
        state
            .theme
            .style(if selected { sem } else { Sem::Muted })
            .add_modifier(if selected {
                Modifier::BOLD
            } else {
                Modifier::empty()
            }),
    )
}

fn truncate_display(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut width = 0usize;
    for ch in text.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width > max_width {
            if width < max_width {
                out.push('…');
            }
            return out;
        }
        out.push(ch);
        width += ch_width;
    }
    out
}

fn truncate_display_ascii_ellipsis(text: &str, max_width: usize) -> String {
    const ELLIPSIS: &str = "...";
    if max_width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_string();
    }
    if max_width <= ELLIPSIS.len() {
        return ".".repeat(max_width);
    }
    let content_width = max_width - ELLIPSIS.len();
    let mut out = String::new();
    let mut width = 0usize;
    for ch in text.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width > content_width {
            break;
        }
        out.push(ch);
        width += ch_width;
    }
    out.push_str(ELLIPSIS);
    out
}

fn pad_display_width(text: &str, width: usize) -> String {
    let clipped = truncate_display(text, width);
    let padding = width.saturating_sub(UnicodeWidthStr::width(clipped.as_str()));
    format!("{clipped}{}", " ".repeat(padding))
}

fn render_stats_overlay(state: &AppState, stats: &StatsOverlay, f: &mut Frame, area: Rect) {
    let th = &state.theme;
    let usage = &stats.usage;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(overlay_border_type(state))
        .border_style(Style::default().fg(th.color(Sem::AccentSoft)))
        .title(Span::styled(
            format!(" {} ", state.i18n.t("stats-title")),
            th.style(Sem::Muted).add_modifier(Modifier::BOLD),
        ));
    let inner = inset_rect(block.inner(area), 2, 1);
    f.render_widget(block, area);

    let width = inner.width as usize;
    let [range_area, spacer_area, body_area, footer_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(inner);
    f.render_widget(
        Paragraph::new(stats_range_line(state, stats.range)),
        range_area,
    );
    f.render_widget(Paragraph::new(Line::default()), spacer_area);

    let mut lines = Vec::new();
    let timeline = ranged_timeline(&stats.timeline, stats.range);
    let timeline_costs: Vec<f64> = timeline.iter().map(|day| day.cost).collect();
    lines.push(Line::from(vec![
        Span::styled(
            format_cost_line(state, "stats-today-cost", usage.today_cost),
            Style::default()
                .fg(th.color(Sem::Accent))
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("   "),
        Span::styled(
            format_cost_line(state, "stats-range-cost", timeline_costs.iter().sum()),
            Style::default().fg(th.color(Sem::AccentSoft)),
        ),
        Span::raw("   "),
        Span::styled(
            format_cost_line(state, "stats-total-cost", usage.total_cost),
            th.style(Sem::Muted),
        ),
    ]));
    let mut cache_args = fluent_bundle::FluentArgs::new();
    cache_args.set("hit", format!("{:.0}", usage.cache_hit_rate * 100.0));
    cache_args.set("savings", format!("{:.2}", usage.cache_savings));
    lines.push(Line::from(vec![Span::styled(
        state.i18n.format("stats-cache-summary", Some(&cache_args)),
        th.style(Sem::Muted),
    )]));
    lines.push(Line::from(vec![
        Span::styled("Tokens ", th.style(Sem::Muted)),
        Span::styled(
            compact_u64(usage.total_tokens),
            Style::default().fg(th.color(Sem::AccentSoft)),
        ),
        Span::styled("  in ", th.style(Sem::Muted)),
        Span::styled(compact_u64(usage.input_tokens), th.style(Sem::Muted)),
        Span::styled("  out ", th.style(Sem::Muted)),
        Span::styled(compact_u64(usage.output_tokens), th.style(Sem::Muted)),
        Span::styled("  turns ", th.style(Sem::Muted)),
        Span::styled(
            usage.turn_count.to_string(),
            Style::default().fg(th.color(Sem::AccentSoft)),
        ),
        Span::styled("  req ", th.style(Sem::Muted)),
        Span::styled(
            usage.request_count.to_string(),
            Style::default().fg(th.color(Sem::AccentSoft)),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled(
            format!("{} ", state.i18n.t("stats-cache")),
            th.style(Sem::Muted),
        ),
        Span::styled(
            format!("{} ", state.i18n.t("stats-cache-read")),
            th.style(Sem::Muted),
        ),
        Span::styled(
            compact_u64(usage.cache_read_tokens),
            Style::default().fg(th.color(Sem::AccentSoft)),
        ),
        Span::styled(
            format!("  {} ", state.i18n.t("stats-cache-write")),
            th.style(Sem::Muted),
        ),
        Span::styled(
            compact_u64(usage.cache_write_tokens),
            Style::default().fg(th.color(Sem::AccentSoft)),
        ),
    ]));
    let context_pct = if usage.context_limit_tokens == 0 {
        0.0
    } else {
        usage.context_used_tokens as f64 / usage.context_limit_tokens as f64 * 100.0
    };
    lines.push(Line::from(vec![
        Span::styled(
            format!("{} ", state.i18n.t("stats-context")),
            th.style(Sem::Muted),
        ),
        Span::styled(
            format!(
                "{} / {} ({context_pct:.0}%)",
                compact_u64(usage.context_used_tokens),
                compact_u64(usage.context_limit_tokens)
            ),
            Style::default().fg(if context_pct >= 85.0 {
                th.color(Sem::Warning)
            } else {
                th.color(Sem::AccentSoft)
            }),
        ),
    ]));
    lines.push(Line::from(String::new()));
    lines.push(Line::from(vec![
        Span::styled(
            format!("{} ", state.i18n.t("stats-recent-trend")),
            th.style(Sem::Muted),
        ),
        Span::styled(
            chart::sparkline(&timeline_costs, width.saturating_sub(10), state.icons),
            Style::default().fg(th.color(Sem::AccentSoft)),
        ),
    ]));
    lines.push(Line::from(String::new()));

    // Keep the range-sensitive daily chart ahead of the potentially long model
    // table. The whole body scrolls, so neither section needs to be discarded.
    if !timeline.is_empty() {
        lines.push(Line::from(Span::styled(
            state.i18n.t("stats-daily-usage"),
            Style::default()
                .fg(th.color(Sem::Accent))
                .add_modifier(Modifier::BOLD),
        )));
        lines.extend(chart::bar_chart(
            &timeline_costs,
            inner.width.saturating_sub(2),
            6,
            &state.theme,
            state.icons == IconTier::Ascii,
        ));
        lines.push(Line::from(String::new()));
    }

    lines.push(Line::from(Span::styled(
        state.i18n.t("stats-by-model"),
        Style::default()
            .fg(th.color(Sem::Accent))
            .add_modifier(Modifier::BOLD),
    )));
    lines.extend(stats_model_table(
        state,
        &stats.by_model,
        inner.width.saturating_sub(2),
    ));

    let viewport = body_area.height as usize;
    let total = lines.len();
    let start = viewport_scroll_start(stats.scroll, total, viewport);
    let end = (start + viewport).min(total);
    let visible = lines
        .into_iter()
        .skip(start)
        .take(end - start)
        .collect::<Vec<_>>();
    f.render_widget(Paragraph::new(visible), body_area);

    let position = if end == 0 {
        String::new()
    } else {
        format!("  {}-{}/{}", start + 1, end, total)
    };
    let footer = format!("{}{}", state.i18n.t("stats-range-hint"), position);
    f.render_widget(Paragraph::new(fullscreen_hint(state, &footer)), footer_area);
}

/// Slice the day-granularity timeline (oldest → newest) to the active range:
/// 1d → today only, 7d → last 7 days, 30d → last 30 days, month →
/// the complete current-month dataset loaded from core.
fn ranged_timeline(timeline: &[TurnUsage], range: StatsRange) -> &[TurnUsage] {
    let Some(days) = range.days() else {
        return timeline;
    };
    &timeline[timeline.len().saturating_sub(days)..]
}

fn inset_rect(area: Rect, x: u16, y: u16) -> Rect {
    Rect {
        x: area.x.saturating_add(x),
        y: area.y.saturating_add(y),
        width: area.width.saturating_sub(x.saturating_mul(2)),
        height: area.height.saturating_sub(y.saturating_mul(2)),
    }
}

fn centered_rect(
    area: Rect,
    max_width: u16,
    max_height: u16,
    margin_x: u16,
    margin_y: u16,
) -> Rect {
    let horizontal_margin = margin_x.saturating_mul(2);
    let vertical_margin = margin_y.saturating_mul(2);
    let available_width = if area.width > horizontal_margin {
        area.width - horizontal_margin
    } else {
        area.width
    };
    let available_height = if area.height > vertical_margin {
        area.height - vertical_margin
    } else {
        area.height
    };
    let width = max_width.min(available_width);
    let height = max_height.min(available_height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn stats_range_line(state: &AppState, active: StatsRange) -> Line<'static> {
    let th = &state.theme;
    let ranges = [
        ("1d".to_string(), StatsRange::OneDay),
        ("7d".to_string(), StatsRange::SevenDays),
        ("30d".to_string(), StatsRange::ThirtyDays),
        (state.i18n.t("stats-range-month"), StatsRange::Month),
    ];
    let mut spans = vec![Span::styled(
        format!("{} ", state.i18n.t("stats-range-label")),
        th.style(Sem::Muted),
    )];
    for (i, (label, range)) in ranges.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        let selected = *range == active;
        let text = if selected {
            format!("[{label}]")
        } else {
            label.to_string()
        };
        spans.push(Span::styled(
            text,
            th.style(if selected { Sem::Accent } else { Sem::Muted })
                .add_modifier(if selected {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        ));
    }
    Line::from(spans)
}

fn format_cost_line(state: &AppState, key: &str, cost: f64) -> String {
    let mut args = fluent_bundle::FluentArgs::new();
    args.set("cost", format!("{cost:.2}"));
    state.i18n.format(key, Some(&args))
}

fn stats_model_table(state: &AppState, by_model: &[ModelUsage], width: u16) -> Vec<Line<'static>> {
    let columns = vec![
        Column {
            header: "MODEL".into(),
            min: 12,
            max: 28,
            priority: 3,
            align: Align::Left,
        },
        Column {
            header: "TOKENS".into(),
            min: 8,
            max: 12,
            priority: 2,
            align: Align::Right,
        },
        Column {
            header: "SHARE".into(),
            min: 5,
            max: 7,
            priority: 1,
            align: Align::Right,
        },
        Column {
            header: "COST".into(),
            min: 7,
            max: 9,
            priority: 2,
            align: Align::Right,
        },
        Column {
            header: "TTFT".into(),
            min: 6,
            max: 8,
            priority: 1,
            align: Align::Right,
        },
        Column {
            header: "TOK/S".into(),
            min: 6,
            max: 8,
            priority: 1,
            align: Align::Right,
        },
    ];
    let rows: Vec<Vec<String>> = by_model
        .iter()
        .map(|m| {
            vec![
                m.model.clone(),
                compact_u64(m.tokens),
                format!("{:.0}%", m.share * 100.0),
                format!("${:.2}", m.cost),
                m.ttft_p50_ms
                    .map(|v| format!("{v}ms"))
                    .unwrap_or_else(|| "—".into()),
                m.throughput_tok_s
                    .map(|v| format!("{v:.0}"))
                    .unwrap_or_else(|| "—".into()),
            ]
        })
        .collect();
    // Reserve 2 cols for the series marker (`■ ` / `# `) prefixed to every row.
    let mut lines = table::render_table(
        &columns,
        &rows,
        width.saturating_sub(2),
        state.theme.style(Sem::Muted),
        state.theme.style(Sem::Muted),
        state.theme.style(Sem::Muted),
        state.icons,
    );
    let marker = chart::series_marker(state.icons == IconTier::Ascii);
    for (i, line) in lines.iter_mut().enumerate() {
        // render_table layout: [0] header, [1] rule, [2..] data rows — data row `n`
        // gets the `Sem::Chart(n)` cycling series colour, chrome rows stay aligned.
        if i >= 2 {
            line.spans.insert(
                0,
                Span::styled(format!("{marker} "), state.theme.style(Sem::Chart(i - 2))),
            );
        } else {
            line.spans.insert(0, Span::raw("  "));
        }
    }
    lines
}

fn compact_u64(v: u64) -> String {
    if v >= 1_000_000 {
        format!("{:.1}M", v as f64 / 1_000_000.0)
    } else if v >= 1_000 {
        format!("{:.1}K", v as f64 / 1_000.0)
    } else {
        v.to_string()
    }
}

fn live_lines(state: &AppState) -> Vec<Line<'static>> {
    timeline::live_lines(state, braille_spinner(state))
}

fn boot_lines(state: &AppState, width: u16, height: u16) -> Vec<Line<'static>> {
    let mut lines = boot_logo_lines(state, width);
    lines.push(Line::from(String::new()));
    lines.extend(boot_checklist_lines(state, width));
    top_padded_lines(
        lines,
        splash_top_padding(height, state.help.len(), state.icons),
        height as usize,
    )
}

fn exit_lines(state: &AppState, width: u16, height: u16) -> Vec<Line<'static>> {
    let mut lines = boot_logo_lines(state, width);
    lines.push(Line::from(String::new()));
    lines.extend(exit_resume_lines(state, width));
    top_padded_lines(
        lines,
        splash_top_padding(height.saturating_sub(4), state.help.len(), state.icons),
        height as usize,
    )
}

fn boot_logo_lines(state: &AppState, width: u16) -> Vec<Line<'static>> {
    let pad = splash::indent(width, state.icons);
    (0..splash::logo_height(state.icons) as usize)
        .map(|row| {
            let mut spans = vec![Span::raw(pad.clone())];
            spans.extend(
                splash::logo_row_chunks(&state.theme, state.icons, row)
                    .into_iter()
                    .map(|(text, color)| Span::styled(text, Style::default().fg(color))),
            );
            Line::from(spans)
        })
        .collect()
}

fn boot_checklist_lines(state: &AppState, width: u16) -> Vec<Line<'static>> {
    let th = &state.theme;
    let spin = spinner(state, state.anim_ticks as usize);
    let labels: Vec<String> = splash::LOADING
        .iter()
        .enumerate()
        .map(|(idx, _)| {
            if idx < state.boot_step {
                "[checked]".to_string()
            } else if idx == state.boot_step {
                format!("[loading {spin}]")
            } else {
                "[       ]".to_string()
            }
        })
        .collect();
    let block_width = labels
        .iter()
        .zip(splash::LOADING.iter())
        .map(|(label, msg)| UnicodeWidthStr::width(format!("{label} {msg}").as_str()))
        .max()
        .unwrap_or(0);
    let pad = " ".repeat((width as usize).saturating_sub(block_width) / 2);

    splash::LOADING
        .iter()
        .enumerate()
        .map(|(idx, msg)| {
            let style = if idx < state.boot_step {
                Style::default().fg(th.color(Sem::Success))
            } else if idx == state.boot_step {
                Style::default().fg(th.color(Sem::ToolRunning))
            } else {
                th.style(Sem::Muted)
            };
            Line::from(vec![
                Span::raw(pad.clone()),
                Span::styled(labels[idx].clone(), style.add_modifier(Modifier::BOLD)),
                Span::raw(" "),
                Span::styled(
                    (*msg).to_string(),
                    if idx <= state.boot_step {
                        th.style(Sem::AccentSoft)
                    } else {
                        th.style(Sem::Muted)
                    },
                ),
            ])
        })
        .collect()
}

fn exit_resume_lines(state: &AppState, width: u16) -> Vec<Line<'static>> {
    let th = &state.theme;
    let command = state
        .exit_resume_tip
        .clone()
        .unwrap_or_else(|| state.i18n.t("exit-resume-command-fallback"));
    let label = state.i18n.t("exit-resume-label");

    let content_w = UnicodeWidthStr::width(label.as_str())
        .max(UnicodeWidthStr::width(command.as_str()))
        .max(1);
    let box_w = content_w + 4; // + 2 for the left/right borders + 2 for the inner padding
    let left = (width as usize).saturating_sub(box_w) / 2;
    let pad = " ".repeat(left);
    let border = Style::default().fg(th.color(Sem::Border));
    let (tl, tr, bl, br, h, v) = match overlay_border_type(state) {
        BorderType::Rounded => ("╭", "╮", "╰", "╯", "─", "│"),
        _ => ("+", "+", "+", "+", "-", "|"),
    };
    let inner = box_w - 2;

    let mut lines = Vec::with_capacity(5);
    lines.push(Line::from(Span::styled(
        format!("{pad}{tl}{}{tr}", h.repeat(inner)),
        border,
    )));
    lines.push(splash_card_row(
        &pad,
        v,
        &label,
        inner,
        border,
        th.style(Sem::Muted),
    ));
    lines.push(splash_card_row(
        &pad,
        v,
        "",
        inner,
        border,
        Style::default(),
    ));
    lines.push(splash_card_row(
        &pad,
        v,
        &command,
        inner,
        border,
        Style::default()
            .fg(th.color(Sem::AccentSoft))
            .add_modifier(Modifier::BOLD),
    ));
    lines.push(Line::from(Span::styled(
        format!("{pad}{bl}{}{br}", h.repeat(inner)),
        border,
    )));
    lines
}

fn splash_card_row(
    pad: &str,
    v: &str,
    content: &str,
    inner: usize,
    border: Style,
    content_style: Style,
) -> Line<'static> {
    let content_w = UnicodeWidthStr::width(content);
    let right = inner.saturating_sub(content_w + 2); // one space of slack on each side
    Line::from(vec![
        Span::styled(format!("{pad}{v} "), border),
        Span::styled(content.to_string(), content_style),
        Span::styled(format!("{} {v}", " ".repeat(right)), border),
    ])
}

fn render_command_overlay(state: &AppState, f: &mut Frame, area: Rect) {
    let Some(suggest) = state.command_suggest() else {
        return;
    };
    if area.height < 5 || area.width < 28 {
        return;
    }
    let th = &state.theme;
    let height = (suggest.items.len() as u16 + 4).clamp(5, area.height.min(13));
    let width = area.width.clamp(34, 88);
    let x = area.x;
    let y = area.y + area.height.saturating_sub(height);
    let rect = Rect::new(x, y, width, height);

    let title = format!(" {} ", state.i18n.t("command-overlay-title"));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(overlay_border_type(state))
        .border_style(Style::default().fg(th.color(Sem::AccentSoft)))
        .padding(Padding::horizontal(1))
        .title(Span::styled(
            title,
            Style::default()
                .fg(th.color(Sem::Info))
                .add_modifier(Modifier::BOLD),
        ));

    let visible_rows = height.saturating_sub(4).max(1) as usize;
    let selected = suggest.selected.min(suggest.items.len().saturating_sub(1));
    let start = selected.saturating_sub(visible_rows.saturating_sub(1));
    let end = (start + visible_rows).min(suggest.items.len());
    let mut lines: Vec<Line> = if suggest.items.is_empty() {
        vec![Line::from(Span::styled(
            state.i18n.t("command-empty"),
            th.style(Sem::Muted),
        ))]
    } else {
        suggest.items[start..end]
            .iter()
            .enumerate()
            .map(|(offset, item)| {
                render_suggest_item(
                    state,
                    item,
                    start + offset == selected,
                    &suggest.query,
                    rect.width.saturating_sub(4) as usize,
                )
            })
            .collect()
    };
    lines.push(Line::from(String::new()));
    lines.push(styled_hint(state, &state.i18n.t("command-hint")));

    f.render_widget(Clear, rect);
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

fn render_suggest_item(
    state: &AppState,
    item: &SuggestItem,
    selected: bool,
    query: &str,
    content_width: usize,
) -> Line<'static> {
    let th = &state.theme;
    let marker = selection_marker(state, selected);
    let marker_style = th.style(if selected { Sem::Accent } else { Sem::Muted });
    match item {
        SuggestItem::Command(cmd) => {
            let kind = match cmd.kind {
                CommandKind::Builtin => state.i18n.t("command-kind-builtin"),
                CommandKind::Skill => state.i18n.t("command-kind-skill"),
            };
            let command_sem = if cmd.name == "help" {
                Sem::Info
            } else if selected {
                Sem::Accent
            } else {
                Sem::AccentSoft
            };
            let command_style = th.style(command_sem).add_modifier(if selected {
                Modifier::BOLD
            } else {
                Modifier::empty()
            });
            let kind_sem = if cmd.kind == CommandKind::Builtin {
                Sem::Info
            } else {
                Sem::Muted
            };
            let command_name = truncate_display(&cmd.name, 18);
            let description_width = content_width
                .saturating_sub(2 + 1 + UnicodeWidthStr::width(command_name.as_str()) + 2 + 7 + 2);
            let mut spans = vec![
                Span::styled(marker, marker_style),
                Span::styled("/", command_style),
            ];
            spans.extend(highlight_match(
                &command_name,
                query,
                command_style,
                Style::default()
                    .fg(th.color(Sem::Accent))
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(format!("  {kind}  "), th.style(kind_sem)));
            spans.push(Span::styled(
                truncate_display(&cmd.description, description_width),
                th.style(Sem::Muted),
            ));
            Line::from(spans)
        }
        SuggestItem::Argument {
            command,
            value,
            description,
        } => {
            let command = truncate_display(command, 14);
            let value = truncate_display(value, 14);
            let command_style = Style::default()
                .fg(th.color(if selected {
                    Sem::Accent
                } else {
                    Sem::AccentSoft
                }))
                .add_modifier(if selected {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                });
            let value_style = Style::default()
                .fg(th.color(Sem::Info))
                .add_modifier(if selected {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                });
            let description_width = content_width.saturating_sub(
                2 + 1
                    + UnicodeWidthStr::width(command.as_str())
                    + 1
                    + UnicodeWidthStr::width(value.as_str())
                    + 2
                    + 7
                    + 2,
            );
            Line::from(vec![
                Span::styled(marker, marker_style),
                Span::styled("/", command_style),
                Span::styled(command, command_style),
                Span::raw(" "),
                Span::styled(value, value_style),
                Span::styled(
                    format!("  {:<7}  ", state.i18n.t("command-kind-option")),
                    Style::default().fg(th.color(Sem::Info)),
                ),
                Span::styled(
                    truncate_display(description, description_width),
                    th.style(Sem::Muted),
                ),
            ])
        }
    }
}

fn render_file_picker_overlay(state: &AppState, f: &mut Frame, area: Rect) {
    let Some(picker) = state.file_picker() else {
        return;
    };
    if area.height < 3 || area.width < 20 {
        return;
    }
    let th = &state.theme;
    let height = (picker.entries.len() as u16 + 3).clamp(3, area.height.min(14));
    let width = area.width.clamp(20, 90);
    let x = area.x;
    let y = area.y + area.height.saturating_sub(height);
    let rect = Rect::new(x, y, width, height);

    let title = format!(" {} ", state.i18n.t("file-picker-title"));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(overlay_border_type(state))
        .border_style(Style::default().fg(th.color(Sem::AccentSoft)))
        .title(Span::styled(
            title,
            th.style(Sem::Muted).add_modifier(Modifier::BOLD),
        ));

    let visible_rows = height.saturating_sub(3).max(1) as usize;
    let selected = picker.selected.min(picker.entries.len().saturating_sub(1));
    let start = selected.saturating_sub(visible_rows.saturating_sub(1));
    let end = (start + visible_rows).min(picker.entries.len());
    let name_width = rect.width.saturating_sub(7) as usize;
    let mut lines: Vec<Line> = picker.entries[start..end]
        .iter()
        .enumerate()
        .map(|(offset, entry)| {
            let idx = start + offset;
            let marker = selection_marker(state, idx == selected);
            let glyph = if entry.is_dir {
                Glyph::Dir
            } else {
                Glyph::File
            };
            let name = if entry.is_parent {
                "../".to_string()
            } else {
                middle_ellipsis(&entry.display, name_width)
            };
            let style = th
                .style(if entry.is_dir {
                    Sem::AccentSoft
                } else {
                    Sem::Muted
                })
                .add_modifier(if idx == selected {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                });
            let mut spans = vec![
                Span::styled(
                    marker,
                    th.style(if idx == selected {
                        Sem::Accent
                    } else {
                        Sem::Muted
                    }),
                ),
                Span::styled(format!("{} ", glyph.render(state.icons)), style),
            ];
            spans.extend(highlight_match(
                &name,
                &picker.query,
                style,
                Style::default()
                    .fg(th.color(Sem::Accent))
                    .add_modifier(Modifier::BOLD),
            ));
            Line::from(spans)
        })
        .collect();
    lines.push(styled_hint(state, &state.i18n.t("file-picker-hint")));

    f.render_widget(Clear, rect);
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

fn highlight_match(text: &str, query: &str, base: Style, highlight: Style) -> Vec<Span<'static>> {
    if query.is_empty() {
        return vec![Span::styled(text.to_string(), base)];
    }
    let lower = text.to_lowercase();
    let needle = query.to_lowercase();
    let Some(start) = lower.find(&needle) else {
        return vec![Span::styled(text.to_string(), base)];
    };
    let end = start + needle.len();
    if !text.is_char_boundary(start) || !text.is_char_boundary(end) {
        return vec![Span::styled(text.to_string(), base)];
    }
    let mut spans = Vec::new();
    if start > 0 {
        spans.push(Span::styled(text[..start].to_string(), base));
    }
    spans.push(Span::styled(text[start..end].to_string(), highlight));
    if end < text.len() {
        spans.push(Span::styled(text[end..].to_string(), base));
    }
    spans
}

fn middle_ellipsis(s: &str, max_chars: usize) -> String {
    let len = s.chars().count();
    if len <= max_chars || max_chars < 8 {
        return s.to_string();
    }
    let keep = max_chars - 1;
    let head = keep / 2;
    let tail = keep - head;
    let start: String = s.chars().take(head).collect();
    let end: String = s
        .chars()
        .rev()
        .take(tail)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{start}…{end}")
}

/// Draw the input box. Returns the caret cell; `view` decides whether the hardware
/// cursor is placed there visibly (input editable), owned by a form text field, or
/// merely parked there hidden (see the `view` doc comment).
fn render_input(
    state: &AppState,
    f: &mut Frame,
    area: ratatui::layout::Rect,
    wrapped: &[String],
    content_lines: u16,
) -> Position {
    let th = &state.theme;
    let (badge, border_sem) = match state.mode {
        Mode::Normal => (Glyph::ModeNormal, Sem::Border),
        Mode::God => (Glyph::ModeGod, Sem::Error),
        Mode::Plan => (Glyph::ModePlan, Sem::Info),
        Mode::Streaming => (Glyph::ModePlan, Sem::Warning),
    };
    let input_title = if state.mode == Mode::Streaming {
        // Braille sweep spinner: a visible "still replying" cue at the bottom of the
        // screen while the model streams (see glyph::braille_spinner_frame).
        format!(" {} {} ", braille_spinner(state), badge.render(state.icons))
    } else {
        format!(" {} ", badge.render(state.icons))
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(overlay_border_type(state))
        .border_style(th.style(border_sem))
        // 1 col inside the border on each side (matches INPUT_CHROME in input_layout).
        .padding(Padding::horizontal(1))
        .title(Span::styled(
            input_title,
            Style::default().fg(th.color(Sem::AccentSoft)),
        ))
        .title(
            Line::from(Span::styled(
                input_identity_title(state, (area.width as usize).saturating_sub(14)),
                th.style(Sem::Muted),
            ))
            .right_aligned(),
        );

    // Show the last `content_lines` display lines (internal scroll when input exceeds
    // the cap). First visible line carries the `› ` prompt; continuations indent.
    let start = wrapped.len().saturating_sub(content_lines as usize);
    let visible = &wrapped[start..];
    let prompt = Style::default()
        .fg(th.color(Sem::Accent))
        .add_modifier(Modifier::BOLD);
    let mut lines: Vec<Line> = Vec::new();
    lines.extend(visible.iter().enumerate().map(|(i, s)| {
        let gutter = if i == 0 {
            Span::styled(format!("{} ", Glyph::Prompt.render(state.icons)), prompt)
        } else {
            Span::raw("  ")
        };
        let mut spans = vec![gutter];
        spans.extend(input_spans(state, s));
        Line::from(spans)
    }));

    f.render_widget(Paragraph::new(lines).block(block), area);

    // Hardware cursor cell — the cursor itself is KEPT hardware (not self-rendered)
    // because IME candidate windows anchor to it on macOS / Windows / Linux (the Ink
    // fork added hardware-cursor anchoring for exactly this). Whether it is
    // shown here, owned by a form text field, or parked hidden is decided by `view`.
    // Locate the caret's char index inside the wrapped rows, then map the row into
    // the visible window (the box scrolls internally past MAX_INPUT_LINES).
    let (caret_row, caret_prefix) = caret_in_wrapped(state, wrapped);
    let last = visible.last().map(|s| s.as_str()).unwrap_or("");
    let (row_off, col) = if caret_row >= start {
        (
            caret_row - start,
            UnicodeWidthStr::width(caret_prefix.as_str()),
        )
    } else {
        // Caret scrolled above the window; park at the tail of the visible text.
        (
            visible.len().saturating_sub(1),
            UnicodeWidthStr::width(last),
        )
    };
    // border(1) + padding(1) + gutter + text width. Clamp inside the right border.
    let cx = (area.x + 2 + GUTTER as u16 + col as u16).min(area.x + area.width.saturating_sub(2));
    let cy = (area.y + 1 + row_off as u16).min(area.y + area.height.saturating_sub(1));
    Position::new(cx, cy)
}

/// Walk the wrapped input rows (already split at hard newlines by `wrap_line`,
/// which never drops chars) accumulating char counts until the caret's char index
/// falls inside a row. Returns (row index, text before the caret within that row).
fn caret_in_wrapped(state: &AppState, wrapped: &[String]) -> (usize, String) {
    let mut left = state.input_cursor;
    for (i, line) in wrapped.iter().enumerate() {
        let n = line.chars().count();
        if left <= n {
            return (i, line.chars().take(left).collect());
        }
        left -= n;
    }
    // Caret past the last row (only reachable if cursor was clamped oddly); park
    // at the very end.
    let last = wrapped.len().saturating_sub(1);
    (last, wrapped.get(last).cloned().unwrap_or_default())
}

/// Whether to show (and anchor) the hardware cursor in the MAIN input. Only while it is
/// actually editable — hidden during boot/exit, under a fullscreen overlay, while a
/// pending interaction owns the keyboard, and while streaming with empty input
/// (watching output, not steering) so no stray cursor sits at the prompt.
fn caret_visible(state: &AppState) -> bool {
    // We do NOT gate on window focus: like vim/less/codex, we accept the terminal's own
    // hollow-cursor rendering when the window is unfocused (normal behavior, not worth
    // tracking `?1004` for).
    if state.stage != Stage::Ready || state.overlay_is_fullscreen() {
        return false;
    }
    if state.pending_interaction.is_some() {
        return false; // keys go to the interaction form, not the input box
    }
    match state.mode {
        Mode::Normal | Mode::Plan | Mode::God => true,
        Mode::Streaming => !state.input.is_empty(),
    }
}

fn input_spans(state: &AppState, text: &str) -> Vec<Span<'static>> {
    let th = &state.theme;
    let token_style = th.style(Sem::Muted).add_modifier(Modifier::BOLD);
    let mut spans = Vec::new();
    let mut raw = String::new();
    let mut idx = 0usize;

    while idx < text.len() {
        let rest = &text[idx..];
        if let Some(token) = state.chips.iter().find_map(|chip| match &chip.kind {
            ChipKind::Paste(paste) if rest.starts_with(&paste.display) => {
                Some(paste.display.as_str())
            }
            ChipKind::File(file) if rest.starts_with(&file.token) => Some(file.token.as_str()),
            _ => None,
        }) {
            if rest.starts_with(token) {
                if !raw.is_empty() {
                    spans.push(Span::raw(std::mem::take(&mut raw)));
                }
                spans.push(Span::styled(token.to_string(), token_style));
                idx += token.len();
                continue;
            }
        }

        let at_token_boundary = idx == 0
            || text[..idx]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace);
        if at_token_boundary && (rest.starts_with('/') || rest.starts_with('@')) {
            let token_len = rest
                .char_indices()
                .find_map(|(i, ch)| ch.is_whitespace().then_some(i))
                .unwrap_or(rest.len());
            if token_len > 1 {
                if !raw.is_empty() {
                    spans.push(Span::raw(std::mem::take(&mut raw)));
                }
                spans.push(Span::styled(rest[..token_len].to_string(), token_style));
                idx += token_len;
                continue;
            }
        }

        let ch = rest.chars().next().unwrap_or_default();
        raw.push(ch);
        idx += ch.len_utf8();
    }

    if !raw.is_empty() {
        spans.push(Span::raw(raw));
    }
    if spans.is_empty() {
        spans.push(Span::raw(String::new()));
    }
    spans
}

fn render_status(state: &AppState, f: &mut Frame, area: ratatui::layout::Rect) {
    let th = &state.theme;
    let width = area.width as usize;
    let right = status_right(state, width);
    let right_w = UnicodeWidthStr::width(right.as_str());
    let gap = if right.is_empty() { 0 } else { 2 };
    let left_w = width.saturating_sub(right_w).saturating_sub(gap);
    let left = compose_status_left(&status_fields(state, width), left_w);
    let pad = width
        .saturating_sub(UnicodeWidthStr::width(left.as_str()))
        .saturating_sub(right_w);
    let mut spans = vec![Span::styled(left, th.style(Sem::Muted))];
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
    }
    if !right.is_empty() {
        spans.push(Span::styled(
            right,
            Style::default().fg(th.color(Sem::AccentSoft)),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn input_identity_title(state: &AppState, avail: usize) -> String {
    let session = state.status_snapshot.session_title.clone().or_else(|| {
        state
            .status_snapshot
            .session_id
            .as_deref()
            .map(short_session_id)
    });
    let Some(title) = session else {
        return String::new();
    };
    let glyph = Glyph::Session.render(state.icons);
    let room = avail
        .saturating_sub(UnicodeWidthStr::width(glyph) + 3)
        .max(1);
    let title = truncate_display_ascii_ellipsis(&title, room);
    format!(" {glyph} {title} ")
}

/// Inputs to the layer-2 status line, with each field's pre-computed
/// shortened form so `compose_status_left` stays a pure width function.
pub(crate) struct StatusLineFields {
    /// Current directory name only; the full path is available from the shell.
    pub cwd: String,
    /// Only exceptional modes are shown; normal/streaming already have UI cues.
    pub mode: Option<String>,
    /// Auto approval is the default and stays hidden.
    pub appr: Option<String>,
    pub model_full: Option<String>,
    pub model_abbrev: Option<String>,
    /// Transient status message (command feedback) — first thing dropped.
    pub status: Option<String>,
}

fn status_fields(state: &AppState, _width: usize) -> StatusLineFields {
    let snap = &state.status_snapshot;
    StatusLineFields {
        cwd: last_path_segment(&snap.cwd),
        mode: match state.mode {
            Mode::God => Some("god".into()),
            Mode::Plan => Some("plan".into()),
            Mode::Normal | Mode::Streaming => None,
        },
        appr: match state.permission_mode {
            crate::session::dto::PermissionMode::Auto => None,
            crate::session::dto::PermissionMode::Deny => Some("appr deny".into()),
            crate::session::dto::PermissionMode::ApproveAll => Some("appr all".into()),
        },
        model_full: snap.model_identity(),
        model_abbrev: snap
            .provider
            .as_deref()
            .or(snap.model.as_deref())
            .map(provider_abbrev),
        status: None,
    }
}

/// Narrow-terminal priority truncation. Default mode/approval are absent before
/// this runs. As width shrinks: transient status → approval → exceptional mode
/// → abbreviated model, then hard-ellipsize.
pub(crate) fn compose_status_left(fields: &StatusLineFields, avail: usize) -> String {
    // (status, appr, mode, full_model)
    const LADDER: [(bool, bool, bool, bool); 5] = [
        (true, true, true, true),
        (false, true, true, true),
        (false, false, true, true),
        (false, false, false, true),
        (false, false, false, false),
    ];
    for step in LADDER {
        let candidate = join_status_fields(fields, step);
        if UnicodeWidthStr::width(candidate.as_str()) <= avail {
            return candidate;
        }
    }
    fit_status_left(
        &join_status_fields(fields, (false, false, false, false)),
        avail,
    )
}

fn join_status_fields(
    fields: &StatusLineFields,
    (status, appr, mode, full_model): (bool, bool, bool, bool),
) -> String {
    let mut parts: Vec<&str> = Vec::new();
    parts.push(&fields.cwd);
    if mode {
        if let Some(mode) = &fields.mode {
            parts.push(mode);
        }
    }
    if appr {
        if let Some(appr) = &fields.appr {
            parts.push(appr);
        }
    }
    let model = if full_model {
        fields.model_full.as_deref()
    } else {
        fields.model_abbrev.as_deref()
    };
    if let Some(model) = model {
        parts.push(model);
    }
    if status {
        if let Some(status) = &fields.status {
            parts.push(status);
        }
    }
    parts.join(" · ")
}

fn last_path_segment(cwd: &str) -> String {
    cwd.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .filter(|segment| !segment.is_empty())
        .unwrap_or(cwd)
        .to_string()
}

/// Provider abbreviation for the narrowest model form: the first two chars of
/// the provider segment (`deepseek/deepseek-r1` → `ds`).
fn provider_abbrev(model: &str) -> String {
    let provider = model.split('/').next().unwrap_or(model);
    provider.chars().take(2).collect()
}

fn status_right(state: &AppState, _width: usize) -> String {
    let snap = &state.status_snapshot;
    let mut parts = vec![format!("{} tok", compact_u64(snap.total_tokens))];
    if snap.context_limit_tokens > 0 {
        let pct = snap.context_used_tokens as f64 / snap.context_limit_tokens as f64 * 100.0;
        parts.push(format!("ctx {:.0}%", pct));
    }
    parts.join(" · ")
}

fn short_session_id(id: &str) -> String {
    if id.chars().count() <= 12 {
        id.to_string()
    } else {
        middle_ellipsis(id, 12)
    }
}

fn fit_status_left(s: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(s) <= max_width {
        return s.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    let ell = "…";
    let ell_w = UnicodeWidthStr::width(ell);
    if max_width <= ell_w {
        return ell.to_string();
    }
    let mut out = String::new();
    let mut width = 0usize;
    for ch in s.chars() {
        let ch_w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_w + ell_w > max_width {
            break;
        }
        out.push(ch);
        width += ch_w;
    }
    out.push_str(ell);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::SuggestState;
    use crate::glyph::IconTier;
    use crate::session::dto::{CommandKind, CommandSpec};
    use crate::theme::{themes, ColorTier, ThemeState};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn state() -> AppState {
        AppState::new(
            ThemeState {
                theme: themes::dark::theme(),
                tier: ColorTier::Rich,
            },
            IconTier::Ascii,
            80,
            24,
            24,
        )
    }

    #[test]
    fn fullscreen_selection_hint_matches_terminal_kind() {
        let mut s = state();
        for (kind, key) in [
            (TerminalKind::AppleTerminal, "overlay-mouse-select-apple"),
            (TerminalKind::ITerm2, "overlay-mouse-select-iterm"),
            (TerminalKind::Windows, "overlay-mouse-select-windows"),
            (TerminalKind::XtermLike, "overlay-mouse-select-xterm"),
        ] {
            s.terminal_kind = kind;
            let text = fullscreen_hint(&s, "Esc close")
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            assert!(text.contains(&s.i18n.t(key)), "kind={kind:?}: {text}");
        }
    }

    /// Regression: a settled reply longer than a few lines must render its NEWEST
    /// lines fully above the input — the re-wrap-on-already-wrapped bug used to clip
    /// the reply tail off the bottom (it "hid behind the input box").
    #[test]
    fn reply_tail_is_fully_visible_above_input() {
        use crate::render::history::HistLine;
        let mut s = AppState::new(
            ThemeState {
                theme: themes::dark::theme(),
                tier: ColorTier::Rich,
            },
            IconTier::Ascii,
            120,
            30,
            30,
        );
        s.stage = crate::app::Stage::Ready;
        let reply = "# Heading\n\nInline **bold**.\n\n- a\n  - b\n\n```diff\nUpdate f.rs (+2 -1)\n@@\n- old line here\n+ new line here\n+ tail marker line\n```";
        for mut line in crate::markdown::block::render_markdown(reply, &s.theme, s.icons) {
            line.spans.insert(0, Span::raw("  "));
            s.push_history_line(HistLine::from_line(line));
        }
        let th = history_tail_height(&s, 120, 30);
        s.queue_scrollback_until_tail(th as usize);
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| {
                let _ = view(&s, f);
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut screen = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                screen.push_str(buf[(x, y)].symbol());
            }
            screen.push('\n');
        }
        // The last diff line (newest content) must be on screen, not clipped.
        assert!(
            screen.contains("tail marker line"),
            "reply tail clipped off the bottom:\n{screen}"
        );
    }

    #[test]
    fn command_completion_is_floating_overlay() {
        let mut s = state();
        s.set_command_suggest(SuggestState {
            items: vec![SuggestItem::Command(CommandSpec {
                name: "help".to_string(),
                description: "Show help".to_string(),
                kind: CommandKind::Builtin,
            })],
            selected: 0,
            query: String::new(),
        });

        assert!(matches!(overlay_kind(&s), OverlayKind::Floating));
    }

    #[test]
    fn status_left_fits_width_with_unicode() {
        let fitted = fit_status_left("cwd ~/very-very-long-project-path · mode normal", 12);
        assert!(UnicodeWidthStr::width(fitted.as_str()) <= 12);
        assert!(fitted.ends_with('…'));
    }

    #[test]
    fn display_width_padding_and_truncation_never_overflow() {
        let padded = pad_display_width("用户", 10);
        assert_eq!(UnicodeWidthStr::width(padded.as_str()), 10);

        let clipped = truncate_display("ab", 1);
        assert_eq!(UnicodeWidthStr::width(clipped.as_str()), 1);
    }

    #[test]
    fn ascii_ellipsis_truncation_uses_three_dots() {
        assert_eq!(truncate_display_ascii_ellipsis("abcdef", 5), "ab...");
        assert_eq!(truncate_display_ascii_ellipsis("abcdef", 2), "..");
        assert_eq!(truncate_display_ascii_ellipsis("用户abcdef", 7), "用户...");
    }

    #[test]
    fn centered_rect_never_escapes_the_resized_terminal() {
        for (width, height) in [(120, 40), (44, 12), (20, 5), (1, 1), (0, 0)] {
            let area = Rect::new(3, 7, width, height);
            let card = centered_rect(area, 80, 20, 2, 1);
            assert!(card.x >= area.x && card.y >= area.y);
            assert!(card.right() <= area.right());
            assert!(card.bottom() <= area.bottom());
        }
    }

    #[test]
    fn scroll_offset_is_clamped_to_fill_the_resized_viewport() {
        assert_eq!(viewport_scroll_start(99, 100, 20), 80);
        assert_eq!(viewport_scroll_start(99, 100, 120), 0);
        assert_eq!(viewport_scroll_start(4, 100, 20), 4);
    }

    #[test]
    fn ascii_tier_uses_single_column_input_chrome() {
        let s = state();
        assert_eq!(selection_marker(&s, true), "> ");
        assert_eq!(spinner(&s, 0), "-");
        assert_eq!(Glyph::ModeNormal.render(s.icons), "*");
    }

    #[test]
    fn input_title_balances_the_mode_badge_on_both_sides() {
        let mut s = state();
        s.mode = Mode::Normal;
        let wrapped = wrap_line(&s.input, 40);
        let backend = TestBackend::new(40, 5);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 40, 5);
                let _ = render_input(&s, f, area, &wrapped, 1);
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let cells: Vec<char> = (0..buf.area.width)
            .map(|x| buf[(x, 0)].symbol().chars().next().unwrap_or(' '))
            .collect();
        let star = cells
            .iter()
            .position(|c| matches!(c, '●' | '*'))
            .expect("mode badge on the input title");
        assert!(star >= 1, "badge must not touch the left border");
        assert_eq!(cells[star - 1], ' ', "space before the badge");
        assert_eq!(cells[star + 1], ' ', "space after the badge");
    }

    fn status_fields_fixture() -> StatusLineFields {
        StatusLineFields {
            cwd: "cli-rs".into(),
            mode: Some("plan".into()),
            appr: Some("appr deny".into()),
            model_full: Some("deepseek-r1".into()),
            model_abbrev: Some("ds".into()),
            status: Some("Renamed session".into()),
        }
    }

    #[test]
    fn status_line_never_overflows_at_any_width() {
        let fields = status_fields_fixture();
        for avail in [200usize, 120, 100, 80, 60, 48, 40, 30, 12, 8, 1, 0] {
            let line = compose_status_left(&fields, avail);
            assert!(
                UnicodeWidthStr::width(line.as_str()) <= avail,
                "width {avail}: {line:?} overflows"
            );
        }
    }

    #[test]
    fn status_line_priority_truncation_follows_design_order() {
        let fields = status_fields_fixture();

        // Everything fits when wide.
        let wide = compose_status_left(&fields, 200);
        for part in [
            "cli-rs",
            "plan",
            "appr deny",
            "deepseek-r1",
            "Renamed session",
        ] {
            assert!(wide.contains(part), "wide line missing {part}: {wide:?}");
        }

        let without_status = compose_status_left(&fields, 55);
        assert!(!without_status.contains("Renamed session"));
        assert!(without_status.contains("appr deny"));

        let w35 = compose_status_left(&fields, 35);
        assert!(!w35.contains("appr deny"));
        assert!(w35.contains("cli-rs"));
        assert!(w35.contains("deepseek-r1"));

        let w40 = compose_status_left(&fields, 40);
        assert!(w40.contains("cli-rs"));
        assert!(w40.contains("deepseek-r1"));

        // Last ladder step: even the model shrinks to the provider abbreviation.
        let w15 = compose_status_left(&fields, 15);
        assert!(!w15.contains("deepseek-r1"));
        assert!(w15.contains("cli-rs"));
        assert!(w15.contains("ds"));
    }

    #[test]
    fn default_status_hides_theme_mode_and_approval_and_compacts_usage() {
        let mut state = state();
        state.status_snapshot.cwd = "~/IdeaProjects/zlogic/apps/cli-rs".into();
        state.status_snapshot.provider = Some("deepseek".into());
        state.status_snapshot.model = Some("deepseek/deepseek-r1".into());
        state.status_snapshot.total_tokens = 4_800_000;
        state.status_snapshot.context_used_tokens = 92_000;
        state.status_snapshot.context_limit_tokens = 128_000;

        let fields = status_fields(&state, 160);
        assert_eq!(fields.cwd, "cli-rs");
        assert!(fields.mode.is_none());
        assert!(fields.appr.is_none());
        assert_eq!(fields.model_full.as_deref(), Some("deepseek:deepseek-r1"));
        assert_eq!(status_right(&state, 160), "4.8M tok · ctx 72%");
    }

    #[test]
    fn session_title_truncation_counts_terminal_columns() {
        assert_eq!(truncate_display("abcdefghijklmnop", 8), "abcdefgh");
        assert_eq!(truncate_display("你好世界和平", 8), "你好世界");
        assert_eq!(truncate_display("你好世界和平", 9), "你好世界…");
    }

    #[test]
    fn status_line_shortening_helpers() {
        assert_eq!(last_path_segment("~/IdeaProjects/zlogic"), "zlogic");
        assert_eq!(last_path_segment("/"), "/");
        assert_eq!(last_path_segment("~"), "~");
        assert_eq!(
            last_path_segment(r"D:\code\sample-project"),
            "sample-project"
        );
        assert_eq!(last_path_segment(r"C:\"), "C:");
        assert_eq!(provider_abbrev("deepseek/deepseek-r1"), "de");
        assert_eq!(provider_abbrev("gpt-5"), "gp");
    }

    #[test]
    fn input_title_shows_session_without_model() {
        let mut s = state();
        s.status_snapshot.session_title = Some("修复 token 统计口径".into());
        s.status_snapshot.provider = Some("deepseek".into());
        s.status_snapshot.model = Some("deepseek/deepseek-r1".into());

        let title = input_identity_title(&s, 60);
        assert!(title.contains("修复"), "session title shown: {title:?}");
        assert!(
            !title.contains("deepseek"),
            "model identity not in the input title: {title:?}"
        );
        assert!(
            UnicodeWidthStr::width(title.as_str()) <= 60,
            "fits the available width: {title:?}"
        );

        s.status_snapshot.session_title = Some("一个非常非常非常非常非常非常长的会话标题".into());
        let cut = input_identity_title(&s, 30);
        assert!(
            cut.trim().ends_with("..."),
            "ellipsized with three dots: {cut:?}"
        );
        assert!(
            UnicodeWidthStr::width(cut.as_str()) <= 30,
            "never overflows: {cut:?}"
        );
        assert!(!cut.contains("长的会话标题"), "long tail dropped: {cut:?}");

        s.status_snapshot.session_title = None;
        s.status_snapshot.session_id = None;
        assert_eq!(input_identity_title(&s, 60), "");
    }

    #[test]
    fn toolbar_indicator_row_appears_only_with_content() {
        use crate::app::Notification;
        use crate::session::dto::NotificationLevel;

        let mut s = state();
        assert!(toolbar_indicator_line(&s).is_none());
        let tail_without = history_tail_height(&s, 80, 24);

        s.push_notification(Notification {
            level: NotificationLevel::Info,
            title: "t".into(),
            message: "m".into(),
            source: None,
            sticky: false,
        });
        let line = toolbar_indicator_line(&s).expect("indicator with content");
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.contains('1'), "count badge shows: {text:?}");
        assert!(
            text.contains(Glyph::Notice.render(s.icons)),
            "glyph respects the icon tier"
        );
        assert_eq!(
            history_tail_height(&s, 80, 24),
            tail_without - 2,
            "indicator + notification bar cost two tail rows"
        );
        let bar = notification_bar_line(&s, 80).expect("bar with content");
        let bar_text: String = bar.spans.iter().map(|span| span.content.as_ref()).collect();
        assert!(
            bar_text.contains("m") && bar_text.contains(Glyph::Notice.render(s.icons)),
            "bar shows message with glyph: {bar_text:?}"
        );
    }

    #[test]
    fn quit_confirm_hint_shows_in_bar_only_while_armed() {
        let mut s = state();
        assert!(
            notification_bar_line(&s, 80).is_none(),
            "no bar without notifications or quit-arm"
        );
        s.quit_armed_at = Some(std::time::Instant::now());
        let bar = notification_bar_line(&s, 80).expect("quit hint bar");
        let text: String = bar.spans.iter().map(|span| span.content.as_ref()).collect();
        assert!(
            text.contains("Ctrl+C"),
            "quit confirm hint shown above input: {text:?}"
        );
        s.quit_armed_at = Some(std::time::Instant::now() - std::time::Duration::from_secs(3));
        assert!(
            notification_bar_line(&s, 80).is_none(),
            "expired quit hint must not render"
        );
    }

    #[test]
    fn queue_card_combines_core_mailbox_and_local_follow_ups() {
        use crate::app::QueuedTurn;
        use crate::session::dto::{MailboxEntry, Message};

        let mut s = state();
        s.mode = Mode::Streaming;
        s.live.mailbox.push(MailboxEntry {
            id: "mailbox-1".into(),
            messages: vec![Message::text("inject now")],
        });
        s.next_turn_queue.push_back(QueuedTurn {
            messages: vec![Message::text("ask later")],
            display: "ask later".into(),
        });
        assert_eq!(queue_card_height(&s), 4);

        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                let _ = view(&s, frame);
            })
            .expect("queue card renders");
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        assert!(text.contains("inject now"));
        assert!(text.contains("ask later"));
        assert!(text.contains("Ctrl+T"));
        assert!(text.contains('+'), "ASCII queue uses a plain border");
    }

    #[test]
    fn ranged_timeline_filters_to_the_last_n_days() {
        let timeline: Vec<TurnUsage> = (1..=14)
            .map(|i| TurnUsage {
                turn_id: format!("day-{i:02}"),
                cost: i as f64,
                tokens_in: 0,
                tokens_out: 0,
            })
            .collect();
        assert_eq!(ranged_timeline(&timeline, StatsRange::OneDay).len(), 1);
        assert_eq!(
            ranged_timeline(&timeline, StatsRange::OneDay)[0].turn_id,
            "day-14"
        );
        assert_eq!(ranged_timeline(&timeline, StatsRange::SevenDays).len(), 7);
        assert_eq!(
            ranged_timeline(&timeline, StatsRange::SevenDays)[0].turn_id,
            "day-08"
        );
        assert_eq!(ranged_timeline(&timeline, StatsRange::ThirtyDays).len(), 14);
        assert_eq!(ranged_timeline(&timeline, StatsRange::Month).len(), 14);
    }

    #[test]
    fn stats_overlay_keeps_daily_usage_visible_and_scrolls_to_models() {
        use crate::session::dto::{ModelUsage, TurnUsage, UsageSnapshot};

        let usage = UsageSnapshot {
            daily_cost: (0..30).map(|_| 0.1).collect(),
            today_cost: 0.42,
            total_cost: 12.3,
            total_tokens: 1_000_000,
            input_tokens: 700_000,
            output_tokens: 300_000,
            request_count: 120,
            turn_count: 14,
            by_model: vec![
                ModelUsage {
                    model: "model-a".into(),
                    tokens: 600_000,
                    share: 0.6,
                    cost: 7.4,
                    ..Default::default()
                },
                ModelUsage {
                    model: "model-b".into(),
                    tokens: 400_000,
                    share: 0.4,
                    cost: 4.9,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let by_model = usage.by_model.clone();
        let timeline: Vec<TurnUsage> = (0..14)
            .map(|i| TurnUsage {
                turn_id: format!("day-{:02}", i + 1),
                ..Default::default()
            })
            .collect();
        for height in [40u16, 24u16] {
            let backend = TestBackend::new(100, height);
            let mut terminal = Terminal::new(backend).expect("terminal");
            let mut s = state();
            s.set_stats_overlay(usage.clone(), by_model.clone(), timeline.clone());
            let render_text = |terminal: &mut Terminal<TestBackend>, s: &AppState| {
                terminal
                    .draw(|frame| {
                        let _ = view(s, frame);
                    })
                    .expect("stats overlay renders");
                let buffer = terminal.backend().buffer().clone();
                let mut text = String::new();
                for y in 0..buffer.area.height {
                    for x in 0..buffer.area.width {
                        text.push_str(buffer[(x, y)].symbol());
                    }
                    text.push('\n');
                }
                // Wide (CJK) glyphs leave continuation cells in the test buffer.
                text.replace(' ', "")
            };
            let by_model_heading = s.i18n.t("stats-by-model").replace(' ', "");
            let chart_heading = s.i18n.t("stats-daily-usage").replace(' ', "");

            let first_page = render_text(&mut terminal, &s);
            assert!(
                first_page.contains(&chart_heading),
                "range-sensitive chart renders on the first page at height {height}"
            );

            let Some(FullscreenOverlay::Stats(stats)) = s.fullscreen_overlay_mut() else {
                panic!("expected stats overlay");
            };
            stats.scroll = usize::MAX;
            let last_page = render_text(&mut terminal, &s);
            assert!(
                last_page.contains(&by_model_heading),
                "model table is reachable by scrolling at height {height}"
            );
        }
    }

    #[test]
    fn help_overlay_renders_after_shrinking_to_tiny_terminal() {
        for (width, height) in [(80, 24), (20, 5), (1, 1)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).expect("terminal");
            let mut s = state();
            s.set_help_overlay(Vec::new());
            terminal
                .draw(|frame| {
                    let _ = view(&s, frame);
                })
                .expect("help overlay should remain inside the resized frame");
        }
    }

    #[test]
    fn zoom_overlay_windows_lines_by_scroll_and_reports_percent() {
        let backend = TestBackend::new(76, 10); // 7 content rows inside the card
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut s = state();
        s.overlay = Some(crate::app::OverlayState::Fullscreen(
            FullscreenOverlay::Zoom(ZoomOverlay {
                title: "reply".into(),
                lines: (0..20).map(|n| Line::from(format!("row{n}"))).collect(),
                scroll: 5,
                kind: ZoomKind::Document,
                copy_text: Some("raw reply".into()),
                copy_feedback: None,
                restore: None,
            }),
        ));
        terminal
            .draw(|frame| {
                let _ = view(&s, frame);
            })
            .expect("zoom overlay draws");
        let text = format!("{:?}", terminal.backend().buffer());
        assert!(text.contains("reply"), "title rendered: {text}");
        assert!(text.contains("row5"), "window starts at scroll: {text}");
        assert!(text.contains("row11"), "window fills content rows: {text}");
        assert!(!text.contains("row12"), "rows past the window clipped");
        assert!(!text.contains("row4"), "rows before the window clipped");
        // View bottom = line 12 of 20 → 60%.
        assert!(text.contains("60%"), "footer percent rendered: {text}");
        assert!(text.contains("c copy"), "copy shortcut rendered: {text}");
        assert!(
            text.contains(&s.i18n.t("overlay-mouse-select-xterm")),
            "selection hint rendered: {text}"
        );

        // Tiny terminals must not panic (inner-area guard).
        for (width, height) in [(10, 3), (1, 1)] {
            let backend = TestBackend::new(width, height);
            let mut tiny = Terminal::new(backend).expect("terminal");
            tiny.draw(|frame| {
                let _ = view(&s, frame);
            })
            .expect("zoom overlay survives tiny frames");
        }
    }

    /// Regression: the missing-key confirm dialog renders with complete borders, the
    /// message never splits an ASCII word, and the text sits on a padded line — same
    /// card style as the provider key form (no dimmed background).
    #[test]
    fn models_confirm_dialog_padded_and_wrap() {
        use crate::app::{ConfirmAction, ConfirmDialog, FullscreenOverlay, OverlayState};
        use crate::i18n::{I18n, Locale};
        use crate::session::dto::{KeyStatus, ModelEntry, ProviderEntry, Tier};
        let mut s = AppState::new(
            ThemeState {
                theme: themes::dark::theme(),
                tier: ColorTier::Rich,
            },
            IconTier::Ascii,
            120,
            30,
            30,
        );
        // The wide-char wrapping this test is about only exists in the Chinese message.
        s.i18n = I18n::new(Locale::ZhCn);
        s.set_models_overlay(
            vec![ProviderEntry {
                name: "anthropic".into(),
                sdk: "anthropic".into(),
                base_url: Some("https://api.anthropic.com".into()),
                key: KeyStatus::Missing,
            }],
            vec![ModelEntry {
                name: "anthropic:claude-sonnet-5".into(),
                provider: "anthropic".into(),
                tier: Tier::Main,
                vision: true,
                key: KeyStatus::Missing,
                price: "—".into(),
                context_window: 200_000,
                is_current: false,
            }],
            Vec::new(),
            Some(r"C:\Users\u\.config\zlogic\models.yaml".into()),
            String::new(),
        );
        if let Some(OverlayState::Fullscreen(FullscreenOverlay::Models(o))) = &mut s.overlay {
            let mut args = fluent_bundle::FluentArgs::new();
            args.set("provider", "anthropic");
            args.set("model", "anthropic:claude-sonnet-5");
            o.confirm = Some(ConfirmDialog {
                title: s.i18n.t("models-prompt-key-title"),
                message: s.i18n.format("models-prompt-key-message", Some(&args)),
                confirm_label: s.i18n.t("models-prompt-key-bind"),
                cancel_label: s.i18n.t("form-option-cancel"),
                danger: false,
                selected: 0,
                action: ConfirmAction::BindProviderKey {
                    provider: "anthropic".into(),
                    model: "anthropic:claude-sonnet-5".into(),
                },
            });
        }
        s.stage = crate::app::Stage::Ready;
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| {
                let _ = view(&s, f);
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut screen = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                screen.push_str(buf[(x, y)].symbol());
            }
            screen.push('\n');
        }
        // Dialog: all four corners present (borders were "incomplete" before).
        for corner in ["┌", "┐", "└", "┘"] {
            assert!(
                screen.contains(corner),
                "dialog corner {corner:?} missing:\n{screen}"
            );
        }
        // The terminal's wide-char continuation cells dump as spaces between CJK
        // glyphs; strip them to compare logical text.
        let flat = screen.replace(' ', "");
        assert!(
            flat.contains("绑定APIkey"),
            "dialog title missing:\n{screen}"
        );
        // the old char wrap used to split "key" into "k"/"ey".
        assert!(
            flat.contains("key，绑定"),
            "message word split by wrap:\n{screen}"
        );
        assert!(
            !flat.contains("\ney，绑定"),
            "message word split by wrap (k/ey):\n{screen}"
        );
        // Padded text: the message sits two columns inside the card's left border (same
        // padding as the provider key form).
        let message_row = screen
            .lines()
            .find(|line| line.contains("provider（"))
            .unwrap_or_default();
        assert!(
            message_row.contains("│  anthropic:claude-sonnet-5"),
            "message not padded inside the card: {message_row:?}"
        );
    }

    /// The left list shows the model id + context/vision meta per model (Browse mode,
    /// no dialog — the confirm scrim covers the panes, so it is tested separately).
    #[test]
    fn models_left_list_shows_id_and_meta() {
        use crate::session::dto::{KeyStatus, ModelEntry, ProviderEntry, Tier};
        let mut s = AppState::new(
            ThemeState {
                theme: themes::dark::theme(),
                tier: ColorTier::Rich,
            },
            IconTier::Ascii,
            120,
            30,
            30,
        );
        s.set_models_overlay(
            vec![ProviderEntry {
                name: "anthropic".into(),
                sdk: "anthropic".into(),
                base_url: Some("https://api.anthropic.com".into()),
                key: KeyStatus::Missing,
            }],
            vec![
                ModelEntry {
                    name: "anthropic:claude-sonnet-5".into(),
                    provider: "anthropic".into(),
                    tier: Tier::Main,
                    vision: true,
                    key: KeyStatus::Missing,
                    price: "—".into(),
                    context_window: 200_000,
                    is_current: false,
                },
                ModelEntry {
                    name: "anthropic:claude-opus-4-8".into(),
                    provider: "anthropic".into(),
                    tier: Tier::Thinking,
                    vision: false,
                    key: KeyStatus::Missing,
                    price: "—".into(),
                    context_window: 0,
                    is_current: false,
                },
            ],
            Vec::new(),
            Some(r"C:\Users\u\.config\zlogic\models.yaml".into()),
            String::new(),
        );
        s.stage = crate::app::Stage::Ready;
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| {
                let _ = view(&s, f);
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut screen = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                screen.push_str(buf[(x, y)].symbol());
            }
            screen.push('\n');
        }
        // Left pane only (x < 50 ≈ 38% list + divider): model id without the provider
        // prefix + context/vision meta on the row. The right detail pane legitimately
        // shows the full provider:model name, so scope the check to the list.
        let mut left = String::new();
        for y in 0..buf.area.height {
            for x in 0..50.min(buf.area.width) {
                left.push_str(buf[(x, y)].symbol());
            }
        }
        let left_flat = left.replace(' ', "");
        assert!(
            left_flat.contains("claude-sonnet-5200K"),
            "left row lacks id + meta:\n{left}"
        );
        assert!(
            left_flat.contains("claude-opus-4-8"),
            "second id rendered:\n{left}"
        );
        assert!(
            !left_flat.contains("anthropic:claude"),
            "left list still shows prefixed name:\n{left}"
        );
        assert!(
            !left_flat.contains("缺失"),
            "left list still shows the missing-key tag:\n{left}"
        );
    }

    #[test]
    fn models_test_dialog_shows_running_then_result() {
        use crate::app::{FullscreenOverlay, ModelTest, OverlayState};
        use crate::session::dto::{ConnResult, KeyStatus, ModelEntry, ProviderEntry, Tier};
        let render = |result: Option<ConnResult>| -> String {
            let mut s = AppState::new(
                ThemeState {
                    theme: themes::dark::theme(),
                    tier: ColorTier::Rich,
                },
                IconTier::Ascii,
                120,
                30,
                30,
            );
            s.set_models_overlay(
                vec![ProviderEntry {
                    name: "anthropic".into(),
                    sdk: "anthropic".into(),
                    base_url: None,
                    key: KeyStatus::Present,
                }],
                vec![ModelEntry {
                    name: "anthropic:claude-sonnet-5".into(),
                    provider: "anthropic".into(),
                    tier: Tier::Main,
                    vision: false,
                    key: KeyStatus::Present,
                    price: "—".into(),
                    context_window: 200_000,
                    is_current: false,
                }],
                Vec::new(),
                Some(r"C:\Users\u\.config\zlogic\models.yaml".into()),
                String::new(),
            );
            if let Some(OverlayState::Fullscreen(FullscreenOverlay::Models(o))) = &mut s.overlay {
                o.test = Some(ModelTest {
                    model: "anthropic:claude-sonnet-5".into(),
                    result,
                });
            }
            s.stage = crate::app::Stage::Ready;
            let backend = TestBackend::new(120, 30);
            let mut terminal = Terminal::new(backend).expect("terminal");
            terminal
                .draw(|f| {
                    let _ = view(&s, f);
                })
                .unwrap();
            let buf = terminal.backend().buffer().clone();
            let mut screen = String::new();
            for y in 0..buf.area.height {
                for x in 0..buf.area.width {
                    screen.push_str(buf[(x, y)].symbol());
                }
                screen.push('\n');
            }
            screen.replace(' ', "")
        };
        let running = render(None);
        assert!(
            running.contains("Testmodel") && running.contains("Testingmodel"),
            "running dialog missing: {running}"
        );
        let done = render(Some(ConnResult {
            ok: true,
            latency_ms: 12,
            detail: "all good".into(),
        }));
        assert!(
            done.contains("ok·12ms·allgood"),
            "result dialog missing: {done}"
        );
        let failed = render(Some(ConnResult {
            ok: false,
            latency_ms: 0,
            detail: "no credential".into(),
        }));
        assert!(
            failed.contains("failed·0ms·nocredential"),
            "failed dialog missing: {failed}"
        );
    }

    #[test]
    fn models_detail_scroll_keeps_hint_pinned() {
        use crate::app::{FullscreenOverlay, OverlayState};
        use crate::session::dto::{KeyStatus, ModelEntry, ProviderEntry, Tier};
        let render = |detail_scroll: usize| -> String {
            let mut s = AppState::new(
                ThemeState {
                    theme: themes::dark::theme(),
                    tier: ColorTier::Rich,
                },
                IconTier::Ascii,
                70,
                14,
                14,
            );
            s.set_models_overlay(
                vec![ProviderEntry {
                    name: "anthropic".into(),
                    sdk: "anthropic".into(),
                    base_url: Some("https://api.anthropic.com/v1/messages".into()),
                    key: KeyStatus::Present,
                }],
                vec![ModelEntry {
                    name: "anthropic:claude-sonnet-5".into(),
                    provider: "anthropic".into(),
                    tier: Tier::Thinking,
                    vision: true,
                    key: KeyStatus::Present,
                    price: "$3.00/$15.00 per M".into(),
                    context_window: 200_000,
                    is_current: false,
                }],
                Vec::new(),
                Some(r"/home/u/.config/zlogic/models.yaml".into()),
                String::new(),
            );
            if let Some(OverlayState::Fullscreen(FullscreenOverlay::Models(o))) = &mut s.overlay {
                o.detail_scroll = detail_scroll;
            }
            s.stage = crate::app::Stage::Ready;
            let backend = TestBackend::new(70, 14);
            let mut terminal = Terminal::new(backend).expect("terminal");
            terminal
                .draw(|f| {
                    let _ = view(&s, f);
                })
                .unwrap();
            let buf = terminal.backend().buffer().clone();
            let mut screen = String::new();
            for y in 0..buf.area.height {
                for x in 0..buf.area.width {
                    screen.push_str(buf[(x, y)].symbol());
                }
                screen.push('\n');
            }
            screen.replace(' ', "")
        };

        let top = render(0);
        assert!(top.contains("Modeldetails"), "detail title missing: {top}");
        assert!(
            top.contains("Editmodels—changetheconfigfile") && top.contains("models.yaml"),
            "pinned hint missing at scroll 0:\n{top}"
        );

        let scrolled = render(10);
        assert!(
            !scrolled.contains("anthropic:claude"),
            "detail did not scroll:\n{scrolled}"
        );
        assert!(
            scrolled.contains("Editmodels—changetheconfigfile") && scrolled.contains("models.yaml"),
            "pinned hint scrolled away:\n{scrolled}"
        );
    }

    #[test]
    fn input_title_mode_badge_has_trailing_space() {
        let mut s = AppState::new(
            ThemeState {
                theme: themes::dark::theme(),
                tier: ColorTier::Rich,
            },
            IconTier::Unicode,
            80,
            24,
            24,
        );
        s.input = "hello world".into();
        s.input_cursor = s.input.chars().count();
        s.stage = crate::app::Stage::Ready;
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| {
                let _ = view(&s, f);
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut screen = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                screen.push_str(buf[(x, y)].symbol());
            }
            screen.push('\n');
        }
        assert!(
            screen.lines().any(|l| l.contains("● ─") && l.contains("╭")),
            "mode badge must keep exactly one trailing space:\n{screen}"
        );
    }

    #[test]
    fn exit_splash_logo_and_card_share_center() {
        let mut s = AppState::new(
            ThemeState {
                theme: themes::dark::theme(),
                tier: ColorTier::Rich,
            },
            IconTier::Ascii,
            120,
            30,
            30,
        );
        s.show_exit_splash("zlogic --resume 01234567-89ab-cdef-0123-456789abcdef".into());
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| {
                let _ = view(&s, f);
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut screen = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                screen.push_str(buf[(x, y)].symbol());
            }
            screen.push('\n');
        }
        let center = |needle: Option<&str>| -> Option<i32> {
            let line = needle?;
            let f = line.find(|c| c != ' ')?;
            let l = line.rfind(|c| c != ' ')?;
            Some(((f + l) / 2) as i32)
        };
        let logo_line = screen
            .lines()
            .find(|l| l.trim_start().starts_with("_") || l.trim_start().starts_with("|"))
            .expect("logo row exists");
        let card_line = screen
            .lines()
            .find(|l| l.contains("zlogic --resume"))
            .expect("card row exists");
        let logo = center(Some(logo_line)).expect("logo row");
        let card = center(Some(card_line)).expect("card row");
        let screen_center = (120 / 2) as i32;
        assert!(
            (logo - screen_center).abs() <= 1 && (card - screen_center).abs() <= 1,
            "logo {logo} / card {card} not centered (screen center {screen_center}):\n{screen}"
        );
        assert!(
            (logo - card).abs() <= 2,
            "logo {logo} vs card {card} centers misaligned:\n{screen}"
        );
    }

    #[test]
    fn sessions_history_hides_events_and_groups_messages_by_turn() {
        use crate::app::{SessionDetailMode, SessionsOverlay};
        use crate::session::dto::{HistoryItem, Message, MessageRole, SessionSummary};
        use std::collections::BTreeSet;

        let s = AppState::new(
            ThemeState {
                theme: themes::dark::theme(),
                tier: ColorTier::Rich,
            },
            IconTier::Ascii,
            120,
            30,
            30,
        );
        let summary = SessionSummary {
            id: "s1".into(),
            title: "hist".into(),
            renamed: false,
            group: "Today".into(),
            when: "14:02".into(),
            cost: 0.0,
            model: "m".into(),
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            context_used_tokens: 0,
            context_limit_tokens: 0,
            archived: false,
        };
        let history = vec![
            HistoryItem::Message {
                id: "u1".into(),
                turn_index: 0,
                role: MessageRole::User,
                message: Message::text("first question"),
            },
            HistoryItem::Event {
                id: "e1".into(),
                turn_index: 0,
                event: crate::session::dto::CoreEvent::ToolCallStart {
                    id: "c1".into(),
                    name: "shell".into(),
                    args: Default::default(),
                },
            },
            HistoryItem::Message {
                id: "a1".into(),
                turn_index: 0,
                role: MessageRole::Assistant,
                message: Message::text("first answer"),
            },
            HistoryItem::Message {
                id: "u2".into(),
                turn_index: 1,
                role: MessageRole::User,
                message: Message::text("second question"),
            },
        ];
        let overlay = SessionsOverlay {
            items: vec![summary],
            selected: 0,
            query: String::new(),
            searching: false,
            last_searched: None,
            renaming: None,
            history,
            detail_mode: SessionDetailMode::Preview,
            history_selected: 0,
            selection_mode: false,
            selected_session_ids: BTreeSet::new(),
            confirm: None,
        };

        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                render_session_history(&s, &overlay, frame, frame.area());
            })
            .expect("sessions history renders");

        let buf = terminal.backend().buffer().clone();
        let mut text = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                text.push_str(buf[(x, y)].symbol());
            }
            text.push('\n');
        }

        let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();

        assert!(
            !compact.contains("shell"),
            "tool call event rendered: {text}"
        );
        assert!(
            !compact.contains("event"),
            "event role label rendered: {text}"
        );
        assert!(compact.contains("firstanswer"), "{text}");
        assert!(compact.contains("secondquestion"), "{text}");
        let header_of = |n: i64| {
            let mut args = fluent_bundle::FluentArgs::new();
            args.set("n", n);
            s.i18n
                .format("sessions-history-turn", Some(&args))
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect::<String>()
        };
        let h0 = header_of(0);
        let h1 = header_of(1);
        assert_eq!(
            compact.matches(&h0).count(),
            1,
            "one header for turn 0: {text}"
        );
        assert_eq!(
            compact.matches(&h1).count(),
            1,
            "one header for turn 1: {text}"
        );
        let pos0 = compact.find(&h0).expect("turn 0 header");
        let pos1 = compact.find(&h1).expect("turn 1 header");
        let first_answer = compact.find("firstanswer").expect("first answer row");
        assert!(
            pos0 < first_answer && first_answer < pos1,
            "grouping order wrong: {text}"
        );
    }
}
