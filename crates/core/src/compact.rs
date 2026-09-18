//! Compaction: summarise old turns so a long conversation still fits.
//! # Nothing is deleted and nothing is rewritten
//! Compaction **appends one entry**: `kind: compaction`, holding a range of turn numbers and the
//! summary text. The turns it covers stay exactly where they are, byte for byte. `build_context` is
//! what replaces them on the way to the model — so the timeline, replay, export and rollback all
//! still see the real conversation, and a bad summary is a *view* problem rather than the permanent
//! loss of what the user actually said.
//! It also makes the operation crash-safe for free: either the entry is there or it is not.
//! Deleting and inserting could leave a history with a hole in the middle.
//! ```text
//! entries:  turn 1..6              turn 7..10        compaction{1-6}
//!                │                      │                  │
//! context:  <conversation_summary 1-6>  +  turns 7..10 structured
//! ```
//! # The tail stays structured
//! The most recent `tail_turns` are never covered. That is not a nicety: flattening them into prose
//! would throw away every reasoning block's `signature` / `encrypted_content`, and the next round of
//! an in-flight tool chain would be a hard 400. `tail_turns >= 1` is validated in the config.
//! # Summaries chain rather than nest
//! A later compaction starts where the last one stopped, so ranges are disjoint and a long session
//! accumulates `summary(1-6), summary(7-12), …`. The summarisation call for a later range *reads*
//! the earlier summary (it falls inside the input window), so nothing is lost from the chain. If a
//! range ever does contain an earlier one — a re-compaction that summarises summaries — the
//! contained one is superseded and not rendered twice.
//! # The summarising call rides on the conversation's own prefix
//! When the summary is written by the conversation's **own** model — no `compaction` role
//! configured, or one that resolves to the same `provider:model` — the call is assembled as a
//! request of that conversation: the same system prompt, the same tool definitions, and the
//! conversation's opening messages up to where the protected tail begins, plus a final user message
//! asking for the summary. What it shares with the round's own request it shares **byte for byte**,
//! so a provider can serve that part from its prompt cache at the cached-input price.
//! **The tail is cut, not carried.** The most recent `tail_turns` are not summarised, so feeding
//! them to the summariser would only invite it to restate them — the same recent facts then sit in
//! the context twice, once verbatim and once in prose. Cutting them out also makes the instruction
//! simpler: there is no boundary for the model to locate, because everything it can see is being
//! replaced. The earlier shape sent the whole conversation and told the model "the most recent
//! exchanges stay as they are" — a rule it had no way to check.
//! **Cutting the tail costs nothing in cache.** A cache read is not "does this exact request exist"
//! but: hash the prefix at a breakpoint in *this* request, then walk **backwards** looking for a
//! position an earlier request wrote. The previous round's newest user message — the position it
//! wrote — sits before the cut, so the walk finds it. The walk is also shorter than it would be for
//! a request carrying the tail, which is what keeps it inside a provider's look-back window.
//! That bounds the request too: it is the range being replaced plus one instruction, so it is
//! *smaller* than the round's own request. A provider that still refuses it fails the threshold path
//! softly — the round runs, and if it too does not fit, the overflow path takes over.
//! Two cases cannot use it, and both fall back to a request of their own: a different model writes
//! the summary (another model has another cache), or the provider has just said the request was too
//! long — that request is by definition the one that did not fit, so resending it with one more
//! message cannot fit either.
//! # An empty, or half-finished, summary is discarded
//! A model that returns nothing would otherwise erase the whole covered range and put nothing in its
//! place. That is worse than not compacting: the context gets *smaller* and *wrong*. The same goes
//! for a reply the output limit cut off mid-sentence — it is the first part of a summary, not one.
//! Both are dropped with a warning and the turn carries on uncompacted, and a reply that was a tool
//! call rather than text is reported as exactly that.
//! The reply itself is used **as written**: no JSON, no schema, no delimiter. See
//! [`extract_summary`] for why, and for the one thing that is stripped.

use serde::{Deserialize, Serialize};

use futures_util::StreamExt;
use zlogic_protocol::llm::{
    CacheSpec, LlmEvent, LlmRequest, MessageCache, RequestMeta, ThinkingIntent, ThinkingMode,
};
use zlogic_protocol::message::{ContentPart, Message, Role, TextPart};
use zlogic_protocol::stream::{CompactionReason, NoticeLevel, StreamPayload};
use zlogic_protocol::usage::Purpose;
use zlogic_protocol::{RoundId, TurnId};
use zlogic_store::{EntryKind, EntryRecord, NewEntry};

use crate::{AuxModel, Core, CoreError, Result, TurnEmitter, context, cost};
use zlogic_tools::Materialized;

/// The payload of a `kind: compaction` entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    /// First covered turn, inclusive.
    pub from_turn: i64,
    /// Last covered turn, **inclusive**.
    pub to_turn: i64,
    pub content: String,
    pub reason: CompactionReason,
    /// The model that wrote it, `provider:model`. Diagnostic: a summary that reads oddly is
    /// usually a small aux model, and this is how you find out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_ref: Option<String>,
    /// How many tokens the summary itself cost.
    /// Persisted rather than only streamed on `CompactionEnd`: otherwise the live view shows a
    /// figure and the reloaded transcript shows nothing, for the same event. `None` when the
    /// provider reported no usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_tokens: Option<u64>,
}

impl Summary {
    pub fn covers(&self, turn_seq: i64) -> bool {
        turn_seq >= self.from_turn && turn_seq <= self.to_turn
    }

    /// Whether `other`'s range sits entirely inside this one.
    pub fn contains_range(&self, other: &Summary) -> bool {
        other.from_turn >= self.from_turn && other.to_turn <= self.to_turn
    }

    pub fn turns(&self) -> i64 {
        (self.to_turn - self.from_turn + 1).max(0)
    }

    /// How the summary reaches the model.
    /// A **user** message, deliberately. Not `system`: the system prompt is assembled per request
    /// and has no position in the history, while a summary has to sit exactly where the turns it
    /// replaces were. Not `assistant`: an assistant message carries a source stamp for the raw gate,
    /// and stamping a summary with a model it may not have come from is the precise confusion that
    /// gate exists to prevent.
    /// The tag is there so the model can tell recalled context from something the user just said.
    pub fn to_message(&self) -> Message {
        Message::user(vec![ContentPart::Text(TextPart {
            text: format!(
                "<conversation_summary turns=\"{}-{}\">\n{}\n</conversation_summary>",
                self.from_turn, self.to_turn, self.content
            ),
            raw: None,
            truncated: false,
        })])
    }
}

