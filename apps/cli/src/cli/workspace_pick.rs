use std::io::Write;
use std::path::Path;

use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

use super::pick::{is_interactive, read_key, Key, RawGuard};
use crate::i18n::I18n;
use crate::session::WorkspaceSummary;

const MAX_ROW_WIDTH: usize = 72;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    CreateHere,
    PickExisting { workspace_id: String },
}

pub fn choose(cwd: &Path, workspaces: &[WorkspaceSummary], i18n: &I18n) -> Option<Choice> {
    if !is_interactive() {
        return None;
    }
    let _guard = RawGuard::enter()?;
    let mut out = std::io::stdout();
    let _ = out.write_all(b"\x1b[?25l"); // hide cursor

    let title = {
        let mut t = i18n.t("workspace-pick-title");
        t.push('\n');
        t.push_str(&cwd.display().to_string());
        t
    };

    loop {
        let mut options: Vec<String> = vec![i18n.t("workspace-pick-create")];
        if !workspaces.is_empty() {
            options.push(i18n.t("workspace-pick-select"));
        }
        let labels: Vec<&str> = options.iter().map(String::as_str).collect();
        let block_rows = labels.len() + 2;
        let picks = pick_option(&mut out, &title, &labels, i18n);
        erase_block(&mut out, block_rows);
        match picks {
            None => {
                let _ = out.write_all(b"\x1b[?25h");
                return None;
            }
            Some(0) => {
                let _ = out.write_all(b"\x1b[?25h");
                return Some(Choice::CreateHere);
            }
            Some(_) => match pick_workspace(&mut out, workspaces, i18n) {
                Some(workspace_id) => {
                    let _ = out.write_all(b"\x1b[?25h");
                    return Some(Choice::PickExisting { workspace_id });
                }
                None => continue, // list Esc → back to the main two-way choice (the list has already erased itself)
            },
        }
    }
}

fn pick_option(out: &mut impl Write, title: &str, options: &[&str], i18n: &I18n) -> Option<usize> {
    let mut selected = 0usize;
    let rows = options.len() + 2;
    render_options(out, title, options, selected, i18n);
    loop {
        match read_key() {
            Key::Up if selected > 0 => {
                selected -= 1;
                redraw_options(out, title, options, selected, rows, i18n);
            }
            Key::Down if selected + 1 < options.len() => {
                selected += 1;
                redraw_options(out, title, options, selected, rows, i18n);
            }
            Key::Enter => break Some(selected),
            Key::Cancel => break None,
            _ => {}
        }
    }
}

fn render_options(
    out: &mut impl Write,
    title: &str,
    options: &[&str],
    selected: usize,
    i18n: &I18n,
) {
    let _ = write!(out, "\r\x1b[2K{title}\r\n");
    for (i, label) in options.iter().enumerate() {
        let marker = if i == selected { "▸" } else { " " };
        let _ = write!(out, "\x1b[2K{marker} {label}\r\n");
    }
    let _ = write!(out, "\x1b[2K{}\r\n", i18n.t("workspace-pick-hint"));
    let _ = out.flush();
}

fn redraw_options(
    out: &mut impl Write,
    title: &str,
    options: &[&str],
    selected: usize,
    rows: usize,
    i18n: &I18n,
) {
    let _ = write!(out, "\x1b[{}A\x1b[J", rows);
    render_options(out, title, options, selected, i18n);
}

fn pick_workspace(
    out: &mut impl Write,
    workspaces: &[WorkspaceSummary],
    i18n: &I18n,
) -> Option<String> {
    let items: Vec<&WorkspaceSummary> = workspaces.iter().filter(|w| w.exists).collect();
    if items.is_empty() {
        let _ = write!(out, "\r\x1b[2K{}\r\n", i18n.t("workspace-pick-empty"));
        let _ = out.flush();
        let _ = read_key(); // let the user read this notice; any key returns to the main two-way choice
        erase_block(out, 1);
        return None;
    }
    let mut selected = 0usize;
    let block_rows = items.len() * 2 + 2; // two rows per workspace: the name row + the root row
    render_workspaces(out, &items, selected, i18n);
    loop {
        match read_key() {
            Key::Up if selected > 0 => {
                selected -= 1;
                redraw_workspaces(out, &items, selected, block_rows, i18n);
            }
            Key::Down if selected + 1 < items.len() => {
                selected += 1;
                redraw_workspaces(out, &items, selected, block_rows, i18n);
            }
            Key::Enter => {
                let id = items[selected].workspace_id.to_string();
                erase_block(out, block_rows);
                return Some(id);
            }
            Key::Cancel => {
                erase_block(out, block_rows);
                return None;
            }
            _ => {}
        }
    }
}

fn render_workspaces(
    out: &mut impl Write,
    items: &[&WorkspaceSummary],
    selected: usize,
    i18n: &I18n,
) {
    let _ = write!(out, "\r\x1b[2K{}\r\n", i18n.t("workspace-pick-list-title"));
    for (i, ws) in items.iter().enumerate() {
        let marker = if i == selected { "▸" } else { " " };
        let count = i18n.count("sessions-count", ws.session_count as usize);
        let name = truncate(&ws.name, 40);
        let root = truncate(&ws.root, MAX_ROW_WIDTH.saturating_sub(8));
        let _ = write!(out, "\x1b[2K{marker} {name} · {count}\r\n");
        let _ = write!(out, "\x1b[2K   {root}\r\n");
    }
    let _ = write!(out, "\x1b[2K{}\r\n", i18n.t("workspace-pick-list-hint"));
    let _ = out.flush();
}

fn redraw_workspaces(
    out: &mut impl Write,
    items: &[&WorkspaceSummary],
    selected: usize,
    block_rows: usize,
    i18n: &I18n,
) {
    let _ = write!(out, "\x1b[{}A\x1b[J", block_rows);
    render_workspaces(out, items, selected, i18n);
}

fn erase_block(out: &mut impl Write, up_lines: usize) {
    let _ = write!(out, "\x1b[{up_lines}A\r\x1b[J");
    let _ = out.flush();
}

fn truncate(text: &str, max: usize) -> String {
    if UnicodeWidthStr::width(text) <= max {
        return text.to_string();
    }
    let mut out = String::new();
    let mut w = 0usize;
    for ch in text.chars() {
        let cw = ch.width().unwrap_or(0);
        if w + cw + 1 > max {
            break;
        }
        out.push(ch);
        w += cw;
    }
    format!("{out}…")
}
