use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};
use zlogic_protocol::llm::LlmEvent;
use zlogic_protocol::llm::{FinishReason, PartKind};
use zlogic_protocol::message::{ContentPart, ReasoningPart, TextPart, ToolCall, ToolCallPart};
use zlogic_protocol::usage::UsageReport;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReasoningRawSpec {
    None,
    Carrier(String),
    Explicit,
}

#[derive(Debug, Default, Clone)]
struct PartialCall {
    id: Option<String>,
    name: Option<String>,
    args: String,
    raw: Option<Value>,
}

#[derive(Debug)]
enum Open {
    Reasoning {
        index: u32,
        text: String,
        raw: Option<Value>,
        suppress_raw: bool,
    },
    Text {
        index: u32,
        text: String,
        raw: Option<Value>,
    },
    Tool {
        index: u32,
    },
}

#[derive(Debug)]
struct Stray {
    reasoning: bool,
    text: String,
    raw: Option<Value>,
    suppress_raw: bool,
}

pub struct PartEmitter {
    next_index: u32,
    open: Option<Open>,
    tool_calls: BTreeMap<u32, PartialCall>,
    detected: BTreeSet<u32>,
    tool_part_opened: bool,
    strays: Vec<Stray>,
    out: Vec<LlmEvent>,
    reasoning_spec: ReasoningRawSpec,
    warnings: Vec<String>,
    finished: bool,
}

impl PartEmitter {
    pub fn new(reasoning_spec: ReasoningRawSpec) -> Self {
        Self {
            next_index: 0,
            open: None,
            tool_calls: BTreeMap::new(),
            detected: BTreeSet::new(),
            tool_part_opened: false,
            strays: Vec::new(),
            out: Vec::new(),
            reasoning_spec,
            warnings: Vec::new(),
            finished: false,
        }
    }