/// How much context may be used before compacting, and how much of the tail is protected.
#[derive(Debug, Clone)]
pub struct ContextPolicy {
    /// Compact once the last reported `input_tokens` reaches this fraction of the window.
    /// A **lagging** signal, not a prediction: compacting one turn early costs one extra summary,
    /// compacting one turn late is a provider error. So it errs small.
    pub compact_ratio: f32,
    /// How many recent turns stay structured. Must be at least 1 — see the module docs.
    pub tail_turns: u32,
    /// How many times a `context_length_exceeded` may be answered by compacting and retrying.
    pub overflow_retries: u32,
}

impl Default for ContextPolicy {
    fn default() -> Self {
        Self {
            compact_ratio: 0.8,
            // One turn: the running one. It is the floor, not a preference — see the module docs —
            // and it is also all that needs to survive verbatim, since everything older is what the
            // summary is for. A bigger value trades context for exactness.
            tail_turns: 1,
            overflow_retries: 1,
        }
    }
}

impl ContextPolicy {
    /// The `input_tokens` figure at which compaction is due.
    /// An explicit `compaction_threshold` on the model wins: someone who wrote a token count knows
    /// something about that model the ratio does not.
    pub fn threshold(&self, window: u64, model_threshold: Option<u64>) -> u64 {
        model_threshold
            .unwrap_or_else(|| (window as f64 * self.compact_ratio as f64).round() as u64)
    }
}

/// The summaries that still apply, oldest first.
/// A summary contained in a later one is dropped: re-compaction supersedes what it swallowed, and
/// rendering both would repeat the same material twice.
pub fn effective_summaries(
    entries: &[EntryRecord],
    load: context::LoadData<'_>,
) -> Result<Vec<Summary>> {
    let mut all: Vec<Summary> = Vec::new();
    for rec in entries.iter().filter(|e| e.kind == EntryKind::Compaction) {
        let data = load(rec)?;
        let summary: Summary = serde_json::from_value(data).map_err(|e| {
            CoreError::Corrupt(format!(
                "compaction entry {} is unreadable: {e}",
                rec.entry_id
            ))
        })?;
        all.push(summary);
    }
    all.sort_by_key(|s| (s.from_turn, s.to_turn));

    let mut kept: Vec<Summary> = Vec::new();
    for s in &all {
        // Superseded if any *other* summary's range strictly contains this one's.
        let superseded = all.iter().any(|other| {
            other.contains_range(s) && (other.from_turn, other.to_turn) != (s.from_turn, s.to_turn)
        });
        if !superseded {
            kept.push(s.clone());
        }
    }
    Ok(kept)
}

/// End of the contiguous compacted prefix starting at turn 1.
/// Only a prefix is safe to omit from the database query: a hand-authored or future disjoint
/// summary may cover a middle range, and dropping everything before its `to_turn` would erase the
/// uncovered gap.
pub fn covered_prefix_end(summaries: &[Summary]) -> Option<i64> {
    let mut end = 0;
    for summary in summaries {
        if summary.from_turn > end + 1 {
            break;
        }
        end = end.max(summary.to_turn);
    }
    (end > 0).then_some(end)
}

/// The range the next compaction should cover, or `None` when there is nothing to do.
/// Starts where the last summary stopped and ends `tail_turns` short of the newest turn — including
/// the turn now in flight, which must never be summarised out from under itself.
pub fn plan_range(
    entries: &[EntryRecord],
    summaries: &[Summary],
    tail_turns: u32,
) -> Option<(i64, i64)> {
    // **A summary is not a conversation turn.** Manual compaction runs as its own turn and therefore
    // allocates its own `turn_seq` for the entry it writes; counting those pushes `newest` forward
    // once per run and moves the tail boundary with it. Five manual compactions in a row with
    // `tail_turns = 4` would summarise the newest real turn — exactly what the tail exists to
    // prevent, and the reasoning `signature`s it drops are a hard 400 on the next tool chain.
    let conversation = || {
        entries
            .iter()
            .filter(|e| e.kind != EntryKind::Compaction)
            .map(|e| e.turn_seq)
    };
    let newest = conversation().max()?;
    let oldest = conversation().min()?;

    let from = summaries
        .iter()
        .map(|s| s.to_turn)
        .max()
        .map_or(oldest, |t| t + 1);
    let to = newest - tail_turns as i64;
    (to >= from).then_some((from, to))
}

/// What has to survive into the summary, in priority order.
/// Written around what the *next* rounds need rather than what reads well: an agent resuming from
/// this has to know which files it touched, what it already tried, and what it was in the middle of.
/// A summary that reads like minutes of a meeting but omits the file paths makes the model redo work.
/// **One copy, two prompts.** The two shapes below differ in *where* this is said, never in what:
/// keeping the requirements in one place is what stops the standalone summary from quietly getting
/// worse than the one that rides the conversation's prefix.
const PRESERVE: &str = "\
What the summary has to carry, above all:

1. What the user asked for — every requirement, constraint, and anything they explicitly rejected.
2. The decisions taken and the reasons given for them.
3. Concrete facts discovered: file paths, symbol names, commands, error messages, versions, \
numbers. Keep them verbatim — an approximate path is worse than none.
4. What is in progress right now and what the immediate next step was.

Images and other visual attachments will not be replayed once this part is replaced by your \
summary. For every image, preserve its file name and all visually established facts that could \
affect later work: text visible in it, UI state, errors, measurements, diagrams, and conclusions \
drawn from it. Do not merely write \"an image was attached\".

Leave out: pleasantries, restatements, and step-by-step narration of work that ended up discarded \
(say that it was tried and rejected, and why).";

/// How the summary has to come back: the text, and nothing around it.
/// A preamble is not free. This text is replayed in the conversation's place on **every** later
/// request, and it is also what `/compact` shows, so "Here is the summary:" is noise that gets paid
/// for repeatedly. Asking for the text alone is cheaper and more honest than stripping it
/// afterwards — a stripper that removes a first line would have to guess at prose, and would eat a
/// summary's own first heading as readily as a preamble.
const REPLY_DISCIPLINE: &str = "\
Answer with the summary text and nothing else, in the same language the conversation used. No \
preamble (\"Here is the summary:\", \"To sum up,\"), no closing remark, no offer to help, no \
title announcing the answer itself, and no code fence around the whole thing: your answer is \
replayed verbatim in place of the original conversation, so anything around it is something the \
user has to read past.";

