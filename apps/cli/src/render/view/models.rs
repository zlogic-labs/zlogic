use super::*;
use crate::app::{ModelRow, ModelTest};

pub(super) fn render_models_overlay(
    state: &AppState,
    models: &ModelsOverlay,
    f: &mut Frame,
    area: Rect,
) {
    let th = &state.theme;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(overlay_border_type(state))
        .border_style(Style::default().fg(th.color(Sem::AccentSoft)))
        .title(Span::styled(
            format!(" {} ", state.i18n.t("models-title")),
            th.style(Sem::Muted).add_modifier(Modifier::BOLD),
        ));
    let inner = inset_rect(block.inner(area), 2, 1);
    f.render_widget(block, area);

    // Blank spacer rows above the panes and above the footer give the panel room to
    // breathe (design polish: appropriate vertical padding).
    let rows = Layout::vertical([
        Constraint::Length(1), // subtitle
        Constraint::Length(1), // gap
        Constraint::Min(1),    // panes
        Constraint::Length(1), // gap
        Constraint::Length(1), // footer
    ])
    .split(inner);
    f.render_widget(
        Paragraph::new(styled_hint(state, &state.i18n.t("models-subtitle"))),
        rows[0],
    );

    let panes = Layout::horizontal([
        Constraint::Percentage(38),
        Constraint::Length(3),
        Constraint::Percentage(59),
    ])
    .split(rows[2]);
    render_model_list(state, models, f, panes[0]);
    render_pane_divider(state, f, panes[1]);
    render_model_detail(state, models, f, panes[2]);

    f.render_widget(
        Paragraph::new(fullscreen_hint(
            state,
            &state.i18n.t(model_hint_key(models)),
        )),
        rows[4],
    );

    if let Some(test) = &models.test {
        render_model_test_dialog(state, test, f, area);
    } else if let Some(confirm) = &models.confirm {
        render_confirm_dialog(state, confirm, f, area);
    } else if !matches!(models.mode, ModelOverlayMode::Browse) {
        render_model_form(state, models, f, area);
    }
}

/// Row marker + name color by selection. The list has a single flat cursor (provider
/// headers and model rows live in one list), so the selection is always bright accent
/// `›`; unselected rows stay quiet.
/// Sliding window over `len` rows keeping `selected` visible in `viewport` rows.
/// Reserves up to 2 rows for the "N more above/below" markers when clipping. Returns
/// the [start, end) slice to render.
fn window_around(len: usize, selected: usize, viewport: usize) -> (usize, usize) {
    if viewport == 0 {
        return (0, 0);
    }
    if len <= viewport {
        return (0, len);
    }
    let inner = viewport.saturating_sub(2).max(1);
    let start = selected.saturating_sub(inner / 2).min(len - inner);
    (start, start + inner)
}

fn model_row_style(state: &AppState, active: bool) -> (String, Sem, bool) {
    if active {
        (selection_marker(state, true), Sem::Accent, true)
    } else {
        ("  ".to_string(), Sem::AccentSoft, false)
    }
}