    pub fn drain(&mut self) -> Vec<LlmEvent> {
        std::mem::take(&mut self.out)
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn warn(&mut self, msg: impl Into<String>) {
        self.warnings.push(msg.into());
    }

    // ───────────────────────── reasoning / text ─────────────────────────

    fn tool_open(&self) -> bool {
        matches!(self.open, Some(Open::Tool { .. }))
    }

    fn stray_mut(&mut self, reasoning: bool) -> &mut Stray {
        if !matches!(self.strays.last(), Some(s) if s.reasoning == reasoning) {
            self.stray_push(reasoning);
        }
        self.strays.last_mut().expect("just pushed")
    }

    fn stray_push(&mut self, reasoning: bool) -> &mut Stray {
        self.strays.push(Stray {
            reasoning,
            text: String::new(),
            raw: None,
            suppress_raw: false,
        });
        self.strays.last_mut().expect("just pushed")
    }

    pub fn open_reasoning(&mut self) {
        if self.tool_open() {
            self.stray_push(true);
            return;
        }
        self.ensure_reasoning();
    }

    pub fn reasoning_delta(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        if self.tool_open() {
            self.stray_mut(true).text.push_str(s);
            return;
        }
        self.ensure_reasoning();
        if let Some(Open::Reasoning { index, text, .. }) = &mut self.open {
            text.push_str(s);
            let index = *index;
            self.out.push(LlmEvent::PartDelta {
                index,
                delta: s.to_string(),
            });
        }
    }

    pub fn reasoning_delta_display_only(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        self.reasoning_delta(s);
        if self.tool_open() {
            self.stray_mut(true).suppress_raw = true;
        } else if let Some(Open::Reasoning { suppress_raw, .. }) = &mut self.open {
            *suppress_raw = true;
        }
    }

    pub fn text_delta(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        if self.tool_open() {
            self.stray_mut(false).text.push_str(s);
            return;
        }
        self.ensure_text();
        if let Some(Open::Text { index, text, .. }) = &mut self.open {
            text.push_str(s);
            let index = *index;
            self.out.push(LlmEvent::PartDelta {
                index,
                delta: s.to_string(),
            });
        }
    }

    pub fn set_reasoning_raw(&mut self, raw: Value) {
        if self.tool_open() {
            self.stray_mut(true).raw = Some(raw);
            return;
        }
        self.ensure_reasoning();
        if let Some(Open::Reasoning { raw: slot, .. }) = &mut self.open {
            *slot = Some(raw);
        }
    }

    pub fn set_text_raw(&mut self, raw: Value) {
        if self.tool_open() {
            self.stray_mut(false).raw = Some(raw);
            return;
        }
        self.ensure_text();
        if let Some(Open::Text { raw: slot, .. }) = &mut self.open {
            *slot = Some(raw);
        }
    }

    // ───────────────────────────── tool calls ─────────────────────────────

    pub fn tool_call_id(&mut self, call_index: u32, id: impl Into<String>) {
        self.tool_calls.entry(call_index).or_default().id = Some(id.into());
    }

    pub fn tool_call_name(&mut self, call_index: u32, name: impl Into<String>) {
        let name = name.into();
        if name.is_empty() {
            return;
        }
        self.tool_calls.entry(call_index).or_default().name = Some(name.clone());
        self.ensure_tool();
        let index = match self.open {
            Some(Open::Tool { index }) => index,
            _ => return,
        };
        if self.detected.insert(call_index) {
            let id = self.tool_calls.get(&call_index).and_then(|c| c.id.clone());
            self.out.push(LlmEvent::ToolCallDetected {
                index,
                call_index,
                id,
                name,
            });
        }
    }

    pub fn tool_call_args(&mut self, call_index: u32, fragment: &str) {
        self.tool_calls
            .entry(call_index)
            .or_default()
            .args
            .push_str(fragment);
    }

    pub fn tool_call_raw(&mut self, call_index: u32, raw: Value) {
        self.tool_calls.entry(call_index).or_default().raw = Some(raw);
    }

    pub fn has_tool_calls(&self) -> bool {
        self.tool_calls.values().any(|c| c.name.is_some())
    }

    pub fn usage(&mut self, report: UsageReport) {
        self.out.push(LlmEvent::Usage(report));
    }

    pub fn notice(&mut self, code: impl Into<String>, message: impl Into<String>) {
        self.out.push(LlmEvent::Notice {
            code: code.into(),
            message: message.into(),
        });
    }

    pub fn close_open(&mut self) {
        let Some(open) = self.open.take() else { return };
        match open {
            Open::Reasoning {
                index,
                text,
                raw,
                suppress_raw,
            } => {
                let raw = raw.or_else(|| {
                    if suppress_raw {
                        None
                    } else {
                        self.carrier_raw(&text)
                    }
                });
                self.out.push(LlmEvent::PartEnd {
                    index,
                    part: ContentPart::Reasoning(ReasoningPart {
                        text,
                        raw,
                        truncated: false,
                    }),
                });
            }
            Open::Text { index, text, raw } => {
                self.out.push(LlmEvent::PartEnd {
                    index,
                    part: ContentPart::Text(TextPart {
                        text,
                        raw,
                        truncated: false,
                    }),
                });
            }
            Open::Tool { index } => {
                let calls = self.collect_calls();
                if calls.is_empty() {
                    self.warnings
                        .push("tool-call part opened with no named call".into());
                    return;
                }
                self.out.push(LlmEvent::PartEnd {
                    index,
                    part: ContentPart::ToolCall(ToolCallPart { calls }),
                });
            }
        }
    }

    pub fn finish(&mut self, reason: FinishReason) {
        if self.finished {
            return;
        }
        self.finished = true;

        let truncated = matches!(reason, FinishReason::Length | FinishReason::ContentFilter);
        if truncated {
            match self.open.take() {
                Some(Open::Reasoning { index, text, .. }) => {
                    self.out.push(LlmEvent::PartEnd {
                        index,
                        part: ContentPart::Reasoning(ReasoningPart {
                            text,
                            raw: None,
                            truncated: true,
                        }),
                    });
                }
                Some(Open::Text { index, text, .. }) => {
                    self.out.push(LlmEvent::PartEnd {
                        index,
                        part: ContentPart::Text(TextPart {
                            text,
                            raw: None,
                            truncated: true,
                        }),
                    });
                }
                Some(Open::Tool { .. }) => {
                    self.warnings
                        .push("truncated tool-call part dropped".into());
                }
                None => {}
            }
        } else {
            self.close_open();
        }

        self.flush_strays();
        self.out.push(LlmEvent::ResponseEnd {
            finish_reason: reason,
        });
    }

    fn ensure_reasoning(&mut self) {
        if matches!(self.open, Some(Open::Reasoning { .. })) {
            return;
        }
        self.close_open();
        let index = self.take_index();
        self.out.push(LlmEvent::PartStart {
            index,
            kind: PartKind::Reasoning,
        });
        self.open = Some(Open::Reasoning {
            index,
            text: String::new(),
            raw: None,
            suppress_raw: false,
        });
    }

    fn ensure_text(&mut self) {
        if matches!(self.open, Some(Open::Text { .. })) {
            return;
        }
        self.close_open();
        let index = self.take_index();
        self.out.push(LlmEvent::PartStart {
            index,
            kind: PartKind::Text,
        });
        self.open = Some(Open::Text {
            index,
            text: String::new(),
            raw: None,
        });
    }

    fn ensure_tool(&mut self) {
        if matches!(self.open, Some(Open::Tool { .. })) {
            return;
        }
        if self.tool_part_opened {
            self.warnings
                .push("second tool-call group in one response was folded".into());
            return;
        }
        self.close_open();
        let index = self.take_index();
        self.out.push(LlmEvent::PartStart {
            index,
            kind: PartKind::ToolCall,
        });
        self.open = Some(Open::Tool { index });
        self.tool_part_opened = true;
    }

    fn take_index(&mut self) -> u32 {
        let i = self.next_index;
        self.next_index += 1;
        i
    }

    fn collect_calls(&self) -> Vec<ToolCall> {
        self.tool_calls
            .iter()
            .filter_map(|(idx, c)| {
                let name = c.name.clone()?;
                Some(ToolCall {
                    id: c.id.clone().unwrap_or_else(|| format!("tool_{idx}")),
                    name,
                    args: if c.args.is_empty() {
                        "{}".into()
                    } else {
                        c.args.clone()
                    },
                    raw: c.raw.clone(),
                })
            })
            .collect()
    }

    fn carrier_raw(&self, text: &str) -> Option<Value> {
        match &self.reasoning_spec {
            ReasoningRawSpec::Carrier(name) if !text.is_empty() => {
                Some(json!({ "carrier": name, "value": text }))
            }
            _ => None,
        }
    }

    fn flush_strays(&mut self) {
        if self.strays.is_empty() {
            return;
        }
        self.warnings.push(format!(
            "{} interleaved part(s) emitted after the tool-call group",
            self.strays.len()
        ));
        for stray in std::mem::take(&mut self.strays) {
            let Stray {
                reasoning,
                text,
                raw,
                suppress_raw,
            } = stray;
            if text.is_empty() && raw.is_none() {
                continue;
            }
            let index = self.take_index();
            let kind = if reasoning {
                PartKind::Reasoning
            } else {
                PartKind::Text
            };
            self.out.push(LlmEvent::PartStart { index, kind });
            if !text.is_empty() {
                self.out.push(LlmEvent::PartDelta {
                    index,
                    delta: text.clone(),
                });
            }
            let part = if reasoning {
                let raw = raw.or_else(|| {
                    if suppress_raw {
                        None
                    } else {
                        self.carrier_raw(&text)
                    }
                });
                ContentPart::Reasoning(ReasoningPart {
                    text,
                    raw,
                    truncated: false,
                })
            } else {
                ContentPart::Text(TextPart {
                    text,
                    raw,
                    truncated: false,
                })
            };
            self.out.push(LlmEvent::PartEnd { index, part });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain_all(e: &mut PartEmitter) -> Vec<LlmEvent> {
        e.drain()
    }

    #[test]
    fn switching_type_closes_the_previous_part() {
        let mut e = PartEmitter::new(ReasoningRawSpec::None);
        e.reasoning_delta("think");
        e.text_delta("answer");
        e.finish(FinishReason::Stop);
        let ev = drain_all(&mut e);

        let kinds: Vec<&str> = ev
            .iter()
            .map(|x| match x {
                LlmEvent::PartStart { .. } => "start",
                LlmEvent::PartDelta { .. } => "delta",
                LlmEvent::PartEnd { .. } => "end",
                LlmEvent::ResponseEnd { .. } => "response_end",
                _ => "other",
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "start",
                "delta",
                "end",
                "start",
                "delta",
                "end",
                "response_end"
            ]
        );
    }

    #[test]
    fn carrier_raw_is_built_from_the_full_text() {
        let mut e = PartEmitter::new(ReasoningRawSpec::Carrier("reasoning_content".into()));
        e.reasoning_delta("Let me ");
        e.reasoning_delta("work…");
        e.finish(FinishReason::Stop);
        let ev = drain_all(&mut e);
        let end = ev.iter().find_map(|x| match x {
            LlmEvent::PartEnd {
                part: ContentPart::Reasoning(p),
                ..
            } => Some(p),
            _ => None,
        });
        let p = end.expect("reasoning part_end");
        assert_eq!(p.text, "Let me work…");
        assert_eq!(
            p.raw,
            Some(json!({ "carrier": "reasoning_content", "value": "Let me work…" }))
        );
    }

    #[test]
    fn tool_part_opens_only_when_a_name_arrives() {
        let mut e = PartEmitter::new(ReasoningRawSpec::None);
        e.tool_call_id(0, "call_1");
        e.tool_call_args(0, "{\"a\":");
        e.finish(FinishReason::Stop);
        let ev = drain_all(&mut e);
        assert!(
            !ev.iter().any(|x| matches!(x, LlmEvent::PartStart { .. })),
            "no part should open without a tool name: {ev:?}"
        );
    }

    #[test]
    fn tool_calls_keep_wire_order_and_verbatim_args() {
        let mut e = PartEmitter::new(ReasoningRawSpec::None);
        e.tool_call_id(1, "call_b");
        e.tool_call_name(1, "beta");
        e.tool_call_id(0, "call_a");
        e.tool_call_name(0, "alpha");
        e.tool_call_args(1, "{\"y\": 2}");
        e.tool_call_args(0, "{\"x\": 1}");
        e.finish(FinishReason::ToolCalls);
        let ev = drain_all(&mut e);

        let calls = ev
            .iter()
            .find_map(|x| match x {
                LlmEvent::PartEnd {
                    part: ContentPart::ToolCall(p),
                    ..
                } => Some(&p.calls),
                _ => None,
            })
            .expect("tool-call part_end");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "alpha", "must be ordered by wire callIndex");
        assert_eq!(calls[0].args, "{\"x\": 1}", "args must be verbatim");
        assert_eq!(calls[1].name, "beta");

        assert!(!ev.iter().any(|x| matches!(x, LlmEvent::PartDelta { .. })));
    }

    #[test]
    fn detected_fires_once_per_call_index() {
        let mut e = PartEmitter::new(ReasoningRawSpec::None);
        e.tool_call_name(0, "shell");
        e.tool_call_name(0, "shell");
        e.finish(FinishReason::ToolCalls);
        let ev = drain_all(&mut e);
        let n = ev
            .iter()
            .filter(|x| matches!(x, LlmEvent::ToolCallDetected { .. }))
            .count();
        assert_eq!(n, 1);
    }

    #[test]
    fn truncated_text_keeps_content_but_drops_raw() {
        let mut e = PartEmitter::new(ReasoningRawSpec::Carrier("reasoning_content".into()));
        e.reasoning_delta("half a thought");
        e.finish(FinishReason::Length);
        let ev = drain_all(&mut e);
        let p = ev
            .iter()
            .find_map(|x| match x {
                LlmEvent::PartEnd {
                    part: ContentPart::Reasoning(p),
                    ..
                } => Some(p),
                _ => None,
            })
            .expect("part_end");
        assert_eq!(
            p.text, "half a thought",
            "content the user has seen must be persisted"
        );
        assert!(p.truncated);
        assert!(
            p.raw.is_none(),
            "a block without a signature is always a 400 on replay"
        );
    }

    #[test]
    fn truncated_tool_call_emits_no_part_end() {
        let mut e = PartEmitter::new(ReasoningRawSpec::None);
        e.tool_call_name(0, "shell");
        e.tool_call_args(0, "{\"cmd\": \"ec");
        e.finish(FinishReason::Length);
        let ev = drain_all(&mut e);
        assert!(
            !ev.iter().any(|x| matches!(x, LlmEvent::PartEnd { .. })),
            "a half-written args blob is always a 400 on replay: {ev:?}"
        );
        assert!(matches!(
            ev.last(),
            Some(LlmEvent::ResponseEnd {
                finish_reason: FinishReason::Length
            })
        ));
    }

    #[test]
    fn response_end_is_unique_and_last() {
        let mut e = PartEmitter::new(ReasoningRawSpec::None);
        e.text_delta("hi");
        e.finish(FinishReason::Stop);
        e.finish(FinishReason::Stop); // a repeated call should have no effect
        let ev = drain_all(&mut e);
        let n = ev
            .iter()
            .filter(|x| matches!(x, LlmEvent::ResponseEnd { .. }))
            .count();
        assert_eq!(n, 1);
        assert!(matches!(ev.last(), Some(LlmEvent::ResponseEnd { .. })));
    }

    #[test]
    fn index_is_monotonic() {
        let mut e = PartEmitter::new(ReasoningRawSpec::None);
        e.reasoning_delta("a");
        e.text_delta("b");
        e.tool_call_name(0, "t");
        e.finish(FinishReason::ToolCalls);
        let ev = drain_all(&mut e);
        let mut last = None;
        for x in &ev {
            if let LlmEvent::PartStart { index, .. } = x {
                if let Some(prev) = last {
                    assert!(*index > prev, "index must increase monotonically");
                }
                last = Some(*index);
            }
        }
        assert_eq!(last, Some(2));
    }

    #[test]
    fn reasoning_between_calls_does_not_lose_the_later_call() {
        let mut e = PartEmitter::new(ReasoningRawSpec::Explicit);
        e.open_reasoning();
        e.reasoning_delta("first thought");
        e.set_reasoning_raw(json!({ "id": "rs_1", "encrypted_content": "a" }));
        e.tool_call_id(0, "call_a");
        e.tool_call_name(0, "alpha");
        e.tool_call_args(0, "{}");
        e.open_reasoning();
        e.reasoning_delta("second thought");
        e.set_reasoning_raw(json!({ "id": "rs_2", "encrypted_content": "b" }));
        e.tool_call_id(1, "call_b");
        e.tool_call_name(1, "beta");
        e.tool_call_args(1, "{}");
        e.finish(FinishReason::ToolCalls);
        let ev = drain_all(&mut e);

        let calls = ev
            .iter()
            .find_map(|x| match x {
                LlmEvent::PartEnd {
                    part: ContentPart::ToolCall(p),
                    ..
                } => Some(&p.calls),
                _ => None,
            })
            .expect("tool-call part_end");
        assert_eq!(calls.len(), 2, "both calls must survive");
        assert_eq!(calls[0].name, "alpha");
        assert_eq!(calls[1].name, "beta");

        let detected = ev
            .iter()
            .filter(|x| matches!(x, LlmEvent::ToolCallDetected { .. }))
            .count();
        assert_eq!(detected, 2);
    }

    #[test]
    fn stray_reasoning_keeps_its_raw_and_accumulates_deltas() {
        let mut e = PartEmitter::new(ReasoningRawSpec::Explicit);
        e.tool_call_name(0, "alpha");
        e.open_reasoning();
        e.reasoning_delta("thinking ");
        e.reasoning_delta("more");
        e.set_reasoning_raw(json!({ "type": "thinking", "signature": "sig" }));
        e.finish(FinishReason::ToolCalls);
        let ev = drain_all(&mut e);

        let parts: Vec<&ReasoningPart> = ev
            .iter()
            .filter_map(|x| match x {
                LlmEvent::PartEnd {
                    part: ContentPart::Reasoning(p),
                    ..
                } => Some(p),
                _ => None,
            })
            .collect();
        assert_eq!(
            parts.len(),
            1,
            "fragments that arrive in slices must merge into one part, not one part per delta"
        );
        assert_eq!(parts[0].text, "thinking more");
        assert_eq!(
            parts[0].raw,
            Some(json!({ "type": "thinking", "signature": "sig" }))
        );
    }

    #[test]
    fn each_stray_reasoning_block_keeps_its_own_raw() {
        let mut e = PartEmitter::new(ReasoningRawSpec::Explicit);
        e.tool_call_name(0, "alpha");

        e.open_reasoning();
        e.reasoning_delta("first");
        e.set_reasoning_raw(json!({ "id": "rs_1", "encrypted_content": "a" }));
        e.open_reasoning();
        e.reasoning_delta("second");
        e.set_reasoning_raw(json!({ "id": "rs_2", "encrypted_content": "b" }));
        e.finish(FinishReason::ToolCalls);
        let ev = drain_all(&mut e);

        let parts: Vec<&ReasoningPart> = ev
            .iter()
            .filter_map(|x| match x {
                LlmEvent::PartEnd {
                    part: ContentPart::Reasoning(p),
                    ..
                } => Some(p),
                _ => None,
            })
            .collect();
        assert_eq!(parts.len(), 2, "two blocks must become two parts");
        assert_eq!(parts[0].text, "first");
        assert_eq!(parts[0].raw.as_ref().unwrap()["encrypted_content"], "a");
        assert_eq!(parts[1].text, "second");
        assert_eq!(parts[1].raw.as_ref().unwrap()["encrypted_content"], "b");
    }

    #[test]
    fn stray_atomic_reasoning_block_survives() {
        let mut e = PartEmitter::new(ReasoningRawSpec::Explicit);
        e.tool_call_name(0, "alpha");
        e.open_reasoning();
        e.set_reasoning_raw(json!({ "type": "redacted_thinking", "data": "enc" }));
        e.finish(FinishReason::ToolCalls);
        let ev = drain_all(&mut e);
        let p = ev
            .iter()
            .find_map(|x| match x {
                LlmEvent::PartEnd {
                    part: ContentPart::Reasoning(p),
                    ..
                } => Some(p),
                _ => None,
            })
            .expect("raw must be kept even with no plaintext");
        assert!(p.text.is_empty());
        assert_eq!(
            p.raw,
            Some(json!({ "type": "redacted_thinking", "data": "enc" }))
        );
    }

    #[test]
    fn text_between_calls_is_folded_after_the_group() {
        let mut e = PartEmitter::new(ReasoningRawSpec::None);
        e.tool_call_name(0, "alpha");
        e.text_delta("stray");
        e.tool_call_name(1, "beta");
        e.finish(FinishReason::ToolCalls);
        let ev = drain_all(&mut e);

        let tool_ends = ev
            .iter()
            .filter(|x| {
                matches!(
                    x,
                    LlmEvent::PartEnd {
                        part: ContentPart::ToolCall(_),
                        ..
                    }
                )
            })
            .count();
        assert_eq!(tool_ends, 1, "at most one tool-call part per response");

        let positions: Vec<usize> = ev
            .iter()
            .enumerate()
            .filter_map(|(i, x)| match x {
                LlmEvent::PartEnd {
                    part: ContentPart::ToolCall(_),
                    ..
                } => Some(i),
                _ => None,
            })
            .collect();
        let text_pos = ev.iter().position(|x| {
            matches!(x, LlmEvent::PartEnd { part: ContentPart::Text(t), .. } if t.text == "stray")
        });
        assert!(text_pos.unwrap() > positions[0]);
        assert!(!e.warnings().is_empty(), "a deviation must be recorded");
    }
}
