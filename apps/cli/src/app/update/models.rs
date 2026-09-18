use super::*;
use crate::app::{
    model_rows, sort_providers_by_key, ConfirmAction, ConfirmDialog, FullscreenOverlay,
    ModelOverlayMode, ModelRow, ModelTest, ModelsOverlay, OverlayState,
};

pub(super) fn handle_models_key(state: &mut AppState, k: KeyEvent, session: &dyn CoreSession) {
    let Some(OverlayState::Fullscreen(FullscreenOverlay::Models(mut overlay))) =
        state.overlay.take()
    else {
        return;
    };

    let mut close = false;
    if overlay.test.is_some() {
        handle_model_test_key(state, &mut overlay, k);
    } else if let Some(confirm) = overlay.confirm.take() {
        handle_model_confirm_key(&mut overlay, confirm, k);
    } else {
        match overlay.mode {
            ModelOverlayMode::Browse => {
                close = handle_model_browse_key(state, session, &mut overlay, k);
            }
            ModelOverlayMode::SetKey { .. } => {
                close = handle_model_form_key(state, session, &mut overlay, k);
            }
        }
    }

    if close {
        state.clear_overlay();
    } else {
        state.overlay = Some(OverlayState::Fullscreen(FullscreenOverlay::Models(overlay)));
    }
    state.dirty = true;
}

fn handle_model_browse_key(
    state: &mut AppState,
    session: &dyn CoreSession,
    overlay: &mut ModelsOverlay,
    k: KeyEvent,
) -> bool {
    match k.code {
        KeyCode::Esc | KeyCode::Char('q') => return true,
        // One flat cursor: ↑/↓ move over provider headers and their model rows.
        KeyCode::Up => move_selection(overlay, false),
        KeyCode::Down => move_selection(overlay, true),
        KeyCode::PageUp => (0..5).for_each(|_| move_selection(overlay, false)),
        KeyCode::PageDown => (0..5).for_each(|_| move_selection(overlay, true)),
        KeyCode::Home => overlay.selected = 0,
        KeyCode::End => overlay.selected = overlay.rows.len().saturating_sub(1),
        KeyCode::Char('[') => overlay.detail_scroll = overlay.detail_scroll.saturating_sub(3),
        KeyCode::Char(']') => overlay.detail_scroll = overlay.detail_scroll.saturating_add(3),
        // Enter on a model = switch to it (its provider has no key → prompt to bind
        // one first, then finish the switch once saved). Enter on a provider header =
        // bind/update its API key (the main thing you do to a provider — built-ins
        // ship without keys; provider/model structure itself lives in models.yaml).
        KeyCode::Enter => match overlay.selected_row() {
            Some(ModelRow::Model { provider, model }) => {
                if overlay.models[model].key == KeyStatus::Missing {
                    open_bind_key_prompt(state, overlay, provider, model);
                } else {
                    switch_to_model(state, session, &overlay.models[model].name);
                    return true;
                }
            }
            Some(ModelRow::Provider(_)) => {
                overlay.mode = ModelOverlayMode::SetKey {
                    value: String::new(),
                    storage: "keychain".into(),
                    field: 0,
                };
            }
            None => {}
        },
        // `t` tests connectivity: on a model row → that model; on a provider header →
        // its first model (the key belongs to the provider, so the result is the same
        // for every model under it). The request runs on a background thread and the
        // result shows in a modal; `k` binds the selected provider's API key;
        // `r` re-reads models.yaml from disk (edit the file, press r — no need to
        // reopen the panel).
        KeyCode::Char('t') => {
            if let Some(model_idx) = test_model_idx(overlay) {
                let model = overlay.models[model_idx].name.clone();
                overlay.test = Some(ModelTest {
                    model: model.clone(),
                    result: None,
                });
                state.test_request = Some(model);
            }
        }
        KeyCode::Char('k') if selected_provider_idx(overlay).is_some() => {
            overlay.mode = ModelOverlayMode::SetKey {
                value: String::new(),
                storage: "keychain".into(),
                field: 0,
            };
        }
        KeyCode::Char('r') => {
            refresh_models_overlay(session, overlay);
            state.set_status(state.i18n.t("models-status-reloaded"));
        }
        _ => {}
    }
    false
}

