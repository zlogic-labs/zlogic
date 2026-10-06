//! Small, non-conversational model calls: automatic session titles, and — through
//! [`Auxiliary::ask`] — the prompts the closed half of the product sends itself.
//!
//! These calls intentionally bypass `Core` and `Dispatcher`: they have no transcript, global
//! system prompt, tools, mailbox entry, or visible turn. Only the task-specific user message is
//! sent. Titles are part of the engine because the turn path drafts them; the closed half uses the
//! same path for its own prompts instead of opening a second one.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};

use futures_util::StreamExt;
use serde_json::json;
use tokio::sync::mpsc;
use zlogic_core::SharedStore;
use zlogic_protocol::llm::{
    CacheSpec, LlmEvent, LlmRequest, PartKind, RequestMeta, ResponseFormat, SystemPart,
    ThinkingIntent, ThinkingMode,
};
use zlogic_protocol::message::{ContentPart, Message, TextPart};
use zlogic_protocol::query::{ApiError, TextDelta};
use zlogic_protocol::stream::{StateChange, StateNotice};
use zlogic_protocol::usage::Purpose;
use zlogic_protocol::{RoundId, SessionId, TurnId};
use zlogic_store::{NewUsage, TitleSource};

use crate::hub::EventHub;
use crate::router::{ModelRouter, Routed};
use crate::{EngineError, Result};

/// The request every auxiliary call sends: one user message, no tools, no transcript.
///
/// **No turn.** The ids only correlate the request in the provider log; the usage row
/// deliberately stores no `turn_id` — see [`NewUsage::in_detached_round`].
fn auxiliary_request(
    session_id: SessionId,
    round_id: RoundId,
    purpose: Purpose,
    routed: &Routed,
    system: Vec<SystemPart>,
    prompt: String,
    max_tokens: Option<u64>,
    response_format: Option<ResponseFormat>,
) -> LlmRequest {
    let mut params = routed.model.default_params.clone();
    if let Some(max_tokens) = max_tokens {
        params.insert("max_tokens".into(), json!(max_tokens));
    }
    LlmRequest {
        model: routed.model.wire_model.clone(),
        // Supplied by this auxiliary task only; the agent's global system prompt is absent.
        system,
        messages: vec![Message::user(vec![ContentPart::Text(TextPart {
            text: prompt,
            raw: None,
            truncated: false,
        })])],
        // They also cannot act. In particular, commit-message drafting must not run git commit.
        tools: Vec::new(),
        thinking: ThinkingIntent {
            mode: ThinkingMode::Off,
            ..Default::default()
        },
        params,
        response_format,
        cache: CacheSpec::off(),
        meta: RequestMeta {
            session_id: session_id.to_string(),
            turn_id: TurnId::new().to_string(),
            round_id: round_id.to_string(),
            purpose,
        },
    }
}

/// One auxiliary call's identity: which conversation it is charged to, and as what.
struct DetachedRound {
    session_id: SessionId,
    round_id: RoundId,
    purpose: Purpose,
}

