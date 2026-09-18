use std::io::Write;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};

use crate::i18n::I18n;

pub(crate) fn is_interactive() -> bool {
    use crossterm::tty::IsTty;
    std::io::stdin().is_tty() && std::io::stdout().is_tty()
}

pub fn pick_provider(providers: &[String], i18n: &I18n) -> Option<String> {
    if providers.is_empty() {
        return None;
    }
    if providers.len() == 1 {
        return Some(providers[0].clone());
    }
    let _guard = RawGuard::enter()?;
    let mut out = std::io::stdout();
    let _ = out.write_all(b"\x1b[?25l"); // hide cursor
    let mut selected = 0usize;
    let rows = providers.len() + 2;
    render(&mut out, providers, selected, i18n);
    let result = loop {
        match read_key() {
            Key::Up => {
                if selected > 0 {
                    selected -= 1;
                }
                redraw(&mut out, providers, selected, rows, i18n);
            }
            Key::Down => {
                if selected + 1 < providers.len() {
                    selected += 1;
                }
                redraw(&mut out, providers, selected, rows, i18n);
            }
            Key::Enter => break Some(providers[selected].clone()),
            Key::Cancel => break None,
            _ => {}
        }
    };
    let _ = write!(out, "\x1b[{}A\r\x1b[J", rows - 1);
    let _ = out.write_all(b"\x1b[?25h");
    let _ = out.flush();
    result
}

pub fn prompt_secret(prompt: &str, i18n: &I18n) -> Result<String, String> {
    let _guard = RawGuard::enter().ok_or_else(|| i18n.t("key-pick-notty").to_string())?;
    let mut out = std::io::stdout();
    let _ = out.write_all(b"\x1b[?25h");
    let mut input = String::new();
    loop {
        let _ = write!(out, "\r\x1b[2K{prompt} ");
        for _ in input.chars() {
            let _ = out.write_all(b"*");
        }
        let _ = out.flush();
        match read_key() {
            Key::Char(c) => input.push(c),
            Key::Backspace => {
                input.pop();
            }
            Key::Enter => break,
            Key::Cancel => {
                let _ = write!(out, "\r\x1b[2K");
                let _ = out.flush();
                return Err(i18n.t("key-cancelled").to_string());
            }
            _ => {}
        }
    }
    let _ = write!(out, "\r\x1b[2K");
    let _ = out.flush();
    if input.is_empty() {
        return Err(i18n.t("key-empty").to_string());
    }
    Ok(input)
}

fn render(out: &mut impl Write, providers: &[String], selected: usize, i18n: &I18n) {
    let _ = write!(out, "\r\x1b[2K{}\r\n", i18n.t("key-pick-title"));
    for (i, p) in providers.iter().enumerate() {
        let marker = if i == selected { "▸" } else { " " };
        let _ = write!(out, "\x1b[2K{marker} {p}\r\n");
    }
    let _ = write!(out, "\x1b[2K{}\r\n", i18n.t("key-pick-hint"));
    let _ = out.flush();
}

fn redraw(out: &mut impl Write, providers: &[String], selected: usize, rows: usize, i18n: &I18n) {
    let _ = write!(out, "\x1b[{}A\x1b[J", rows);
    render(out, providers, selected, i18n);
}

pub(crate) enum Key {
    Up,
    Down,
    Enter,
    Backspace,
    Char(char),
    Cancel,
    Other,
}

pub(crate) fn read_key() -> Key {
    match event::read() {
        Ok(Event::Key(KeyEvent {
            code, modifiers, ..
        })) => {
            if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
                return Key::Cancel;
            }
            match code {
                KeyCode::Up => Key::Up,
                KeyCode::Down => Key::Down,
                KeyCode::Enter => Key::Enter,
                KeyCode::Backspace => Key::Backspace,
                KeyCode::Esc => Key::Cancel,
                KeyCode::Char(c) => Key::Char(c),
                _ => Key::Other,
            }
        }
        _ => Key::Other,
    }
}

pub(crate) struct RawGuard {
    armed: bool,
}

impl RawGuard {
    pub(crate) fn enter() -> Option<Self> {
        if !is_interactive() {
            return None;
        }
        let was_raw = crossterm::terminal::is_raw_mode_enabled().unwrap_or(false);
        if !was_raw && crossterm::terminal::enable_raw_mode().is_err() {
            return None;
        }
        Some(RawGuard { armed: !was_raw })
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = crossterm::terminal::disable_raw_mode();
        }
        let _ = std::io::stdout().flush();
    }
}