fn handle_model_test_key(state: &mut AppState, overlay: &mut ModelsOverlay, k: KeyEvent) {
    match k.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => overlay.test = None,
        KeyCode::Char('t') => {
            let Some(model) = overlay.test.as_ref().map(|test| test.model.clone()) else {
                return;
            };
            overlay.test = Some(ModelTest {
                model: model.clone(),
                result: None,
            });
            state.test_request = Some(model);
        }
        _ => {}
    }
}

fn handle_model_confirm_key(overlay: &mut ModelsOverlay, mut confirm: ConfirmDialog, k: KeyEvent) {
    match k.code {
        KeyCode::Esc => {}
        KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
            confirm.selected = if confirm.selected == 0 { 1 } else { 0 };
            overlay.confirm = Some(confirm);
        }
        KeyCode::Enter if confirm.selected == 1 => match confirm.action {
            ConfirmAction::BindProviderKey { model, .. } => {
                // Hand off to the key form; once it saves, switch to `model` and
                // close the panel.
                overlay.pending_switch = Some(model);
                overlay.mode = ModelOverlayMode::SetKey {
                    value: String::new(),
                    storage: "keychain".into(),
                    field: 0,
                };
            }
            _ => {}
        },
        KeyCode::Enter => {}
        _ => overlay.confirm = Some(confirm),
    }
}

/// The SetKey form. Returns `true` when it finished by switching to a pending model
/// (missing-key prompt flow) — the caller then closes the whole overlay.
fn handle_model_form_key(
    state: &mut AppState,
    session: &dyn CoreSession,
    overlay: &mut ModelsOverlay,
    k: KeyEvent,
) -> bool {
    overlay.form_error = None;
    let submitted = match &mut overlay.mode {
        ModelOverlayMode::SetKey {
            value,
            storage,
            field,
        } => {
            if matches!(k.code, KeyCode::Esc) {
                overlay.mode = ModelOverlayMode::Browse;
                overlay.pending_switch = None; // aborted the bind-then-switch flow
                return false;
            }
            handle_model_text_fields(k, [value, storage], field)
        }
        ModelOverlayMode::Browse => return false,
    };
    if !submitted {
        return false;
    }
    let Some(provider_name) = selected_provider(overlay).map(|provider| provider.name.clone())
    else {
        return false;
    };
    let pending = overlay.pending_switch.take();
    let (value, storage) = match &overlay.mode {
        ModelOverlayMode::SetKey { value, storage, .. } => (value.clone(), storage.clone()),
        ModelOverlayMode::Browse => return false,
    };
    if session.set_api_key(&provider_name, &value, &storage) {
        overlay.mode = ModelOverlayMode::Browse;
        refresh_models_overlay(session, overlay);
        if let Some(model) = pending {
            // Opened from the missing-key prompt: finish the switch now.
            switch_to_model(state, session, &model);
            return true;
        }
        state.set_status(state.i18n.t("models-status-key-saved"));
    } else {
        overlay.form_error = Some(state.i18n.t("models-error-key-save"));
        overlay.pending_switch = pending;
    }
    false
}

fn handle_model_text_fields<const N: usize>(
    k: KeyEvent,
    mut fields: [&mut String; N],
    field: &mut usize,
) -> bool {
    match k.code {
        KeyCode::Enter => {
            if *field + 1 >= N {
                true
            } else {
                *field += 1;
                false
            }
        }
        KeyCode::Tab | KeyCode::Down => {
            *field = (*field + 1).min(N.saturating_sub(1));
            false
        }
        KeyCode::BackTab | KeyCode::Up => {
            *field = (*field).saturating_sub(1);
            false
        }
        _ => {
            if let Some(active) = fields.get_mut(*field) {
                edit_text(active, k);
            }
            false
        }
    }
}

fn edit_text(value: &mut String, k: KeyEvent) {
    match k.code {
        KeyCode::Char(ch) if !k.modifiers.contains(KeyModifiers::CONTROL) => value.push(ch),
        KeyCode::Backspace => {
            value.pop();
        }
        _ => {}
    }
}