/// Forward a completion's text to `sink` as it arrives, and account for it like any other
/// auxiliary round.
///
/// A part that streamed deltas is sent delta by delta; a provider that only reports the part
/// at its end is sent the whole part then. Reasoning is never forwarded — it is not something
/// to read aloud, and a call has no use for it.
async fn stream_into_sink(
    routed: &Routed,
    request: LlmRequest,
    sink: &mpsc::UnboundedSender<TextDelta>,
    store: &SharedStore,
    round: DetachedRound,
) -> Result<()> {
    let mut text_parts: HashMap<u32, bool> = HashMap::new();
    let mut saw_text = false;
    let mut stream = routed.client.stream(request).await.map_err(|error| {
        tracing::error!(
            target: "zlogic::auxiliary",
            purpose = %round.purpose.as_wire(),
            model = %routed.model_ref(),
            kind = ?error.kind,
            status = error.status,
            request_id = ?error.request_id,
            "auxiliary model call failed: {error}"
        );
        EngineError::Invalid(format!(
            "auxiliary model call failed (model {}): {}",
            routed.model_ref(),
            zlogic_llm::error::provider_text(&error.message)
        ))
    })?;
    while let Some(event) = stream.next().await {
        match event.map_err(|error| {
            tracing::error!(
                target: "zlogic::auxiliary",
                purpose = %round.purpose.as_wire(),
                model = %routed.model_ref(),
                kind = ?error.kind,
                status = error.status,
                request_id = ?error.request_id,
                "auxiliary model response failed: {error}"
            );
            EngineError::Invalid(format!(
                "auxiliary model response failed (model {}): {}",
                routed.model_ref(),
                zlogic_llm::error::provider_text(&error.message)
            ))
        })? {
            LlmEvent::PartStart { index, kind } => {
                text_parts.insert(index, kind == PartKind::Text);
            }
            LlmEvent::PartDelta { index, delta } => {
                if *text_parts.get(&index).unwrap_or(&false) {
                    saw_text = true;
                    if sink.send(Ok(delta)).is_err() {
                        // The caller stopped listening; there is nobody left to talk to.
                        return Ok(());
                    }
                }
            }
            LlmEvent::PartEnd { index, part } => {
                if let ContentPart::Text(text) = part {
                    let streamed = text_parts.insert(index, true).unwrap_or(false);
                    if !streamed {
                        saw_text = true;
                        if sink.send(Ok(text.text)).is_err() {
                            return Ok(());
                        }
                    }
                }
            }
            LlmEvent::Usage(report) => record_auxiliary_usage(store, &round, routed, report),
            _ => {}
        }
    }
    if !saw_text {
        return Err(EngineError::Invalid(format!(
            "auxiliary model returned an empty response (model: {})",
            routed.model_ref()
        )));
    }
    Ok(())
}

/// The usage half of an auxiliary round, shared by the collected and streamed paths so a
/// streamed call is billed exactly like a collected one.
fn record_auxiliary_usage(
    store: &SharedStore,
    round: &DetachedRound,
    routed: &Routed,
    report: zlogic_protocol::usage::UsageReport,
) {
    let mut usage = NewUsage::new(
        round.session_id.clone(),
        round.purpose.clone(),
        report.tokens,
    )
    .in_detached_round(round.round_id.clone());
    usage.model_ref = Some(routed.model_ref());
    if let Some(cost) = zlogic_core::cost::cost_of(&report, routed.model.pricing.as_ref()) {
        usage = usage.with_cost(cost.amount, &cost.currency, cost.source);
    }
    if let Err(error) = store.with(|db| db.usage().upsert_round(usage)) {
        tracing::warn!(
            target: "zlogic::auxiliary",
            "failed to persist auxiliary model usage: {error}"
        );
    }
}

pub struct Auxiliary {
    store: SharedStore,
    router: Arc<ModelRouter>,
    hub: Arc<EventHub>,
    title_in_flight: Mutex<HashSet<SessionId>>,
}

impl Auxiliary {
    pub fn new(store: SharedStore, router: Arc<ModelRouter>, hub: Arc<EventHub>) -> Self {
        Self {
            store,
            router,
            hub,
            title_in_flight: Mutex::new(HashSet::new()),
        }
    }

    /// Send one prompt through the auxiliary model path and return the assembled answer.
    ///
    /// This is the seam for calls that belong to the product rather than to the engine — a Git
    /// commit message, a background job draft — but must run on the engine's path: no transcript,
    /// no tools, no mailbox entry, no visible turn, and usage accounted like any other auxiliary
    /// call. Keeping the transport here is what stops a closed prompt from drifting into a second,
    /// incompatible model path.
    pub async fn ask(
        &self,
        session_id: SessionId,
        purpose: Purpose,
        model_ref: Option<&str>,
        system: Vec<SystemPart>,
        prompt: String,
        response_format: Option<ResponseFormat>,
    ) -> Result<String> {
        let routed = self.router.resolve(&purpose, model_ref)?;
        let max_tokens = routed.model.max_output_tokens;
        self.complete(
            session_id,
            purpose,
            &routed,
            system,
            prompt,
            max_tokens,
            response_format,
        )
        .await
    }