/// Left pane: providers grouped with their models beneath, one flat cursor.
fn render_model_list(state: &AppState, models: &ModelsOverlay, f: &mut Frame, area: Rect) {
    let th = &state.theme;
    let list_title = if models.filter.is_empty() {
        state.i18n.t("models-list-title")
    } else {
        format!("{} · {}", state.i18n.t("models-list-title"), models.filter)
    };
    let mut lines = vec![Line::from(Span::styled(
        list_title,
        th.style(Sem::Accent).add_modifier(Modifier::BOLD),
    ))];

    // Window the list around the selection so a long list scrolls instead
    // of overflowing the pane.
    let total = models.rows.len();
    let sel = models.selected;
    let viewport = (area.height as usize).saturating_sub(1); // minus the title row
    let (start, end) = window_around(total, sel, viewport);

    if start > 0 {
        lines.push(form_scroll_marker(state, "models-scroll-above", start));
    }
    for pos in start..end {
        let active = pos == models.selected;
        let (marker, name_sem, active_bold) = model_row_style(state, active);
        match &models.rows[pos] {
            ModelRow::Provider(pi) => {
                let provider = &models.providers[*pi];
                let name_w = (area.width as usize).saturating_sub(12).max(4);
                let mut spans = vec![
                    Span::styled(marker, th.style(name_sem)),
                    Span::styled(
                        format!("{} ", Glyph::Dir.render(state.icons)),
                        th.style(name_sem),
                    ),
                    Span::styled(
                        truncate_display(&provider.name, name_w),
                        th.style(name_sem).add_modifier(if active_bold {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                    ),
                ];
                if let Some(badge) = key_badge(state, provider.key) {
                    spans.push(Span::raw("  "));
                    spans.push(badge);
                }
                lines.push(Line::from(spans));
            }
            ModelRow::Model { provider, model } => {
                let model = &models.models[*model];
                let provider_name = models
                    .providers
                    .get(*provider)
                    .map(|p| p.name.as_str())
                    .unwrap_or("");
                let id = model
                    .name
                    .strip_prefix(&format!("{provider_name}:"))
                    .unwrap_or(&model.name);
                let meta = fmt_context_window(model.context_window);
                let name_w = (area.width as usize).saturating_sub(20).max(4);
                let current = if model.is_current {
                    format!(
                        " {} {}",
                        Glyph::Ok.render(state.icons),
                        state.i18n.t("models-current")
                    )
                } else {
                    String::new()
                };
                let mut spans = vec![
                    Span::raw("  "),
                    Span::styled(marker, th.style(name_sem)),
                    Span::styled(
                        truncate_display(id, name_w),
                        th.style(name_sem)
                            .add_modifier(if active_bold || model.is_current {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                    ),
                    Span::raw(" "),
                    Span::styled(meta, th.style(Sem::Muted)),
                    Span::styled(current, th.style(Sem::AccentSoft)),
                ];
                if let Some(badge) = key_badge(state, model.key) {
                    spans.push(Span::raw("  "));
                    spans.push(badge);
                }
                lines.push(Line::from(spans));
            }
        }
    }
    if end < total {
        lines.push(form_scroll_marker(
            state,
            "models-scroll-below",
            total - end,
        ));
    }
    if models.rows.is_empty() {
        lines.push(Line::from(Span::styled(
            empty_models_message(state, models),
            th.style(Sem::Muted),
        )));
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// Right pane: details of the row selected on the left (a model's specs, or the
/// provider's info when its header is selected). The detail block scrolls when it
/// grows past the pane (`[`/`]`); the two-line config-file hint is pinned at the
/// bottom and never scrolls away.
fn render_model_detail(state: &AppState, models: &ModelsOverlay, f: &mut Frame, area: Rect) {
    let th = &state.theme;
    let width = area.width.saturating_sub(1) as usize;
    let mut detail_lines = vec![Line::from(Span::styled(
        state.i18n.t("models-detail-title"),
        th.style(Sem::Muted).add_modifier(Modifier::BOLD),
    ))];
    detail_lines.push(Line::raw(""));

    match models.selected_row() {
        Some(ModelRow::Model { provider, model }) => {
            render_model_detail_lines(state, models, provider, model, width, &mut detail_lines);
        }
        Some(ModelRow::Provider(pi)) => {
            render_provider_detail_lines(state, models, pi, width, &mut detail_lines);
        }
        None => {
            detail_lines.push(Line::from(Span::styled(
                empty_models_message(state, models),
                th.style(Sem::Muted),
            )));
        }
    }

    let rows = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(2),
        Constraint::Length(1),
    ])
    .split(area);

    let viewport = (rows[0].height as usize).max(1);
    let total = detail_lines.len();
    let inner = if total > viewport {
        viewport.saturating_sub(2).max(1)
    } else {
        total
    };
    let start = models.detail_scroll.min(total.saturating_sub(inner));
    let end = (start + inner).min(total);
    let mut shown = Vec::with_capacity(inner.saturating_add(2));
    if start > 0 {
        shown.push(form_scroll_marker(state, "models-scroll-above", start));
    }
    shown.extend(detail_lines[start..end].iter().cloned());
    if end < total {
        shown.push(form_scroll_marker(
            state,
            "models-scroll-below",
            total - end,
        ));
    }
    f.render_widget(Paragraph::new(shown), rows[0]);

    let mut hint_lines = vec![Line::from(Span::styled(
        state.i18n.t("models-config-note"),
        th.style(Sem::Muted),
    ))];
    if let Some(path) = &models.config_path {
        hint_lines.push(Line::from(Span::styled(
            truncate_path_left(path, width.saturating_sub(2).max(8)),
            th.style(Sem::AccentSoft).add_modifier(Modifier::BOLD),
        )));
    }
    f.render_widget(Paragraph::new(hint_lines), rows[1]);
}

fn empty_models_message(state: &AppState, models: &ModelsOverlay) -> String {
    if models.filter.is_empty() {
        state.i18n.t("models-empty-catalog")
    } else {
        let mut args = fluent_bundle::FluentArgs::new();
        args.set("query", models.filter.as_str());
        state.i18n.format("models-filter-empty", Some(&args))
    }
}

fn render_model_detail_lines(
    state: &AppState,
    models: &ModelsOverlay,
    provider_idx: usize,
    model_idx: usize,
    width: usize,
    lines: &mut Vec<Line<'static>>,
) {
    let th = &state.theme;
    let model = &models.models[model_idx];
    let provider = &models.providers[provider_idx];

    lines.push(Line::from(vec![
        Span::styled(
            truncate_display(&model.name, width),
            th.style(Sem::Accent).add_modifier(Modifier::BOLD),
        ),
        if model.is_current {
            Span::styled(
                format!(
                    "  {} {}",
                    Glyph::Ok.render(state.icons),
                    state.i18n.t("models-current")
                ),
                th.style(Sem::AccentSoft),
            )
        } else {
            Span::raw("")
        },
    ]));
    lines.push(Line::raw(""));

    lines.push(model_field_line(
        state,
        "models-field-provider",
        &provider.name,
    ));
    lines.push(model_field_line(state, "models-field-sdk", &provider.sdk));
    if let Some(url) = provider.base_url.as_deref() {
        lines.push(model_field_line(
            state,
            "models-field-base-url",
            &truncate_display(url, width.saturating_sub(11)),
        ));
    }
    lines.push(Line::raw(""));

    lines.push(model_field_line(
        state,
        "models-field-tier",
        model_tier_label(model),
    ));
    lines.push(model_field_line(
        state,
        "models-field-context",
        &fmt_context_window(model.context_window),
    ));
    lines.push(model_field_line(
        state,
        "models-field-vision",
        if model.vision { "✓" } else { "—" },
    ));
    lines.push(model_field_line(state, "models-field-price", &model.price));
    lines.push(Line::raw(""));

    let key = key_status_label(state, model.key);
    if model.key == KeyStatus::Missing {
        lines.push(styled_hint(state, &state.i18n.t("models-key-missing-hint")));
    } else {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{:<10}", state.i18n.t("models-field-key")),
                th.style(Sem::Muted),
            ),
            Span::styled(key, th.style(key_status_color(model.key))),
        ]));
    }
}