/// Which row the cursor sits on, captured by name — a reload (external edit of
/// `models.yaml`, or a key write) re-anchors the cursor to the same model/provider
/// instead of jumping back to the top.
enum Anchor {
    Provider(String),
    Model(String),
}

fn refresh_models_overlay(session: &dyn CoreSession, overlay: &mut ModelsOverlay) {
    let anchor = match overlay.selected_row() {
        Some(ModelRow::Provider(pi)) => overlay
            .providers
            .get(pi)
            .map(|provider| Anchor::Provider(provider.name.clone())),
        Some(ModelRow::Model { model, .. }) => overlay
            .models
            .get(model)
            .map(|model| Anchor::Model(model.name.clone())),
        None => None,
    };
    overlay.providers = session.provider_catalog();
    overlay.models = session.model_catalog();
    if !overlay.filter.is_empty() {
        let (providers, models) =
            crate::app::filter_catalog(&overlay.providers, &overlay.models, &overlay.filter);
        overlay.providers = providers;
        overlay.models = models;
    }
    sort_providers_by_key(&mut overlay.providers);
    overlay.keys = session.key_catalog();
    overlay.rows = model_rows(&overlay.providers, &overlay.models);
    overlay.selected = match anchor {
        Some(Anchor::Provider(name)) => overlay.rows.iter().position(|row| {
            matches!(row, ModelRow::Provider(pi) if overlay.providers[*pi].name == name)
        }),
        Some(Anchor::Model(name)) => overlay.rows.iter().position(|row| {
            matches!(row, ModelRow::Model { model, .. } if overlay.models[*model].name == name)
        }),
        None => None,
    }
    .unwrap_or(0);
    overlay.detail_scroll = 0;
}

fn move_selection(overlay: &mut ModelsOverlay, down: bool) {
    if overlay.rows.is_empty() {
        overlay.selected = 0;
        return;
    }
    overlay.selected = if down {
        (overlay.selected + 1).min(overlay.rows.len() - 1)
    } else {
        overlay.selected.saturating_sub(1)
    };
    overlay.detail_scroll = 0;
}

/// Missing-key prompt: "this model's provider has no API key — bind one to switch".
fn open_bind_key_prompt(
    state: &AppState,
    overlay: &mut ModelsOverlay,
    provider_idx: usize,
    model_idx: usize,
) {
    let provider = overlay.providers[provider_idx].name.clone();
    let model = overlay.models[model_idx].name.clone();
    let mut args = fluent_bundle::FluentArgs::new();
    args.set("provider", provider.as_str());
    args.set("model", model.as_str());
    overlay.confirm = Some(ConfirmDialog {
        title: state.i18n.t("models-prompt-key-title"),
        message: state.i18n.format("models-prompt-key-message", Some(&args)),
        confirm_label: state.i18n.t("models-prompt-key-bind"),
        cancel_label: state.i18n.t("form-option-cancel"),
        danger: false,
        selected: 0,
        action: ConfirmAction::BindProviderKey { provider, model },
    });
}

fn switch_to_model(state: &mut AppState, session: &dyn CoreSession, model: &str) {
    session.set_session_model(model);
    state.refresh_status_snapshot(session);
    state.set_status(format!("model {model}"));
}

fn selected_provider_idx(overlay: &ModelsOverlay) -> Option<usize> {
    match overlay.selected_row()? {
        ModelRow::Provider(pi) => Some(pi),
        ModelRow::Model { provider, .. } => Some(provider),
    }
}

fn selected_provider(overlay: &ModelsOverlay) -> Option<&ProviderEntry> {
    overlay.providers.get(selected_provider_idx(overlay)?)
}

fn test_model_idx(overlay: &ModelsOverlay) -> Option<usize> {
    match overlay.selected_row()? {
        ModelRow::Model { model, .. } => Some(model),
        ModelRow::Provider(pi) => overlay
            .models
            .iter()
            .position(|model| model.provider == overlay.providers[pi].name),
    }
}
