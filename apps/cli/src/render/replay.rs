use std::collections::HashMap;

use ratatui::style::Modifier;
use zlogic_protocol::query::{TranscriptBody, TranscriptEntry, TranscriptPart, TurnItem};
use zlogic_protocol::stream::ToolStatus;

use crate::app::{AppState, ReplayRound};
use crate::glyph::Glyph;
use crate::log::classify::{self, LogKind};
use crate::render::history::{HistLine, HistSpan};
use crate::theme::Sem;

fn truncate(s: &str, max_chars: usize) -> String {
    let head: String = s.chars().take(max_chars).collect();
    if head.chars().count() < s.chars().count() {
        format!("{head}…")
    } else {
        head
    }
}

fn parts_text(parts: &[TranscriptPart]) -> String {
    parts
        .iter()
        .map(|part| match part {
            TranscriptPart::Text { text } => text.clone(),
            TranscriptPart::File {
                display_name, path, ..
            } => display_name.clone().unwrap_or_else(|| path.clone()),
            TranscriptPart::Attachment { display_name, .. } => display_name.clone(),
        })
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn turn_list_row(state: &AppState, item: &TurnItem, width: usize) -> HistLine {
    let max = (width.saturating_sub(4)).max(20).min(140);
    let user = truncate(&parts_text(&item.user), max / 2);
    let mut spans: Vec<HistSpan> = Vec::new();
    spans.push(HistSpan::styled(
        format!("{} ", Glyph::Prompt.render(state.icons)),
        state.theme.style(Sem::Info),
    ));
    spans.push(HistSpan::styled(
        user,
        state.theme.style(Sem::Info).add_modifier(Modifier::BOLD),
    ));
    spans.push(HistSpan::raw("  →  "));
    if let Some(answer) = &item.answer {
        let text = truncate(answer.text.trim(), (max / 2).max(24));
        let sem = if answer.truncated {
            Sem::Warning
        } else {
            Sem::Muted
        };
        spans.push(HistSpan::styled(text, state.theme.style(sem)));
    } else if let Some(status) = &item.status {
        spans.push(HistSpan::styled(
            truncate(&format!("{status:?}"), 40),
            state.theme.style(Sem::Warning),
        ));
    } else {
        spans.push(HistSpan::styled(
            state.i18n.t("replay-no-answer"),
            state.theme.style(Sem::Muted),
        ));
    }
    /* A round can end on **cards** with no text answer. A terminal cannot render sandboxed HTML,
     * but "this round produced N cards" still has to be said — otherwise a card-only round looks
     * exactly the same as a failure / empty reply (the `replay-no-answer` line above is precisely
     * that misreading). The count comes from the `kind='widget'` asset rows in `entry_object`, and
     * has nothing to do with whether the detail can be expanded. */
    if !item.widgets.is_empty() {
        spans.push(HistSpan::raw("  "));
        spans.push(HistSpan::styled(
            state.i18n.count("replay-widget-count", item.widgets.len()),
            state.theme.style(Sem::Muted),
        ));
    }
    HistLine::spans(spans)
}

pub fn detail_rounds(state: &AppState, entries: &[TranscriptEntry]) -> Vec<ReplayRound> {
    let results: HashMap<&str, (bool, &str)> = entries
        .iter()
        .filter_map(|entry| match &entry.body {
            TranscriptBody::ToolResult {
                call_id,
                status,
                summary,
                ..
            } => Some((
                call_id.as_str(),
                (status == &ToolStatus::Completed, summary.as_str()),
            )),
            _ => None,
        })
        .collect();

    let mut rounds: Vec<RoundBuf> = Vec::new();

    for entry in entries {
        let seq = entry.round_seq.or_else(|| match &entry.body {
            TranscriptBody::Reasoning { .. }
            | TranscriptBody::Text { .. }
            | TranscriptBody::ToolCall { .. } => Some((rounds.len() + 1) as u32),
            _ => None,
        });
        let index = match seq {
            Some(seq) => {
                let idx = (seq as usize).saturating_sub(1);
                while rounds.len() <= idx {
                    rounds.push(RoundBuf::default());
                }
                idx
            }
            None => {
                if rounds.is_empty() {
                    rounds.push(RoundBuf::default());
                }
                rounds.len() - 1
            }
        };
        let buf = &mut rounds[index];

        match &entry.body {
            TranscriptBody::Reasoning { text, .. } if !text.trim().is_empty() => {
                buf.think = true;
                buf.steps += 1;
                buf.body.push(HistLine::from_line(classify::line(
                    LogKind::Thinking,
                    text.trim().to_string(),
                    &state.theme,
                    state.icons,
                )));
            }
            TranscriptBody::Text { text, .. } if !text.trim().is_empty() => {
                buf.steps += 1;
                for line in crate::markdown::block::render_markdown(text, &state.theme, state.icons)
                {
                    buf.body.push(HistLine::from_line(line));
                }
            }
            TranscriptBody::ToolCall { calls } => {
                for call in calls {
                    buf.steps += 1;
                    bump_tool(buf, &call.name);
                    let (ok, summary) = results
                        .get(call.call_id.as_str())
                        .map(|(ok, summary)| (Some(*ok), *summary))
                        .unwrap_or((None, ""));
                    buf.body
                        .push(tool_row(state, &call.name, &call.args, ok, summary));
                }
            }
            TranscriptBody::ToolResult {
                call_id,
                name,
                summary,
                status,
                ..
            } => {
                if !results_has_call(&results, call_id) {
                    buf.steps += 1;
                    buf.body.push(tool_row(
                        state,
                        name,
                        "",
                        Some(status == &ToolStatus::Completed),
                        summary,
                    ));
                }
            }
            TranscriptBody::Steering { parts } => {
                let text = parts_text(parts);
                if !text.is_empty() {
                    buf.body.push(HistLine::from_line(classify::line(
                        LogKind::Notice,
                        format!("steering · {text}"),
                        &state.theme,
                        state.icons,
                    )));
                }
            }
            TranscriptBody::Compaction {
                replaces, summary, ..
            } => {
                if !summary.is_empty() {
                    buf.body
                        .extend(crate::app::update::compaction_history_lines(
                            state, *replaces, summary,
                        ));
                }
            }
            TranscriptBody::TaskUpdate { summary, .. } => {
                if let Some(summary) = summary {
                    if !summary.is_empty() {
                        buf.body.push(HistLine::from_line(classify::line(
                            LogKind::Notice,
                            format!("task · {summary}"),
                            &state.theme,
                            state.icons,
                        )));
                    }
                }
            }
            _ => {}
        }
    }

    rounds
        .into_iter()
        .enumerate()
        .map(|(index, buf)| ReplayRound {
            head: round_head(state, index, &buf),
            open: false,
            body: buf.body,
        })
        .collect()
}

fn results_has_call(results: &HashMap<&str, (bool, &str)>, call_id: &str) -> bool {
    results.contains_key(call_id)
}

#[derive(Default)]
struct RoundBuf {
    think: bool,
    steps: usize,
    tools: Vec<(String, usize)>,
    body: Vec<HistLine>,
}

fn bump_tool(buf: &mut RoundBuf, name: &str) {
    match buf.tools.iter_mut().find(|(n, _)| n == name) {
        Some((_, count)) => *count += 1,
        None => buf.tools.push((name.to_string(), 1)),
    }
}

fn round_head(state: &AppState, index: usize, buf: &RoundBuf) -> HistLine {
    let mut parts: Vec<String> = Vec::new();
    if buf.think {
        parts.push(state.i18n.t("round-think"));
    }
    for (name, count) in buf.tools.iter().take(3) {
        parts.push(if *count > 1 {
            format!("{name} ×{count}")
        } else {
            name.clone()
        });
    }
    if buf.tools.len() > 3 {
        parts.push("…".into());
    }
    if buf.steps > 0 {
        parts.push(state.i18n.count("round-steps", buf.steps));
    }
    let text = if parts.is_empty() {
        format!("round {}", index + 1)
    } else {
        format!("round {} · {}", index + 1, parts.join(" · "))
    };
    HistLine::from_line(classify::line(
        LogKind::Round,
        text,
        &state.theme,
        state.icons,
    ))
}

fn tool_row(state: &AppState, name: &str, args: &str, ok: Option<bool>, summary: &str) -> HistLine {
    let detail = if !summary.is_empty() {
        truncate(summary, 160)
    } else {
        truncate(args, 80)
    };
    let mark = match ok {
        Some(true) => Glyph::Ok.render(state.icons),
        Some(false) => Glyph::Fail.render(state.icons),
        None => "·",
    };
    HistLine::from_line(classify::line(
        LogKind::Tool,
        format!("{name}  {mark} {detail}").trim_end().to_string(),
        &state.theme,
        state.icons,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyph::IconTier;
    use crate::theme::{themes, ColorTier, ThemeState};
    use zlogic_protocol::query::{TurnAnswer, TurnAnswerKind, TurnWidget};

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

    fn line_text(line: &HistLine) -> String {
        line.spans.iter().map(|s| s.text.as_str()).collect()
    }

    fn turn(seq: u32, answer: Option<&str>, widgets: usize) -> TurnItem {
        TurnItem {
            turn_seq: seq,
            turn_id: zlogic_protocol::TurnId::new(),
            at: chrono::Utc::now(),
            user: vec![TranscriptPart::Text {
                text: "draw three charts".into(),
            }],
            answer: answer.map(|text| TurnAnswer {
                kind: TurnAnswerKind::Text,
                text: text.into(),
                truncated: false,
            }),
            status: None,
            reason: None,
            detail: false,
            compaction: None,
            widgets: (0..widgets)
                .map(|i| TurnWidget {
                    object_id: format!("sha256:{i}"),
                    title: format!("Chart {i}"),
                    height: 300,
                    libraries: vec!["chart".into()],
                })
                .collect(),
        }
    }

    #[test]
    fn a_card_only_turn_is_not_reported_as_a_missing_reply() {
        let s = state();
        let text = line_text(&turn_list_row(&s, &turn(1, None, 3), 120));

        assert!(
            text.contains(&s.i18n.count("replay-widget-count", 3)),
            "{text}"
        );
    }

    #[test]
    fn a_turn_with_both_text_and_cards_shows_both() {
        let s = state();
        let text = line_text(&turn_list_row(&s, &turn(2, Some("all drawn"), 2), 120));

        assert!(text.contains("all drawn"), "{text}");
        assert!(
            text.contains(&s.i18n.count("replay-widget-count", 2)),
            "{text}"
        );
    }

    #[test]
    fn a_turn_without_cards_has_no_marker() {
        let s = state();
        let text = line_text(&turn_list_row(&s, &turn(3, Some("text only"), 0), 120));

        assert!(!text.contains("card"), "{text}");
        assert!(!text.contains("卡片"), "{text}");
    }
}