    /// The same path as [`Self::ask`], with the answer handed over as it arrives.
    ///
    /// A caller that speaks to a person cannot wait for the whole sentence before it starts
    /// reacting to the first half of one, so the deltas are the point. Everything else is
    /// deliberately identical to `ask`: same resolution, same detached-round accounting, no
    /// transcript, no tools.
    ///
    /// Returns a receiver of text pieces. An error inside the model call arrives as the last
    /// item, after whatever text had already been sent — a partial answer is still an answer.
    pub async fn ask_stream(
        &self,
        session_id: SessionId,
        purpose: Purpose,
        model_ref: Option<&str>,
        system: Vec<SystemPart>,
        prompt: String,
        max_tokens: Option<u64>,
    ) -> Result<mpsc::UnboundedReceiver<TextDelta>> {
        let routed = self.router.resolve(&purpose, model_ref)?;
        let max_tokens = max_tokens.or(routed.model.max_output_tokens);
        let (sender, receiver) = mpsc::unbounded_channel();
        let round = DetachedRound {
            session_id: session_id.clone(),
            round_id: RoundId::new(),
            purpose,
        };
        let request = auxiliary_request(
            session_id,
            round.round_id.clone(),
            round.purpose.clone(),
            &routed,
            system,
            prompt,
            max_tokens,
            None,
        );
        let store = self.store.clone();
        tokio::spawn(async move {
            let outcome = stream_into_sink(&routed, request, &sender, &store, round).await;
            if let Err(error) = outcome {
                let _ = sender.send(Err(ApiError::from(error)));
            }
        });
        Ok(receiver)
    }

    pub fn draft_session_title(&self, session_id: SessionId, text: &str) -> Result<()> {
        let max_chars = self.router.config().session.auto_title.max_chars;
        let title = title_line(text, max_chars);
        if title.is_empty() {
            return Ok(());
        }
        let session = self.store.with(|db| db.sessions().get(session_id))?;
        if session.title_source.is_some() {
            return Ok(());
        }
        let changed = self.store.with(|db| {
            db.sessions()
                .set_title(session_id, &title, TitleSource::Draft)
        })?;
        if changed {
            self.notify_title(session_id, None);
        }
        Ok(())
    }