fn render_provider_detail_lines(
    state: &AppState,
    models: &ModelsOverlay,
    provider_idx: usize,
    width: usize,
    lines: &mut Vec<Line<'static>>,
) {
    let th = &state.theme;
    let provider = &models.providers[provider_idx];

    lines.push(Line::from(Span::styled(
        truncate_display(&provider.name, width),
        th.style(Sem::Accent).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::raw(""));

    lines.push(model_field_line(state, "models-field-sdk", &provider.sdk));
    if let Some(url) = provider.base_url.as_deref() {
        lines.push(model_field_line(
            state,
            "models-field-base-url",
            &truncate_display(url, width.saturating_sub(11)),
        ));
    }
    lines.push(Line::raw(""));

    let key = key_status_label(state, provider.key);
    if provider.key == KeyStatus::Missing {
        lines.push(styled_hint(state, &state.i18n.t("models-key-missing-hint")));
    } else {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{:<10}", state.i18n.t("models-field-key")),
                th.style(Sem::Muted),
            ),
            Span::styled(key, th.style(key_status_color(provider.key))),
        ]));
    }

    let count = models
        .models
        .iter()
        .filter(|model| model.provider == provider.name)
        .count();
    lines.push(Line::raw(""));
    let mut args = fluent_bundle::FluentArgs::new();
    args.set("count", count as i64);
    lines.push(Line::from(Span::styled(
        state.i18n.format("models-provider-count", Some(&args)),
        th.style(Sem::Muted),
    )));
    lines.push(Line::from(Span::styled(
        state.i18n.t("models-detail-select-model"),
        th.style(Sem::Muted),
    )));
}