/// The system prompt of a summary call that shares nothing with the conversation: a request of its
/// own, with no tools, whose message list *is* the part being replaced.
fn standalone_system() -> String {
    format!(
        "You are compacting the earlier part of a working session between a user and a coding \
         agent, so the agent can continue with less context. The transcript below is exactly the \
         part being replaced.\n\n\
         {PRESERVE}\n\n\
         {REPLY_DISCIPLINE}\n\n\
         Do not address the user and do not describe what you are doing."
    )
}

/// The newest user message of a summary call that rides the conversation's own prompt prefix.
/// Appended at the end. The request's message list **is** the range being replaced — the protected
/// tail is cut off — so the instruction needs no boundary language at all: there is nothing above it
/// that is not being summarised, and nothing it must not touch. That is the whole reason to cut the
/// tail here rather than send the conversation whole: a prompt that has to explain "the most recent
/// exchanges stay as they are" is explaining a rule the model has no way to check.
/// **It may still share a message with a user message.** When the range ends on a tool result (a
/// turn that was interrupted or failed), the instruction has to merge into it — two consecutive
/// user messages are a shape several providers reject.
/// It also says *do not call tools*, because this request carries the conversation's tool
/// definitions (they are part of the cached prefix). A model that calls one anyway answers with no
/// text at all, and the empty/tool-call guard reports that rather than writing a summary that is not
/// one.
fn prefix_instruction() -> String {
    format!(
        "Summarise the conversation above. It is the earlier part of this session, and your answer \
         replaces it in your context, so anything the work still needs has to be in your summary — \
         when in doubt about whether something belongs, include it: a detail repeated costs a few \
         tokens, a detail left out costs the work of finding it again.\n\n\
         {PRESERVE}\n\n\
         {REPLY_DISCIPLINE}\n\n\
         Do not call tools."
    )
}

/// One attempt's reply, called not-a-summary.
/// Its own type rather than a bare `Option` because each case has its own thing to say, and saying
/// the right one is the difference between "the model was cut off" and "the model ignored the
/// instruction" — the two lead to different fixes.
struct Unusable {
    code: &'static str,
    /// Completes "…the summary call {reason}".
    reason: &'static str,
}

/// How many times the summary call is made before giving up.
/// A second attempt covers the ordinary case: a reply cut off by the output limit, or one that came
/// back empty, is usually a sampling accident and an independent regeneration is a different draw.
/// It is deliberately **not** a config knob — a third or fourth attempt does not help a model whose
/// `max_output_tokens` cannot fit the summary at all, while every attempt is a full-window request
/// that the user pays for.
/// Public because the number is on the bill: two attempts are two full-window requests, and a test
/// that asserts "both attempts were charged" has to name the same constant this does.
pub const SUMMARY_ATTEMPTS: u32 = 2;

/// Decides what one attempt produced.
/// A summary is written **as it came back** — no format is imposed and nothing is parsed out of it
/// (see [`extract_summary`]). The three ways a reply is *not* a summary are all reported rather than
/// patched over:
/// - **truncated** — the output limit cut it off mid-sentence. It carries the flag on the part
///   itself (see `PartEmitter::finish`), so this is a fact about the reply, not a guess about the
///   text. Writing it would hide the covered turns behind the first part of a replacement, which
///   does not look broken — only terse.
/// - **empty** — nothing to write at all.
/// - **tool call** — the riding shape sends the conversation's tool definitions, so a model that
///   ignores the instruction can answer with a call and no text.
fn usable_summary(
    text: &str,
    truncated: bool,
    tool_calls: u32,
) -> std::result::Result<String, Unusable> {
    if truncated {
        return Err(Unusable {
            code: "compaction_truncated",
            reason: "was cut off by the model's output limit",
        });
    }
    match extract_summary(text) {
        Some(summary) => Ok(summary),
        None if tool_calls > 0 => Err(Unusable {
            code: "compaction_tool_call",
            reason: "answered with a tool call instead of text",
        }),
        None => Err(Unusable {
            code: "compaction_empty",
            reason: "came back empty",
        }),
    }
}

