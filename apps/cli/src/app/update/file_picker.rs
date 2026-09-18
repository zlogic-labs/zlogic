//! File-completion state, directory navigation, and background glob results.

use std::path::{Component, Path, PathBuf};

use super::{active_token, file_preview, replace_active_token, PICKER_LIMIT};
use crate::app::{AppState, Chip, FileDraft, FilePicker, FilePickerEntry, FileScanRequest};

pub(super) fn refresh(state: &mut AppState) {
    let Some((marker, query)) = active_token(&state.input) else {
        if state.is_file_picker_open() {
            state.clear_overlay();
        }
        return;
    };
    if marker != '@' {
        if state.is_file_picker_open() {
            state.clear_overlay();
        }
        return;
    }
    let query = query.to_string();
    let (cwd, prefix, filter) = picker_parts_from_query(&query);
    let entries = picker_entries(&cwd, &prefix, filter);
    let selected = first_selectable_entry(&entries).unwrap_or(0);
    state.set_file_picker(FilePicker {
        cwd,
        prefix,
        query: filter.to_string(),
        entries,
        selected,
    });
    state.file_scan_generation = state.file_scan_generation.wrapping_add(1);
    state.file_scan_request = should_glob_search(&query).then_some(FileScanRequest {
        generation: state.file_scan_generation,
        query,
    });
}

pub fn take_scan_request(state: &mut AppState) -> Option<FileScanRequest> {
    state.file_scan_request.take()
}

pub(super) fn apply_scan(state: &mut AppState, generation: u64, entries: Vec<FilePickerEntry>) {
    if generation != state.file_scan_generation {
        return;
    }
    let Some(picker) = state.file_picker_mut() else {
        return;
    };
    let existing: std::collections::HashSet<String> = picker
        .entries
        .iter()
        .map(|entry| entry.display.clone())
        .collect();
    picker.entries.extend(
        entries
            .into_iter()
            .filter(|entry| !existing.contains(&entry.display)),
    );
    picker.entries.truncate(PICKER_LIMIT);
    state.dirty = true;
}

/// A directory entered through the picker is navigation state rather than an
/// accepted chip. Treat its complete `@path/` token as one composer block when
/// Backspace is pressed at the end, while keeping partial filename filters
/// editable character by character.
pub(super) fn remove_entered_directory(state: &mut AppState) -> bool {
    let Some(picker) = state.file_picker() else {
        return false;
    };
    if picker.prefix.is_empty() || !picker.query.is_empty() {
        return false;
    }
    let Some((marker, query)) = active_token(&state.input) else {
        return false;
    };
    if marker != '@' || query != picker.prefix || !query.ends_with('/') {
        return false;
    }

    let token_len = marker.len_utf8() + query.len();
    state.input.truncate(state.input.len() - token_len);
    state.input_cursor = state.input.chars().count();
    state.file_scan_generation = state.file_scan_generation.wrapping_add(1);
    state.file_scan_request = None;
    state.clear_overlay();
    true
}

fn picker_parts_from_query(query: &str) -> (PathBuf, String, &str) {
    let (base, filter) = query.rsplit_once('/').unwrap_or(("", query));
    let cwd = if base.is_empty() {
        PathBuf::from(".")
    } else {
        PathBuf::from(base)
    };
    let prefix = if base.is_empty() {
        String::new()
    } else {
        format!("{}/", base.trim_end_matches('/'))
    };
    (cwd, prefix, filter)
}

fn read_picker_entries(cwd: &Path, prefix: &str, filter: &str) -> Vec<FilePickerEntry> {
    let mut items = Vec::new();
    if prefix.is_empty() {
        items.push(FilePickerEntry {
            name: "../".into(),
            path: "..".into(),
            display: "../".into(),
            is_dir: true,
            is_parent: true,
        });
    } else {
        let parent_prefix = parent_prefix(prefix);
        let parent_path = if parent_prefix.is_empty() {
            ".".into()
        } else {
            parent_prefix.trim_end_matches('/').to_string()
        };
        items.push(FilePickerEntry {
            name: "../".into(),
            path: parent_path,
            display: "../".into(),
            is_dir: true,
            is_parent: true,
        });
    }

    let Ok(entries) = std::fs::read_dir(cwd) else {
        return items;
    };
    let needle = filter.to_lowercase();
    let mut children: Vec<_> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || !name.to_lowercase().starts_with(&needle) {
                return None;
            }
            let path = entry.path();
            let is_dir = path.is_dir();
            let display = format!("{prefix}{name}{}", if is_dir { "/" } else { "" });
            Some(FilePickerEntry {
                name,
                path: path.display().to_string(),
                display,
                is_dir,
                is_parent: false,
            })
        })
        .collect();
    children.sort_by_key(|entry| (!entry.is_dir, entry.name.to_lowercase()));
    items.extend(children);
    items
}

fn picker_entries(cwd: &Path, prefix: &str, filter: &str) -> Vec<FilePickerEntry> {
    read_picker_entries(cwd, prefix, filter)
}

fn should_glob_search(query: &str) -> bool {
    !query.is_empty()
        && (query.contains('/')
            || query.contains('*')
            || query.contains('?')
            || query.chars().count() >= 2)
}