    /// Refine a session title from the first real user message.
    /// A one-shot auxiliary request started at submission time: it does not wait for an assistant
    /// reply and never reads the transcript. The store's source guard is checked again after the
    /// await, so a racing user rename always wins.
    pub async fn generate_session_title(&self, session_id: SessionId, text: String) -> Result<()> {
        let config = self.router.config().session.auto_title.clone();
        if !config.enabled || text.trim().is_empty() {
            return Ok(());
        }
        let session = self.store.with(|db| db.sessions().get(session_id))?;
        if matches!(
            session.title_source,
            Some(TitleSource::Model | TitleSource::User)
        ) {
            return Ok(());
        }

        {
            let mut in_flight = self
                .title_in_flight
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if !in_flight.insert(session_id) {
                return Ok(());
            }
        }

        let result = self
            .generate_session_title_inner(session_id, &session, &config, &text)
            .await;
        self.title_in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&session_id);
        result
    }

    async fn generate_session_title_inner(
        &self,
        session_id: SessionId,
        session: &zlogic_store::SessionRecord,
        config: &zlogic_protocol::settings::AutoTitle,
        text: &str,
    ) -> Result<()> {
        let source: String = text.chars().take(config.source_chars).collect();
        let routed = self
            .router
            .resolve(&Purpose::Title, session.model_ref.as_deref())?;
        let title_max_tokens = routed.model.max_output_tokens;
        let response = self
            .complete(
                session_id,
                Purpose::Title,
                &routed,
                vec![SystemPart {
                    text: session_title_prompt(config.max_chars),
                    cache: false,
                }],
                source,
                title_max_tokens,
                Some(ResponseFormat::JsonSchema {
                    name: "session_title".into(),
                    schema: json!({
                        "type": "object",
                        "properties": {
                            "title": { "type": "string" }
                        },
                        "required": ["title"],
                        "additionalProperties": false
                    }),
                    strict: true,
                }),
            )
            .await?;
        let refined = parse_session_title(&response, config.max_chars)?;
        let changed = self.store.with(|db| {
            db.sessions()
                .set_title(session_id, &refined, TitleSource::Model)
        })?;
        if changed {
            self.notify_title(session_id, None);
        }
        Ok(())
    }

    async fn complete(
        &self,
        session_id: SessionId,
        purpose: Purpose,
        routed: &Routed,
        system: Vec<SystemPart>,
        prompt: String,
        max_tokens: Option<u64>,
        response_format: Option<ResponseFormat>,
    ) -> Result<String> {
        let round_id = RoundId::new();
        let mut request = auxiliary_request(
            session_id,
            round_id,
            purpose.clone(),
            routed,
            system,
            prompt,
            max_tokens,
            response_format,
        );

        let mut tried_without_format = false;
        loop {
            let stream = match routed.client.stream(request.clone()).await {
                Ok(stream) => stream,
                Err(error) => {
                    if !tried_without_format && request.response_format.is_some()
                    //&& response_format_rejected(&error.message)
                    {
                        tried_without_format = true;
                        request.response_format = None;
                        tracing::warn!(
                            target: "zlogic::auxiliary",
                            purpose = %purpose.as_wire(),
                            "provider rejected response_format, retrying without it: {error}"
                        );
                        continue;
                    }
                    tracing::error!(
                        target: "zlogic::auxiliary",
                        purpose = %purpose.as_wire(),
                        model = %routed.model_ref(),
                        kind = ?error.kind,
                        status = error.status,
                        request_id = ?error.request_id,
                        "auxiliary model call failed: {error}"
                    );
                    return Err(EngineError::Invalid(format!(
                        "auxiliary model call failed (model {}): {}",
                        routed.model_ref(),
                        zlogic_llm::error::provider_text(&error.message)
                    )));
                }
            };
            return self
                .collect_text(session_id, purpose, routed, round_id, stream)
                .await;
        }
    }

    async fn collect_text(
        &self,
        session_id: SessionId,
        purpose: Purpose,
        routed: &Routed,
        round_id: RoundId,
        mut stream: zlogic_llm::EventStream,
    ) -> Result<String> {
        let mut text = String::new();
        let mut saw_text_part = false;
        while let Some(event) = stream.next().await {
            match event.map_err(|error| {
                tracing::error!(
                    target: "zlogic::auxiliary",
                    purpose = %purpose.as_wire(),
                    model = %routed.model_ref(),
                    kind = ?error.kind,
                    status = error.status,
                    request_id = ?error.request_id,
                    "auxiliary model response failed: {error}"
                );
                EngineError::Invalid(format!(
                    "auxiliary model response failed (model {}): {}",
                    routed.model_ref(),
                    zlogic_llm::error::provider_text(&error.message)
                ))
            })? {
                LlmEvent::PartEnd {
                    part: ContentPart::Text(part),
                    ..
                } => {
                    saw_text_part = true;
                    text.push_str(&part.text);
                }
                LlmEvent::Usage(report) => {
                    let mut usage = NewUsage::new(session_id, purpose.clone(), report.tokens)
                        .in_detached_round(round_id);
                    usage.model_ref = Some(routed.model_ref());
                    if let Some(cost) =
                        zlogic_core::cost::cost_of(&report, routed.model.pricing.as_ref())
                    {
                        usage = usage.with_cost(cost.amount, &cost.currency, cost.source);
                    }
                    if let Err(error) = self.store.with(|db| db.usage().upsert_round(usage)) {
                        tracing::warn!(
                            target: "zlogic::auxiliary",
                            "failed to persist auxiliary model usage: {error}"
                        );
                    }
                }
                _ => {}
            }
        }
        if !saw_text_part && text.is_empty() {
            return Err(EngineError::Invalid(format!(
                "auxiliary model returned an empty response (model: {})",
                routed.model_ref()
            )));
        }
        Ok(text)
    }

    fn notify_title(&self, session_id: SessionId, turn_id: Option<TurnId>) {
        self.hub.notify(StateNotice {
            session_id: session_id.to_string(),
            turn_id: turn_id.map(|id| id.to_string()),
            change: StateChange::SessionMetaChanged,
        });
    }
}