fn render_model_test_dialog(state: &AppState, test: &ModelTest, f: &mut Frame, area: Rect) {
    let th = &state.theme;
    let card = centered_rect(area, 76, 7, 0, 0);
    f.render_widget(Clear, card);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(overlay_border_type(state))
        .border_style(Style::default().fg(th.color(Sem::AccentSoft)))
        .padding(Padding::horizontal(2))
        .title(Span::styled(
            format!(" {} ", state.i18n.t("models-test-title")),
            th.style(Sem::Muted).add_modifier(Modifier::BOLD),
        ));
    let inner_w = block.inner(card).width.saturating_sub(1) as usize;
    let mut lines = Vec::new();
    match &test.result {
        None => {
            let label = format!(
                "{} {}",
                spinner(state, state.spinner_frame),
                truncate_display(&test.model, inner_w.saturating_sub(2))
            );
            lines.push(Line::from(Span::styled(
                format!("{} {}", state.i18n.t("models-test-running"), label),
                th.style(Sem::Muted),
            )));
        }
        Some(result) => {
            lines.push(Line::from(Span::styled(
                truncate_display(&test.model, inner_w),
                th.style(Sem::Accent).add_modifier(Modifier::BOLD),
            )));
            let summary = format!(
                "{} · {}ms · {}",
                if result.ok { "ok" } else { "failed" },
                result.latency_ms,
                result.detail
            );
            lines.push(Line::from(Span::styled(
                truncate_display(&summary, inner_w),
                th.style(if result.ok { Sem::Success } else { Sem::Error }),
            )));
        }
    }
    lines.push(Line::raw(""));
    lines.push(styled_hint(state, &state.i18n.t("models-test-hint")));
    f.render_widget(Paragraph::new(lines).block(block), card);
}