pub fn glob_entries(query: &str) -> Vec<FilePickerEntry> {
    let needle = query
        .trim_start_matches("./")
        .replace(['*', '?'], "")
        .to_ascii_lowercase();
    let mut entries = Vec::new();
    let mut pending = vec![PathBuf::from(".")];
    while let Some(dir) = pending.pop() {
        let Ok(children) = std::fs::read_dir(&dir) else {
            continue;
        };
        for child in children.filter_map(Result::ok) {
            let path = child.path();
            if has_ignored_component(&path) {
                continue;
            }
            let is_dir = child.file_type().is_ok_and(|kind| kind.is_dir());
            if is_dir {
                pending.push(path.clone());
            }
            let display = path
                .strip_prefix(".")
                .unwrap_or(&path)
                .display()
                .to_string()
                .trim_start_matches('/')
                .to_string();
            if !needle.is_empty() && !display.to_ascii_lowercase().contains(&needle) {
                continue;
            }
            if let Some(entry) = glob_entry_from_path(&path) {
                entries.push(entry);
            }
            if entries.len() >= PICKER_LIMIT {
                break;
            }
        }
        if entries.len() >= PICKER_LIMIT {
            break;
        }
    }
    entries.sort_by_key(|entry| (!entry.is_dir, entry.display.len(), entry.display.clone()));
    entries
}

fn glob_entry_from_path(path: &Path) -> Option<FilePickerEntry> {
    let display = path.strip_prefix(".").unwrap_or(path).display().to_string();
    let display = display.trim_start_matches('/').to_string();
    if display.is_empty() {
        return None;
    }
    let is_dir = path.is_dir();
    let name = path.file_name()?.to_string_lossy().to_string();
    Some(FilePickerEntry {
        name,
        path: path.display().to_string(),
        display: format!("{display}{}", if is_dir { "/" } else { "" }),
        is_dir,
        is_parent: false,
    })
}

fn has_ignored_component(path: &Path) -> bool {
    path.components().any(|component| {
        let Component::Normal(name) = component else {
            return false;
        };
        let name = name.to_string_lossy();
        name.starts_with('.')
            || matches!(
                name.as_ref(),
                "target" | "node_modules" | "dist" | "build" | ".git" | ".next"
            )
    })
}

fn parent_prefix(prefix: &str) -> String {
    let trimmed = prefix.trim_end_matches('/');
    if trimmed.is_empty() {
        return "../".into();
    }
    if trimmed == ".." || trimmed.ends_with("/..") {
        return format!("{prefix}../");
    }
    trimmed
        .rsplit_once('/')
        .map(|(parent, _)| format!("{}/", parent))
        .unwrap_or_default()
}

pub(super) fn move_selection(state: &mut AppState, delta: isize) {
    if let Some(picker) = state.file_picker_mut() {
        let len = picker.entries.len();
        if len == 0 {
            return;
        }
        picker.selected = if delta < 0 {
            picker.selected.saturating_sub(1)
        } else {
            (picker.selected + 1).min(len - 1)
        };
        state.dirty = true;
    }
}

pub(super) fn first_selectable_entry(entries: &[FilePickerEntry]) -> Option<usize> {
    entries.iter().position(|entry| !entry.is_parent)
}

fn selected_entry(state: &AppState) -> Option<FilePickerEntry> {
    let picker = state.file_picker()?;
    picker.entries.get(picker.selected).cloned()
}

pub(super) fn enter(state: &mut AppState) -> bool {
    let Some(entry) = selected_entry(state) else {
        return false;
    };
    if entry.is_parent
        && state
            .file_picker()
            .is_some_and(|picker| !picker.query.is_empty() && picker.entries.len() == 1)
    {
        return false;
    }
    if entry.is_dir {
        enter_dir(state, &entry);
    } else {
        accept_entry(state, &entry);
    }
    state.dirty = true;
    true
}

pub(super) fn accept(state: &mut AppState) {
    let Some(entry) = selected_entry(state) else {
        return;
    };
    if entry.is_dir {
        enter_dir(state, &entry);
    } else {
        accept_entry(state, &entry);
    }
    state.dirty = true;
}

fn enter_dir(state: &mut AppState, entry: &FilePickerEntry) {
    let next_prefix = if entry.is_parent {
        state
            .file_picker()
            .map(|picker| parent_prefix(&picker.prefix))
            .unwrap_or_default()
    } else {
        entry.display.clone()
    };
    replace_active_token(state, &format!("@{next_prefix}"));
    let cwd = if next_prefix.is_empty() {
        PathBuf::from(".")
    } else {
        PathBuf::from(next_prefix.trim_end_matches('/'))
    };
    let entries = picker_entries(&cwd, &next_prefix, "");
    let selected = first_selectable_entry(&entries).unwrap_or(0);
    state.set_file_picker(FilePicker {
        cwd,
        prefix: next_prefix,
        query: String::new(),
        entries,
        selected,
    });
}

pub(super) fn accept_entry(state: &mut AppState, entry: &FilePickerEntry) {
    if entry.is_parent {
        return;
    }
    let token = format!("@{}", middle_ellipsis(&entry.display, 64));
    replace_active_token(state, &format!("{token} "));
    let draft = FileDraft {
        token,
        path: entry.path.clone(),
        display: entry.display.clone(),
        is_dir: entry.is_dir,
    };
    let preview = (!draft.is_dir).then(|| file_preview(&draft.path)).flatten();
    state.chips.push(Chip::file(draft, preview.as_deref()));
    state.history_replay = None;
    state.clear_overlay();
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
