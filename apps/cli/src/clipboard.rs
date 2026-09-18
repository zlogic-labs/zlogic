//! Cross-platform plain-text clipboard writer for fullscreen viewers.

#[cfg(not(test))]
use std::io::Write;
#[cfg(not(test))]
use std::process::{Command, Stdio};

#[cfg(not(test))]
struct ClipboardCommand {
    program: &'static str,
    args: &'static [&'static str],
}

#[cfg(not(test))]
fn commands() -> Vec<ClipboardCommand> {
    if cfg!(target_os = "macos") {
        return vec![ClipboardCommand {
            program: "pbcopy",
            args: &[],
        }];
    }
    if cfg!(target_os = "windows") {
        const SCRIPT: &str = "$enc = New-Object System.Text.UTF8Encoding $false; \
            [Console]::InputEncoding = $enc; $text = [Console]::In.ReadToEnd(); \
            Set-Clipboard -Value $text";
        return vec![
            ClipboardCommand {
                program: "powershell.exe",
                args: &["-NoProfile", "-NonInteractive", "-Command", SCRIPT],
            },
            ClipboardCommand {
                program: "pwsh.exe",
                args: &["-NoProfile", "-NonInteractive", "-Command", SCRIPT],
            },
            ClipboardCommand {
                program: "clip.exe",
                args: &[],
            },
        ];
    }

    let mut commands = Vec::new();
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        commands.push(ClipboardCommand {
            program: "wl-copy",
            args: &[],
        });
    }
    commands.extend([
        ClipboardCommand {
            program: "xclip",
            args: &["-selection", "clipboard"],
        },
        ClipboardCommand {
            program: "xsel",
            args: &["--clipboard", "--input"],
        },
    ]);
    commands
}

#[cfg(not(test))]
fn pipe_text(command: &ClipboardCommand, text: &str) -> bool {
    let Ok(mut child) = Command::new(command.program)
        .args(command.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let Some(mut stdin) = child.stdin.take() else {
        return false;
    };
    if stdin.write_all(text.as_bytes()).is_err() {
        return false;
    }
    drop(stdin);
    child.wait().is_ok_and(|status| status.success())
}

#[cfg(not(test))]
pub fn write_text(text: &str) -> bool {
    commands().iter().any(|command| pipe_text(command, text))
}

// Unit tests exercise the zoom interaction without mutating the developer's real
// clipboard. Production builds use the platform writer above.
#[cfg(test)]
pub fn write_text(_text: &str) -> bool {
    true
}