/// The summary out of what the model answered, or `None` when there is nothing worth writing.
/// # No format is imposed on the reply
/// Not JSON, not a schema, not a delimiter. Three reasons, in the order they matter:
/// - **Nothing is parsed out of it.** The payload is prose that goes back into the conversation as
///   prose; there is no field to read. A wrapper would be overhead plus a new way to lose a whole
///   summary (a fence the parser does not expect, a trailing comma, a reply cut off mid-object) —
///   and a lost summary is not a cosmetic failure: on the overflow path it fails the turn.
/// - **It would fork the cached prefix.** The riding shape exists so the provider serves this call
///   from the conversation's own prompt cache, which matches byte for byte. Structured output is
///   request-body configuration that providers may render *into the prompt* (Anthropic renders the
///   thinking effort that way), and a prefix that differs by one token is a prefix that is re-billed
///   in full — several times the price of the summary itself.
/// - **Reasoning is already excluded.** Only finished text parts are read, never deltas and never
///   reasoning — so "how the model got there" is not part of the summary either.
/// # What *is* stripped
/// Only a wrapper around the **whole** answer, which is mechanical rather than a guess about prose:
/// a fenced code block, or a matching pair of tags (`<summary>…</summary>`). Replayed verbatim those
/// are three stray backticks plus an indented body.
/// A chatty preamble the model wrote itself ("Here is the summary:") is **left in**. It costs a few
/// tokens, while the heuristic that would remove it — a short first line ending in a colon — also
/// removes a summary's own first heading, and a wrong guess about prose is worse than a noisy
/// preamble.
fn extract_summary(answer: &str) -> Option<String> {
    let trimmed = answer.trim();
    let unwrapped = strip_whole_tag(trimmed)
        .or_else(|| strip_whole_fence(trimmed))
        .unwrap_or(trimmed);
    let text = unwrapped.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// A tag pair wrapping the whole answer, e.g. `<summary>…</summary>`.
/// Only a bare opening tag with a plain name: an attribute makes it a different thing to reason
/// about, and this is not worth reasoning about.
fn strip_whole_tag(text: &str) -> Option<&str> {
    let (name, rest) = text.strip_prefix('<')?.split_once('>')?;
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    rest.strip_suffix(&format!("</{name}>"))
}

/// A fenced code block wrapping the whole answer.
/// The closing fence is the answer's **last** line, so a body means a newline before it — and an
/// answer that is nothing but an opening and a closing fence (`"```\n```"`) has an empty body, which
/// [`extract_summary`] then treats as nothing. Writing that verbatim would erase the covered turns
/// and put a fence in their place.
fn strip_whole_fence(text: &str) -> Option<&str> {
    let (first, rest) = text.split_once('\n')?;
    let opening = first.trim();
    let ticks = opening.chars().take_while(|c| *c == '`').count();
    if ticks < 3 {
        return None;
    }
    match rest.rsplit_once('\n') {
        Some((body, last)) => is_closing_fence(last, ticks).then_some(body),
        None => is_closing_fence(rest, ticks).then_some(""),
    }
}

fn is_closing_fence(line: &str, opening_ticks: usize) -> bool {
    let trimmed = line.trim();
    let count = trimmed.chars().count();
    count >= opening_ticks && trimmed.chars().all(|c| c == '`')
}

/// What the conversation will cost the next request, in tokens — the compaction **post-condition**,
/// not a figure for the UI.
/// The UI shows `summary_tokens` (what the summary itself cost, measured) after a compaction; this
/// answers a different question — "did the request actually get smaller than the window?" — and it
/// is the one place an estimate is unavoidable: the covered range cannot be counted locally, and
/// deliberately so, since a bundled tokenizer would be a second source of truth that drifts from the
/// provider's. So it is estimated from the model-facing text at ~4 characters per token, and the rest
/// is measured: `last_input` from the provider's own count, `summary_tokens` from the call that
/// produced the summary.
/// It does not include the message about to be sent in this turn — compaction runs before that
/// request is built. A few hundred tokens on a window's worth of conversation.
fn remaining_after_compaction(
    last_input: u64,
    covered_chars: usize,
    summary_tokens: Option<u64>,
    summary_chars: usize,
) -> u64 {
    let covered = covered_chars as u64 / 4;
    let added = summary_tokens.unwrap_or(summary_chars as u64 / 4);
    last_input.saturating_sub(covered).saturating_add(added)
}

/// Whether the summary call can be assembled as one more request of the conversation itself.
/// Three conditions, each about **provable** identity rather than intent:
/// - the model that writes the summary is the conversation's own model, by `provider:model` — a
///   prompt cache belongs to a model, and a different one cannot hit this prefix;
/// - the caller handed us the system prompt the conversation is actually using. Without it the
///   prefix diverges at the system prompt, which sits before the first message, and everything
///   after it is re-billed at full price — reuse would then cost *more* than a standalone call;
/// - the trigger is not a provider saying the request was too long. That request is by definition
///   the one that did not fit, so resending it with one more message cannot fit either. The trigger
///   is the one case where a smaller request is the only thing that can work.
fn rides_the_prefix(plan: &crate::TurnPlan, aux: &AuxModel, reason: CompactionReason) -> bool {
    reason != CompactionReason::ContextOverflow
        && !plan.system.is_empty()
        && aux.model_key() == plan.model_key()
}

/// Tells a **manual** caller that there was nothing to do.
/// Automatic compaction stays silent: it is driven by a threshold a long session crosses many
/// times, and "nothing old enough yet" is the ordinary state of a short one. Someone who typed
/// `/compact`, on the other hand, has to be told — a command that writes nothing and says nothing
/// is indistinguishable from a broken one.
fn nothing_to_compact(emitter: &TurnEmitter, reason: CompactionReason) {
    if reason != CompactionReason::Manual {
        return;
    }
    emitter.notice(
        NoticeLevel::Info,
        "compaction_nothing_to_do",
        "nothing to compact yet: the conversation is still inside the most recent turns, which \
         compaction never covers",
        std::collections::BTreeMap::new(),
    );
}

/// The turns being summarised, and what the summarising model will be shown.
/// Computed together under one database lock: the range and the messages have to agree, and reading
/// the history twice would let a concurrent write come between them.
struct Window {
    from: i64,
    to: i64,
    /// **Exactly the range being replaced** — the protected tail is not in here. It is not
    /// summarised, so showing it to the summariser would only invite it to restate what stays.
    messages: Vec<Message>,
    /// Characters of model-facing text the covered range contributes — the lower bound on what
    /// compaction removes.
    chars: usize,
}

impl Core {
    /// Whether the last reported context size has reached the threshold.
    /// `None` from the store means "no conversation usage yet", which is different from zero:
    /// the first round of a root or child session has nothing to judge and must not compact.
    pub(crate) fn compaction_due(&self, plan: &crate::TurnPlan) -> Result<bool> {
        let session = self.session_id();
        let Some(last) = self
            .services()
            .store
            .with(|db| db.usage().last_conversation_input_tokens(session))?
        else {
            return Ok(false);
        };
        let threshold = self
            .services()
            .context
            .threshold(plan.model.context_window, plan.model.compaction_threshold);
        Ok(last >= threshold)
    }

    /// The upper bound the next compaction could reach: one turn short of the protected tail.
    /// `None` when the history cannot be shrunk any further — the protected tail already reaches
    /// the newest turn, so another summary would cover nothing.
    fn next_compaction_bound(&self) -> Result<Option<i64>> {
        let session = self.session_id();
        let tail = self.services().context.tail_turns;
        Ok(self
            .services()
            .store
            .with(|db| db.entries().max_conversation_turn(session))?
            .map(|newest| newest - tail as i64)
            .filter(|bound| *bound >= 1))
    }

    /// Summarises the next range and appends the summary entry.
    /// Returns whether anything was written. `false` covers the ordinary cases — nothing old enough
    /// to compact, or a model that produced nothing usable — and is not an error.
    /// `tools` is the turn's materialised tool set. The shape that rides the conversation's own
    /// prefix sends these definitions too: they are part of that prefix, so leaving them out would
    /// invalidate the very cache the shape exists to hit.
    pub(crate) async fn compact(
        &self,
        plan: &crate::TurnPlan,
        tools: &Materialized,
        emitter: &TurnEmitter,
        turn_id: TurnId,
        turn_seq: i64,
        reason: CompactionReason,
    ) -> Result<bool> {
        let mut aux = plan.compaction_model();
        let session = self.session_id();
        let objects = self.services().objects.clone();

        // Cheap pre-check before the full scan: if the protected tail already reaches the newest
        // turn there is nothing left to summarise, and `plan_range` inside the lock would agree.
        // Keeps the per-round threshold check from paying a full `list_for_context` when the
        // summary written last round is already the frontier.
        if self.next_compaction_bound()?.is_none() {
            nothing_to_compact(emitter, reason);
            return Ok(false);
        }

        // One pass over the history. Both the range to cover and the messages to send are decided
        // here, under the same lock, because the two have to agree.
        let prepared = self.services().store.with(|db| -> Result<Option<Window>> {
            let entries = db.entries();
            let loader = context::store_loader(&entries, objects.as_ref());
            let object_loader = context::object_loader(objects.as_ref());

            // The summaries come from the compaction rows alone — that is all `effective_summaries`
            // reads, and `list_kind` loads just those.
            let summaries =
                effective_summaries(&entries.list_kind(session, EntryKind::Compaction)?, &loader)?;
            // Everything the compacted prefix already covers is **not read at all** — the same trim
            // a round applies before building its request. On a long session this is the difference
            // between scanning a few hundred rows and scanning the whole history.
            let all = match covered_prefix_end(&summaries) {
                Some(end) => entries.list_for_context_after(session, end)?,
                None => entries.list_for_context(session)?,
            };
            let Some((from, to)) = plan_range(&all, &summaries, self.services().context.tail_turns)
            else {
                return Ok(None);
            };

            // What compaction **removes**: the covered range, plus any earlier summary written
            // inside it (that is how the chain stays lossless). These are also exactly the messages
            // the summariser is shown — see the module docs on why the tail is cut.
            let window: Vec<EntryRecord> = all
                .iter()
                .filter(|e| e.turn_seq >= from && e.turn_seq <= to)
                .cloned()
                .collect();
            let covered =
                context::build_context(&window, &aux.model.source, &loader, &object_loader)?;
            let chars = covered
                .messages
                .iter()
                .flat_map(|m| &m.content)
                .filter_map(|p| match p {
                    ContentPart::Text(t) => Some(t.text.chars().count()),
                    _ => None,
                })
                .sum();
            Ok(Some(Window {
                from,
                to,
                messages: covered.messages,
                chars,
            }))
        })?;

        let Some(Window {
            from,
            to,
            messages,
            chars,
        }) = prepared
        else {
            nothing_to_compact(emitter, reason);
            return Ok(false);
        };
        if messages.is_empty() {
            return Ok(false);
        }
        if contains_image(&messages)
            && aux.model.capabilities.vision == Some(false)
            && plan.model.capabilities.vision != Some(false)
        {
            emitter.notice(
                NoticeLevel::Info,
                "compaction_model_requires_vision",
                "the configured compaction model cannot read images; using the conversation model \
                 so visual facts survive the summary",
                std::collections::BTreeMap::new(),
            );
            aux = AuxModel {
                model: plan.model.clone(),
                client: plan.client.clone(),
            };
        }

        emitter.send(StreamPayload::CompactionStart { reason });

        // Decided **after** the vision fallback, which can swap in the conversation's own model and
        // thereby make the prefix reusable. The messages are the same either way — the range being
        // replaced — so nothing else depends on this.
        let rides = rides_the_prefix(plan, &aux, reason);

        // Two shapes, decided once here so the request below reads as one request.
        let (system, request_tools, cache, instruction) = if rides {
            (
                // The conversation's own system prompt, unchanged: it is the first thing in the
                // cached prefix, and a different one would re-bill everything after it.
                context::build_system(plan.system.clone()),
                // The same definitions the conversation sends. Not so the model can use them — the
                // instruction forbids that — but because they are part of the prefix.
                tools.definitions(),
                CacheSpec {
                    // The round's own policy, breakpoints included. The breakpoint on tools and the
                    // one on the system prompt are what make the stored prefix *readable*, and this
                    // request **is** a prefix of the round's own: same system, same tools, same
                    // opening messages, cut off where the protected tail begins.
                    // Cutting the tail does not cost the read. A provider looks for cached content
                    // by hashing the prefix at a breakpoint and walking *backwards* for a position
                    // an earlier request wrote — and that position (the previous round's newest
                    // user message) sits **before** the cut. The walk is shorter than it is for a
                    // request that carries the tail, so this is the safer of the two.
                    prompt_key: Some(session.to_string()),
                    ..Default::default()
                },
                Some(prefix_instruction()),
            )
        } else {
            (
                context::build_system(vec![standalone_system()]),
                Vec::new(),
                CacheSpec {
                    // **No breakpoints.** This request shares no prefix with anything — its own
                    // system prompt, no tools, a range that starts where the last summary stopped —
                    // so a breakpoint here could only *write* an entry nothing will ever read, and
                    // a write is billed above the input price. The routing hint stays: it is what
                    // lands the call on a machine that holds the session's prefix, which the next
                    // main round then reads back.
                    tools: false,
                    system: false,
                    messages: MessageCache::None,
                    ttl_seconds: None,
                    prompt_key: Some(session.to_string()),
                },
                None,
            )
        };

        // The messages are the same for every attempt; only the draw changes. Built once so a retry
        // cannot quietly reorder or reformat the prompt it is retrying.
        let messages = match instruction {
            // The instruction is a **user** message at the very end, never a system section:
            // appended, everything before it stays byte-identical to the round's own request.
            // Merged into the trailing user message when there is one, so two consecutive user
            // messages never reach a provider.
            Some(instruction) => {
                let mut messages = messages;
                context::push_prepared_user(
                    &mut messages,
                    vec![ContentPart::Text(TextPart {
                        text: instruction,
                        raw: None,
                        truncated: false,
                    })],
                );
                messages
            }
            None => messages,
        };

        // Every attempt is a real request against a real model, so each gets **its own round id**
        // and its own usage row — a retry that went unrecorded would make `TurnStats.cost` smaller
        // than the invoice.
        let mut summary_tokens = None;
        let mut failure: Option<Unusable> = None;
        let mut written: Option<String> = None;

        for attempt in 1..=SUMMARY_ATTEMPTS {
            let round_id = RoundId::new();
            let request = LlmRequest {
                model: aux.model.wire_model.clone(),
                system: system.clone(),
                messages: messages.clone(),
                tools: request_tools.clone(),
                cache: cache.clone(),
                thinking: ThinkingIntent {
                    mode: ThinkingMode::Off,
                    ..Default::default()
                },
                params: aux.model.default_params.clone(),
                response_format: None,
                meta: RequestMeta {
                    session_id: session.to_string(),
                    turn_id: turn_id.to_string(),
                    round_id: round_id.to_string(),
                    purpose: Purpose::Compaction,
                },
            };

            let mut stream = aux.client.stream(request).await?;
            let mut text = String::new();
            // A reply cut off by the output limit is not a summary; the flag travels on the part
            // itself (see `PartEmitter::finish`), so this is read, never guessed from the text.
            let mut truncated = false;
            // The riding shape sends the conversation's tool definitions, so a model that ignores
            // the instruction can answer with a call and no text at all.
            let mut tool_calls = 0u32;

            while let Some(event) = stream.next().await {
                match event? {
                    // Only finished text counts. Reasoning is not part of the summary, and deltas
                    // are for rendering — this call has nothing to render.
                    LlmEvent::PartEnd {
                        part: ContentPart::Text(t),
                        ..
                    } => {
                        truncated |= t.truncated;
                        text.push_str(&t.text);
                    }
                    LlmEvent::PartEnd {
                        part: ContentPart::ToolCall(_),
                        ..
                    } => tool_calls += 1,
                    LlmEvent::Usage(report) => {
                        summary_tokens = Some(report.tokens.output);
                        self.record_aux_usage(
                            &aux,
                            turn_id,
                            round_id,
                            &report,
                            plan.budget.as_ref(),
                        )?;
                    }
                    LlmEvent::Notice { code, message } => {
                        emitter.notice(
                            NoticeLevel::Warn,
                            &code,
                            message,
                            std::collections::BTreeMap::new(),
                        );
                    }
                    _ => {}
                }
            }

            match usable_summary(&text, truncated, tool_calls) {
                Ok(summary) => {
                    written = Some(summary);
                    break;
                }
                // Regenerate rather than keep something that is not a summary: a half-summary
                // silently replaces the covered turns with an incomplete account of them, and
                // "which part is missing" is not a question anyone can answer later.
                Err(reason) if attempt < SUMMARY_ATTEMPTS => {
                    tracing::info!(
                        target: "zlogic::core",
                        attempt,
                        code = reason.code,
                        "the summary attempt was unusable; regenerating"
                    );
                }
                Err(reason) => failure = Some(reason),
            }
        }

        let Some(content) = written else {
            // Giving up is reported, not papered over: the covered turns stay untouched and the
            // conversation is left exactly as it was.
            let reason = failure.expect("the loop either writes a summary or records a failure");
            emitter.notice(
                NoticeLevel::Warn,
                reason.code,
                format!(
                    "no summary was written: the summary call {} on all {SUMMARY_ATTEMPTS} \
                     attempts, so the conversation was left uncompacted",
                    reason.reason
                ),
                std::collections::BTreeMap::new(),
            );
            return Ok(false);
        };

        let summary = Summary {
            from_turn: from,
            to_turn: to,
            content,
            reason,
            model_ref: Some(aux.model_key()),
            summary_tokens,
        };
        self.append(NewEntry::new(
            session,
            turn_id,
            turn_seq,
            EntryKind::Compaction,
            serde_json::to_value(&summary)?,
        ))?;

        // A post-condition that compaction actually shrinks the request. If even the floor exceeds
        // the window, the protected tail itself is the problem — retrying would resend the same
        // oversized tail again, so this is a hard error rather than a silent retry.
        let remaining = self
            .services()
            .store
            .with(|db| db.usage().last_conversation_input_tokens(session))?
            .map(|last| {
                remaining_after_compaction(last, chars, summary_tokens, summary.content.len())
            })
            .unwrap_or(0);
        let window = plan.model.context_window;
        if remaining >= window {
            return Err(CoreError::ContextUncompressible(format!(
                "the conversation cannot be compacted below the model window: even after \
                 replacing turns {from}..={to} with a summary, the remaining context is \
                 estimated at {remaining} tokens against a {window} token window. The most \
                 recent `tail_turns` themselves exceed the window; raise `tail_turns`, or \
                 trim the tail (e.g. a single oversized tool result) before continuing."
            )));
        }

        emitter.send(StreamPayload::CompactionEnd {
            replaces: (from as u32, to as u32),
            summary: summary.content.clone(),
            summary_tokens,
        });
        Ok(true)
    }

    /// Records usage for an auxiliary call.
    /// Its own `purpose` is what keeps it out of the compaction trigger: that reads only
    /// `Purpose::Main`, so a summary's own token count can never make the next turn compact again.
    /// **It also charges the turn.** A summary is a real request against a real model, and it fires
    /// precisely on the long conversations where it costs the most; leaving it out of the turn's
    /// tally made `TurnStats.cost` smaller than the invoice and let `per_turn` be exceeded by an
    /// arbitrary amount without a single checkpoint noticing.
    pub(crate) fn record_aux_usage(
        &self,
        aux: &AuxModel,
        turn_id: TurnId,
        round_id: RoundId,
        report: &zlogic_protocol::usage::UsageReport,
        budget: Option<&std::sync::Arc<crate::budget::TurnBudget>>,
    ) -> Result<()> {
        let cost = cost::cost_of(report, aux.model.pricing.as_ref());
        let mut usage =
            zlogic_store::NewUsage::new(self.session_id(), Purpose::Compaction, report.tokens)
                .in_round(turn_id, round_id);
        usage.model_ref = Some(aux.model_key());
        if let Some(c) = &cost {
            usage = usage.with_cost(c.amount, &c.currency, c.source);
            if let Some(budget) = budget {
                budget.add_aux(c);
            }
        }
        self.services()
            .store
            .with(|db| db.usage().upsert_round(usage))?;
        Ok(())
    }
}

/// Whether an error is the one thing compaction can actually fix.
pub(crate) fn is_context_overflow(err: &CoreError) -> bool {
    matches!(
        err,
        CoreError::Llm(e)
            if e.kind == zlogic_protocol::llm::LlmErrorKind::ContextLengthExceeded
    )
}

/// Whether a message is a rendered summary rather than something the user said.
pub fn is_summary_message(m: &Message) -> bool {
    m.role == Role::User
        && m.content.iter().any(|p| match p {
            ContentPart::Text(t) => t.text.starts_with("<conversation_summary"),
            _ => false,
        })
}

fn contains_image(messages: &[Message]) -> bool {
    messages.iter().any(|message| {
        message
            .content
            .iter()
            .any(|part| matches!(part, ContentPart::Image(_)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(from: i64, to: i64) -> Summary {
        Summary {
            from_turn: from,
            to_turn: to,
            content: format!("turns {from} to {to}"),
            reason: CompactionReason::Threshold,
            model_ref: None,
            summary_tokens: None,
        }
    }

    #[test]
    fn a_range_is_inclusive_at_both_ends() {
        let s = summary(3, 5);
        assert!(!s.covers(2));
        assert!(s.covers(3) && s.covers(4) && s.covers(5));
        assert!(!s.covers(6));
        assert_eq!(s.turns(), 3);
    }

    #[test]
    fn the_threshold_prefers_an_explicit_token_count() {
        let p = ContextPolicy {
            compact_ratio: 0.8,
            ..Default::default()
        };
        assert_eq!(p.threshold(200_000, None), 160_000);
        assert_eq!(p.threshold(200_000, Some(120_000)), 120_000);
    }

    /// f32 arithmetic on a large window truncates if it is not rounded: 0.8 × 128000 came out as
    /// 102399 in an earlier version.
    #[test]
    fn the_threshold_rounds_rather_than_truncates() {
        let p = ContextPolicy {
            compact_ratio: 0.7,
            ..Default::default()
        };
        assert_eq!(p.threshold(128_000, None), 89_600);
    }

    #[test]
    fn the_summary_reaches_the_model_as_a_tagged_user_message() {
        let m = summary(1, 6).to_message();
        assert_eq!(m.role, Role::User);
        assert!(
            m.source.is_none(),
            "a summary has no model to stamp for the raw gate"
        );
        assert!(is_summary_message(&m));
        match &m.content[0] {
            ContentPart::Text(t) => {
                assert!(t.text.contains("turns=\"1-6\""));
                assert!(t.text.contains("turns 1 to 6"));
            }
            other => panic!("{other:?}"),
        }
    }

    /// A minimal resolved model for the shape predicate: only `provider:model` and the client
    /// matter to it.
    fn test_model(model_id: &str) -> zlogic_protocol::config::ResolvedModel {
        use zlogic_protocol::config::{ClientSpec, ResolvedModel, Sdk};
        use zlogic_protocol::message::Source;
        ResolvedModel {
            source: Source::new("mock", model_id),
            wire_model: model_id.into(),
            display_name: model_id.into(),
            client: ClientSpec::Builtin {
                sdk: Sdk::Anthropic,
            },
            base_url: None,
            wiring: Default::default(),
            network: Default::default(),
            credential_refs: Vec::new(),
            context_window: 100_000,
            max_output_tokens: None,
            compaction_threshold: None,
            capabilities: Default::default(),
            pricing: None,
            default_params: Default::default(),
            config_revision: 1,
        }
    }

    /// The requirements live in one place and reach the model through **both** prompts. The
    /// standalone shape is the one that would quietly rot if they were written twice.
    #[test]
    fn both_prompts_carry_the_same_requirements() {
        for prompt in [standalone_system(), prefix_instruction()] {
            assert!(prompt.contains("What the user asked for"), "{prompt}");
            assert!(prompt.contains("The decisions taken"), "{prompt}");
            assert!(
                prompt.contains("file paths, symbol names, commands"),
                "{prompt}"
            );
            assert!(
                prompt.contains("Images and other visual attachments"),
                "{prompt}"
            );
            assert!(prompt.contains("will not be replayed"), "{prompt}");
            assert!(prompt.contains("Do not merely write"), "{prompt}");
            assert!(prompt.contains(REPLY_DISCIPLINE), "{prompt}");
        }
    }

    /// **The reply is asked to be the summary, and nothing else.** Saying it in the prompt is what
    /// keeps `extract_summary` from having to guess at prose — and a preamble is not free: it is
    /// replayed in the conversation's place on every later request.
    #[test]
    fn both_prompts_ask_for_the_text_alone() {
        for prompt in [standalone_system(), prefix_instruction()] {
            assert!(prompt.contains("nothing else"), "{prompt}");
            assert!(
                prompt.contains("Here is the summary:"),
                "the ban names the phrasing it bans, in both languages the conversation may be in: \
                 {prompt}"
            );
            assert!(
                prompt.contains("To sum up,"),
                "and the ban names more than one of the phrasings it bans: {prompt}"
            );
            assert!(prompt.contains("no closing remark"), "{prompt}");
            assert!(prompt.contains("no code fence"), "{prompt}");
        }
    }

    /// What one attempt produced decides whether another is made, and a reply that is not a summary
    /// is never accepted as one.
    #[test]
    fn an_unusable_reply_is_never_taken_for_a_summary() {
        let reason = |text: &str, truncated: bool, tool_calls: u32| {
            usable_summary(text, truncated, tool_calls)
                .expect_err("must not be accepted")
                .code
        };

        // Cut off, however much text arrived before the limit.
        assert_eq!(reason("## Requirements", true, 0), "compaction_truncated");
        // Nothing at all.
        assert_eq!(reason("   ", false, 0), "compaction_empty");
        // A tool call and no text: the riding shape sends tool definitions, so this is reachable.
        assert_eq!(reason("", false, 1), "compaction_tool_call");
        // Truncation outranks the others — it is the one with a specific remedy.
        assert_eq!(reason("", true, 1), "compaction_truncated");

        assert_eq!(
            usable_summary("## Requirements", false, 0).ok().as_deref(),
            Some("## Requirements")
        );
    }

    /// The instruction names no turn number, and describes no boundary: everything the model can
    /// see is being replaced, so there is nothing for it to exclude.
    #[test]
    fn the_instruction_has_no_boundary_to_describe() {
        let text = prefix_instruction();
        assert!(!text.contains("turn"), "{text}");
        assert!(text.contains("Summarise the conversation above"), "{text}");
        assert!(
            text.contains("when in doubt") && text.contains("include it"),
            "it should lean towards keeping too much, not too little: {text}"
        );
        assert!(text.contains("Do not call tools"), "{text}");
    }

    /// Only the conversation's own model may ride the prefix, and only the conversation's own
    /// system prompt makes the prefix match. A wrong shape here is a silent full-price re-bill,
    /// so the predicate is pinned rather than left to the request builder.
    #[test]
    fn only_the_conversations_own_model_over_a_known_system_prompt_rides_the_prefix() {
        use zlogic_llm::mock::{MockClient, MockScript};
        let plan = |sections: Vec<String>, model_id: &str| {
            let mut plan = crate::TurnPlan::new(
                test_model(model_id),
                std::sync::Arc::new(MockClient::new(MockScript::text("x"))),
            )
            .with_system(sections);
            plan.compaction = Some(crate::AuxModel {
                model: test_model(model_id),
                client: std::sync::Arc::new(MockClient::new(MockScript::text("x"))),
            });
            plan
        };

        for reason in [CompactionReason::Threshold, CompactionReason::Manual] {
            let p = plan(vec!["you are a test".into()], "m1");
            assert!(
                rides_the_prefix(&p, &p.compaction_model(), reason),
                "{reason:?}: same model, known system prompt"
            );
            // No system prompt handed over: the prefix diverges before the first message.
            let p = plan(Vec::new(), "m1");
            assert!(!rides_the_prefix(&p, &p.compaction_model(), reason));
        }

        // A different model has a different cache.
        let mut p = plan(vec!["you are a test".into()], "m1");
        p.compaction = Some(crate::AuxModel {
            model: test_model("m-small"),
            client: std::sync::Arc::new(MockClient::new(MockScript::text("x"))),
        });
        assert!(!rides_the_prefix(
            &p,
            &p.compaction_model(),
            CompactionReason::Threshold
        ));

        // Overflow is the one trigger where sending the request that did not fit again cannot work.
        let p = plan(vec!["you are a test".into()], "m1");
        assert!(!rides_the_prefix(
            &p,
            &p.compaction_model(),
            CompactionReason::ContextOverflow
        ));
    }

    /// **No format is imposed on the reply, and nothing is parsed out of it.** A wrapper that
    /// covers the whole answer is unwrapped because it is mechanical; prose the model wrote around
    /// the summary is left alone, because removing it would mean guessing at prose.
    #[test]
    fn the_reply_is_used_as_written_except_for_a_wrapper_around_all_of_it() {
        // A fence around the whole answer: three stray backticks and an indented body otherwise.
        assert_eq!(
            extract_summary("```markdown\n## Requirements\n- Fix the login\n```").as_deref(),
            Some("## Requirements\n- Fix the login")
        );
        assert_eq!(extract_summary("```\nplain\n```").as_deref(), Some("plain"));
        // A tag pair around the whole answer.
        assert_eq!(
            extract_summary("<summary>content</summary>").as_deref(),
            Some("content")
        );

        // A chatty preamble the model wrote itself stays: it costs a few tokens, and the heuristic
        // that would strip it also eats a summary's own first heading.
        assert_eq!(
            extract_summary("Here is the summary:\n\n## Requirements\n- Fix the login").as_deref(),
            Some("Here is the summary:\n\n## Requirements\n- Fix the login")
        );

        // Prose that merely *contains* a fence is not a wrapped answer — stripping here would take
        // the summary's own code sample apart.
        let with_code =
            "The change is:\n\n```rust\nfn main() {}\n```\n\nTests have not been run yet.";
        assert_eq!(extract_summary(with_code).as_deref(), Some(with_code));

        // Text around the fence means the fence is not the whole answer.
        assert_eq!(
            extract_summary("```\ncode\n```\nmore").as_deref(),
            Some("```\ncode\n```\nmore")
        );
        // A single tag is not a pair.
        assert_eq!(
            extract_summary("<summary>dangling").as_deref(),
            Some("<summary>dangling")
        );

        assert_eq!(extract_summary("   \n\t "), None, "whitespace is nothing");
        assert_eq!(extract_summary(""), None);
        assert_eq!(
            extract_summary("```\n```"),
            None,
            "an empty wrapper is nothing"
        );
    }

    #[test]
    fn a_contained_summary_is_superseded() {
        let outer = summary(1, 12);
        let inner = summary(1, 6);
        assert!(outer.contains_range(&inner));
        assert!(!inner.contains_range(&outer));
        // Chained ranges do not contain one another.
        assert!(!summary(1, 6).contains_range(&summary(7, 12)));
    }

    /// Manual compaction runs as its own turn, so the summary it writes gets a `turn_seq` of its
    /// own. If those count when the tail is measured, the boundary walks forward once per run:
    /// after four of them, `newest - tail_turns` lands on the newest real turn and the next
    /// compaction flattens the conversation the tail was protecting.
    #[test]
    fn a_summary_entry_does_not_count_as_a_turn_when_the_tail_is_measured() {
        use zlogic_protocol::{TurnId, WorkspaceId};
        use zlogic_store::{Db, NewEntry, NewSession};

        let db = Db::open_in_memory().unwrap();
        let session = db
            .sessions()
            .create(NewSession::root(WorkspaceId::new()))
            .unwrap()
            .session_id;
        let turn = TurnId::new();
        let entries = db.entries();
        for turn_seq in 1..=10 {
            entries
                .append(NewEntry::new(
                    session,
                    turn,
                    turn_seq,
                    EntryKind::User,
                    serde_json::json!({ "type": "text", "text": "hi" }),
                ))
                .unwrap();
        }
        // Four manual compactions have already run: turns 1-6, then 7, 8, 9 one at a time.
        let summaries = vec![summary(1, 6), summary(7, 7), summary(8, 8), summary(9, 9)];
        for (index, written) in summaries.iter().enumerate() {
            entries
                .append(NewEntry::new(
                    session,
                    TurnId::new(),
                    11 + index as i64,
                    EntryKind::Compaction,
                    serde_json::to_value(written).unwrap(),
                ))
                .unwrap();
        }

        let all = entries.list_for_context(session).unwrap();
        assert_eq!(
            plan_range(&all, &summaries, 4),
            None,
            "turns 7..10 are the protected tail; counting the four summary entries as turns \
             yielded Some((10, 10)) and summarised the newest real turn"
        );
    }

    #[test]
    fn only_a_contiguous_summary_prefix_can_trim_the_store_query() {
        let summary = |from_turn, to_turn| Summary {
            from_turn,
            to_turn,
            content: String::new(),
            reason: CompactionReason::Manual,
            model_ref: None,
            summary_tokens: None,
        };
        assert_eq!(covered_prefix_end(&[summary(1, 3), summary(4, 8)]), Some(8));
        assert_eq!(covered_prefix_end(&[summary(2, 8)]), None);
        assert_eq!(covered_prefix_end(&[summary(1, 3), summary(5, 8)]), Some(3));
    }
}