/// Strip the fences and quoting a model likes to wrap a one-line answer in.
///
/// Shared with the closed half: its own drafting calls parse their answers with it.
pub fn clean_model_text(text: &str) -> String {
    let trimmed = text.trim();
    let without_fence = trimmed
        .strip_prefix("```")
        .and_then(|value| value.strip_suffix("```"))
        .unwrap_or(trimmed)
        .trim();
    without_fence
        .strip_prefix("text\n")
        .unwrap_or(without_fence)
        .trim()
        .trim_matches(['"', '\'', '`'])
        .trim()
        .to_string()
}

/// First non-empty line, unwrapped from quoting, cut to `max_chars` characters.
///
/// Shared with the closed half, which bounds a drafted background job title the same way.
pub fn title_line(text: &str, max_chars: usize) -> String {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .trim()
        .trim_matches(['"', '\'', '`', '#'])
        .trim()
        .chars()
        .take(max_chars)
        .collect()
}

fn session_title_prompt(max_chars: usize) -> String {
    format!(
        "Generate a concise, sentence-case title (3-7 words) that captures the main topic or goal \
         of this coding session. The title should be clear enough that the user recognizes the \
         session in a list. Use sentence case: capitalize only the first word and proper nouns. \
         Keep the title within {max_chars} characters. Return JSON with a single \"title\" field, \
         e.g. {{\"title\": \"Fix session title generation\"}}."
    )
}

fn parse_session_title(response: &str, max_chars: usize) -> Result<String> {
    let cleaned = clean_model_text(response);
    if cleaned.is_empty() {
        return Err(EngineError::Invalid(
            "the title model returned an empty response".into(),
        ));
    }
    let value: serde_json::Value = serde_json::from_str(&cleaned).map_err(|error| {
        EngineError::Invalid(format!(
            "the title model returned invalid JSON: {error} (raw response: {cleaned:?})"
        ))
    })?;
    let title = value
        .get("title")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let title = title_line(title, max_chars);
    if title.is_empty() {
        return Err(EngineError::Invalid(
            "the title model returned an empty title".into(),
        ));
    }
    Ok(title)
}

#[cfg(test)]
mod tests {
    use super::{clean_model_text, parse_session_title, session_title_prompt, title_line};

    #[test]
    fn cleans_fenced_commit_message_without_flattening_body() {
        assert_eq!(
            clean_model_text(
                "```text\nfix: keep commit drafting isolated\n\nDo not enqueue a turn.\n```"
            ),
            "fix: keep commit drafting isolated\n\nDo not enqueue a turn."
        );
    }

    #[test]
    fn title_uses_first_non_empty_line_and_unicode_character_limit() {
        assert_eq!(
            title_line("\n  \"Fixing commit messages\"  \nmore", 6),
            "Fixing"
        );
    }

    #[test]
    fn parses_structured_session_title() {
        assert_eq!(
            parse_session_title(r#"{"title":"Fix session title generation"}"#, 30).unwrap(),
            "Fix session title generation"
        );
    }

    #[test]
    fn empty_title_response_is_reported_as_empty_not_as_bad_json() {
        for empty in ["", "   ", "\n```\n```\n", "\"\"", "''"] {
            let error = parse_session_title(empty, 30).unwrap_err().to_string();
            assert!(
                error.contains("empty response"),
                "expected empty-response error for {empty:?}, got: {error}"
            );
            assert!(
                !error.contains("EOF") && !error.contains("invalid JSON"),
                "must not report bad JSON for {empty:?}, got: {error}"
            );
        }
    }

    #[test]
    fn non_json_title_response_is_reported_as_bad_json() {
        let error = parse_session_title("just some prose", 30)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("invalid JSON"),
            "expected invalid-JSON error, got: {error}"
        );
        assert!(
            error.contains("raw response"),
            "diagnostic should carry the raw text, got: {error}"
        );
    }

    #[test]
    fn title_prompt_requests_short_sentence_case_json() {
        let prompt = session_title_prompt(30);
        assert!(prompt.contains("3-7 words"));
        assert!(prompt.contains("sentence-case"));
        assert!(prompt.contains("single \"title\" field"));
        assert!(prompt.contains("within 30 characters"));
    }
}