fn render_model_form(state: &AppState, models: &ModelsOverlay, f: &mut Frame, area: Rect) {
    let th = &state.theme;
    let card = centered_rect(area, 76, 9, 0, 0);
    f.render_widget(Clear, card);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(overlay_border_type(state))
        .border_style(Style::default().fg(th.color(Sem::Accent)))
        .padding(Padding::horizontal(2))
        .title(Span::styled(
            format!(
                " {} ",
                model_form_title(state, &models.mode, selected_provider_name(models)),
            ),
            th.style(Sem::Muted).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(card);
    f.render_widget(block, card);
    let mut lines = model_form_lines(state, &models.mode);
    if let Some(err) = &models.form_error {
        // Insert the validation error just before the trailing hint line.
        let at = lines.len().saturating_sub(1);
        lines.insert(
            at,
            Line::from(Span::styled(format!("⚠ {err}"), th.style(Sem::Error))),
        );
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn model_form_lines(state: &AppState, mode: &ModelOverlayMode) -> Vec<Line<'static>> {
    match mode {
        ModelOverlayMode::SetKey {
            value,
            storage,
            field,
        } => vec![
            model_input_line(state, "models-field-key", value, *field == 0, true),
            model_input_line(state, "models-field-storage", storage, *field == 1, false),
            Line::raw(""),
            form_hint(state, &state.i18n.t("models-form-hint")),
        ],
        ModelOverlayMode::Browse => Vec::new(),
    }
}

fn model_input_line(
    state: &AppState,
    label_key: &str,
    value: &str,
    active: bool,
    secret: bool,
) -> Line<'static> {
    let th = &state.theme;
    let display = if secret && !value.is_empty() {
        "*".repeat(value.chars().count().max(4))
    } else if value.is_empty() {
        state.i18n.t("form-empty")
    } else {
        value.to_string()
    };
    Line::from(vec![
        Span::styled(
            if active { "\u{203a} " } else { "  " },
            th.style(if active { Sem::Accent } else { Sem::Muted }),
        ),
        Span::styled(
            format!("{:<12}", state.i18n.t(label_key)),
            th.style(Sem::Muted),
        ),
        Span::styled(
            format!("[ {display} ]"),
            Style::default()
                .fg(th.color(if active { Sem::Accent } else { Sem::AccentSoft }))
                .add_modifier(if active {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        ),
    ])
}

fn model_form_title(state: &AppState, mode: &ModelOverlayMode, provider: Option<&str>) -> String {
    let with = |key: &str, subject: Option<&str>| match subject {
        Some(s) if !s.is_empty() => format!("{} · {}", state.i18n.t(key), s),
        _ => state.i18n.t(key),
    };
    match mode {
        ModelOverlayMode::SetKey { .. } => with("models-form-set-key", provider),
        ModelOverlayMode::Browse => String::new(),
    }
}

/// Provider of the row under the cursor (a provider header, or the provider a model
/// row belongs to).
fn selected_provider_name(models: &ModelsOverlay) -> Option<&str> {
    match models.selected_row()? {
        ModelRow::Provider(pi) => models.providers.get(pi).map(|p| p.name.as_str()),
        ModelRow::Model { provider, .. } => models.providers.get(provider).map(|p| p.name.as_str()),
    }
}

fn model_hint_key(_models: &ModelsOverlay) -> &'static str {
    "models-hint"
}

/// Compact context-window label: 131072 → "128K", 400000 → "400K", 0 → "ctx —".
fn fmt_context_window(tokens: u32) -> String {
    if tokens == 0 {
        return "ctx —".to_string();
    }
    if tokens >= 1_000_000 {
        let m = (tokens as f64 / 100_000.0).round() / 10.0; // one decimal, decimal M
        if (m.fract()).abs() < f64::EPSILON {
            format!("{}M", m as u32)
        } else {
            format!("{m}M")
        }
    } else {
        format!("{}K", (tokens as f64 / 1000.0).round() as u32)
    }
}

fn model_tier_label(model: &ModelEntry) -> &'static str {
    match model.tier {
        crate::session::dto::Tier::Light => "light",
        crate::session::dto::Tier::Main => "main",
        crate::session::dto::Tier::Thinking => "thinking",
    }
}

fn key_status_label(state: &AppState, status: KeyStatus) -> String {
    state.i18n.t(match status {
        KeyStatus::Present => "models-key-present",
        KeyStatus::Missing => "models-key-missing",
        KeyStatus::Env => "models-key-env",
    })
}

fn key_badge(state: &AppState, status: KeyStatus) -> Option<Span<'static>> {
    match status {
        KeyStatus::Present | KeyStatus::Env => Some(Span::styled(
            key_status_label(state, status),
            state.theme.style(key_status_color(status)),
        )),
        KeyStatus::Missing => None,
    }
}

fn key_status_color(status: KeyStatus) -> Sem {
    match status {
        KeyStatus::Present | KeyStatus::Env => Sem::Muted,
        KeyStatus::Missing => Sem::Error,
    }
}

fn truncate_path_left(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    let total: usize = text
        .chars()
        .map(|ch| UnicodeWidthChar::width(ch).unwrap_or(0))
        .sum();
    if total <= max_width {
        return text.to_string();
    }
    let mut suffix = String::new();
    let mut width = 0usize;
    for ch in text.chars().rev() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width + 1 > max_width {
            break;
        }
        suffix.insert(0, ch);
        width += ch_width;
    }
    format!("…{suffix}")
}
