//! Conversion from segmented composer state to structured core messages.

use crate::app::{Chip, ChipKind, FileDraft, PasteDraft};
use crate::session::dto::{CommandKind, Message, Part};
use crate::session::CoreSession;

pub(super) fn messages_for_submit(
    text: &str,
    chips: &[Chip],
    session: &dyn CoreSession,
) -> Vec<Message> {
    let mut parts = Vec::new();
    let mut segments = composer_segments(text, chips, session);
    for segment in segments.drain(..) {
        match segment {
            ComposerSegment::Text(text) => push_text_part(&mut parts, &text),
            ComposerSegment::Paste(p) => parts.push(Part::Text {
                text: p.text.clone(),
            }),
            ComposerSegment::File(f) => push_file_part(&mut parts, f),
            ComposerSegment::Command { name, text } => parts.push(Part::Command {
                command: "skill".into(),
                name,
                text,
            }),
        }
    }
    (!parts.is_empty())
        .then(|| Message::from_parts(parts))
        .into_iter()
        .collect()
}

pub(super) fn validate_submit_messages(
    messages: Vec<Message>,
    session: &dyn CoreSession,
) -> Vec<Message> {
    let catalog = session.command_catalog();
    let skills = catalog
        .iter()
        .filter(|command| command.kind == CommandKind::Skill)
        .map(|command| command.name.as_str())
        .collect::<std::collections::HashSet<_>>();
    let builtins = catalog
        .iter()
        .filter(|command| command.kind == CommandKind::Builtin)
        .map(|command| command.name.as_str())
        .collect::<std::collections::HashSet<_>>();
    messages
        .into_iter()
        .map(|message| {
            let parts = message
                .parts
                .into_iter()
                .map(|part| match part {
                    Part::File {
                        path,
                        display,
                        preview,
                    } if std::fs::metadata(&path).is_ok_and(|metadata| metadata.is_file()) => {
                        Part::File {
                            path,
                            display,
                            preview,
                        }
                    }
                    Part::Directory { path, display }
                        if std::fs::metadata(&path).is_ok_and(|metadata| metadata.is_dir()) =>
                    {
                        Part::Directory { path, display }
                    }
                    Part::File { display, .. } | Part::Directory { display, .. } => Part::Text {
                        text: format!("@{}", display.trim_start_matches('@')),
                    },
                    Part::Command {
                        command,
                        name,
                        text,
                    } if command == "skill" && skills.contains(name.as_str()) => Part::Command {
                        command,
                        name,
                        text,
                    },
                    Part::Command {
                        command,
                        name,
                        text,
                    } if command == "builtin" && builtins.contains(name.as_str()) => {
                        Part::Command {
                            command,
                            name,
                            text,
                        }
                    }
                    Part::Command { name, text, .. } => Part::Text {
                        text: if text.is_empty() {
                            format!("/{}", name.trim_start_matches('/'))
                        } else {
                            format!("/{} {text}", name.trim_start_matches('/'))
                        },
                    },
                    part => part,
                })
                .collect();
            Message::from_parts(parts)
        })
        .collect()
}

enum ComposerSegment<'a> {
    Text(String),
    Paste(&'a PasteDraft),
    File(&'a FileDraft),
    Command { name: String, text: String },
}

fn composer_segments<'a>(
    text: &str,
    chips: &'a [Chip],
    session: &dyn CoreSession,
) -> Vec<ComposerSegment<'a>> {
    if let Some((name, rest)) = skill_command(text, session) {
        return vec![ComposerSegment::Command { name, text: rest }];
    }

    let mut out = Vec::new();
    let mut idx = 0usize;
    while idx < text.len() {
        let rest = &text[idx..];
        if let Some(p) = chips.iter().find_map(|chip| match &chip.kind {
            ChipKind::Paste(paste) if rest.starts_with(&paste.display) => Some(paste),
            _ => None,
        }) {
            out.push(ComposerSegment::Paste(p));
            idx += p.display.len();
            continue;
        }
        if let Some(f) = chips.iter().find_map(|chip| match &chip.kind {
            ChipKind::File(file) if rest.starts_with(&file.token) => Some(file),
            _ => None,
        }) {
            out.push(ComposerSegment::File(f));
            idx += f.token.len();
            continue;
        }
        let next = next_special_offset(rest, chips).unwrap_or(rest.len());
        out.push(ComposerSegment::Text(rest[..next].to_string()));
        idx += next;
    }
    out
}

fn skill_command(text: &str, session: &dyn CoreSession) -> Option<(String, String)> {
    let name = text.strip_prefix('/')?.split_whitespace().next()?;
    let cmd = session
        .command_catalog()
        .into_iter()
        .find(|cmd| cmd.name == name && cmd.kind == CommandKind::Skill)?;
    let rest = text
        .strip_prefix(&format!("/{}", cmd.name))
        .unwrap_or_default()
        .trim()
        .to_string();
    Some((cmd.name, rest))
}

fn next_special_offset(text: &str, chips: &[Chip]) -> Option<usize> {
    chips
        .iter()
        .filter_map(|chip| {
            let token = match &chip.kind {
                ChipKind::Paste(paste) => &paste.display,
                ChipKind::File(file) => &file.token,
            };
            text.find(token)
        })
        .min()
}

fn push_text_part(parts: &mut Vec<Part>, text: &str) {
    // Spaces around attachment chips are presentation separators, but explicit
    // user newlines carry meaning and must survive into the structured message.
    let text = text.trim_matches(|ch| ch == ' ' || ch == '\t');
    if text.chars().any(|ch| !ch.is_whitespace()) {
        parts.push(Part::Text {
            text: text.to_string(),
        });
    }
}

fn push_file_part(parts: &mut Vec<Part>, file: &FileDraft) {
    let Ok(metadata) = std::fs::metadata(&file.path) else {
        // The picker selection may become stale before Enter is submitted. A stale
        // chip is ordinary visible text, never a structured attachment.
        push_text_part(parts, &file.token);
        return;
    };
    if metadata.is_dir() {
        parts.push(Part::Directory {
            path: file.path.clone(),
            display: file.display.clone(),
        });
    } else if metadata.is_file() {
        parts.push(Part::File {
            path: file.path.clone(),
            display: file.display.clone(),
            preview: None,
        });
    } else {
        push_text_part(parts, &file.token);
    }
}

pub(super) fn file_preview(path: &str) -> Option<String> {
    let Ok(meta) = std::fs::metadata(path) else {
        return None;
    };
    if !meta.is_file() || meta.len() > 64 * 1024 {
        return None;
    }
    std::fs::read_to_string(path)
        .ok()
        .map(|text| text.chars().take(500).collect())
}
