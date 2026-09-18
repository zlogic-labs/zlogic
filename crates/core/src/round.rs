//! One round: one request, its events, and the tool batch that follows.
//! # `PartEnd` is the only thing that writes to the database
//! Deltas are for rendering. They are lossy by design — an inline `<think>` tag split across two
//! chunks is corrected only when the part closes — so accumulating them and storing the result
//! would persist text the provider never sent. `PartEnd` carries the authoritative part, including
//! the provider-native `raw` payload, and that is what becomes an entry.
//! # Tool calls are stored when the model asks, not when they are answered
//! A crash between the two leaves a call with no result, which every provider rejects on replay.
//! The alternative — hold the call back until all its results exist — loses the calls entirely if
//! the process dies mid-batch, and the UI could not show what was running. So the write happens
//! immediately and `build_context` drops any group whose results are incomplete. One place handles
//! the broken shape, and nothing is lost from the timeline.
//! # The batch runs in parallel, up to a cap
//! A round's `tool_calls` execute concurrently (window = `Limits::max_parallel_tools`, 0 =
//! sequential). Two things stay deliberate rather than inherited:
//! - Tool outputs interleave on one stream per call id, which the UI already renders as separate
//!   cards. Results are bound to calls by `call_id` on every codec (`tool_call_id` /
//!   `tool_use_id` / `call_id`), so completion order never affects replay.
//! - **Approval stays one-at-a-time.** Only calls that reach "ask the user" queue on a round-level
//!   mutex; calls the deterministic rule layer settles directly never touch it, so execution is
//!   parallel exactly where it helps.
//! A running tool is never dropped mid-execution: cancelling only stops scheduling calls that have
//! not started (which then get a cancelled result so the group stays replayable) and lets in-flight
//! tools wind down on their own token, exactly as before.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures_util::{FutureExt, StreamExt};
use tokio_util::sync::CancellationToken;
use zlogic_hooks::{HookEvent, HookOutcome, HookRequest, PermissionDecision};
use zlogic_protocol::interaction::{InteractionDecision, InteractionRequest};
use zlogic_protocol::llm::{
    CacheSpec as LlmCacheSpec, FinishReason, LlmRequest, PartKind, RequestMeta,
};
use zlogic_protocol::message::{
    ContentPart, ReasoningPart, TextPart, ToolCall, ToolResultFile, ToolResultPart,
};
use zlogic_protocol::stream::{
    BlockFinal, BlockKind, CompactionReason, IncompleteReason, NoticeLevel, RoundOutcome,
    RoundStats, StreamPayload, ToolStats, ToolStatus, UiToolCall,
};
use zlogic_protocol::usage::{ContextUsage, TokenUsage, UsageReport};
use zlogic_protocol::{CallId, EntryId, RoundId, SessionId, TurnId};
use zlogic_store::{NewEntry, NewUsage, SharedStore};
use zlogic_tools::{
    InteractionPort, Materialized, Tool, ToolContent, ToolCtx, ToolError, ToolExecResult,
    ToolExecStatus, ToolMeta,
};

use crate::sink::display_to_wire;
use crate::{
    Core, CoreError, GrantSet, PolicyDecision, PolicyRequest, Result, TurnEmitter, TurnPlan,
    compact, context, cost, entry_data,
};

pub(crate) struct RoundCtx<'a> {
    pub core: &'a Core,
    pub plan: &'a TurnPlan,
    pub emitter: &'a Arc<TurnEmitter>,
    pub tools: &'a Materialized,
    pub grants: &'a GrantSet,
    pub turn_id: TurnId,
    pub turn_seq: i64,
    pub round_seq: u32,
    /// The turn's usage before this round, so the `Usage` event can show a running total.
    pub turn_usage: TokenUsage,
    /// The conversation's token. Forwarded unchanged to tools and sub-agents.
    pub cancel: &'a CancellationToken,
    /// Counted here rather than returned from `gate`, whose result is already "run it or don't" —
    /// threading two extra values back through four call layers would obscure that.
    /// Atomics rather than `Cell` because the round's future must stay `Send`: it is awaited from a
    /// sub-agent's boxed future, and a `&RoundCtx` crossing that boundary has to be `Sync`.
    pub asked: AtomicU32,
    pub wait_ms: AtomicU64,
    pub approval: Arc<tokio::sync::Mutex<()>>,
}

/// Core owns the durable half of every interaction. The engine port is only an in-memory answer
/// router; persisting in both places creates duplicate pending dialogs with different ids.
pub(crate) struct RoundInteractions {
    pub(crate) delegate: Arc<dyn InteractionPort>,
    pub(crate) store: SharedStore,
    pub(crate) emitter: Arc<TurnEmitter>,
    pub(crate) session_id: SessionId,
    pub(crate) turn_id: TurnId,
    pub(crate) turn_seq: i64,
    pub(crate) cancel: CancellationToken,
}

impl RoundInteractions {
    fn append(&self, entry: NewEntry) -> std::result::Result<(), String> {
        self.store
            .with(|db| db.entries().append(entry))
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    fn close(&self, interaction_id: &str, decision: &InteractionDecision) {
        let written = entry_data::interaction_response_entry(
            self.session_id,
            self.turn_id,
            self.turn_seq,
            interaction_id,
            decision,
        )
        .map_err(|error| error.to_string())
        .and_then(|entry| self.append(entry));
        if let Err(error) = written {
            tracing::warn!(target: "zlogic::core", "ack response could not be written to timeline: {error}");
        }
        self.emitter.send(StreamPayload::InteractionResolved {
            interaction_id: interaction_id.to_string(),
            decision: decision.clone(),
        });
    }
}

#[async_trait]
impl InteractionPort for RoundInteractions {
    async fn ask(
        &self,
        req: InteractionRequest,
    ) -> std::result::Result<InteractionDecision, String> {
        let interaction_id = req.interaction_id.clone();
        let entry = entry_data::interaction_request_entry(
            self.session_id,
            self.turn_id,
            self.turn_seq,
            &interaction_id,
            req.call_id.as_ref(),
            &req.body,
        )
        .map_err(|error| error.to_string())?;
        self.append(entry)?;

        self.emitter.send(StreamPayload::InteractionRequired {
            interaction_id: interaction_id.clone(),
            body: req.body.clone(),
        });

        let decision = tokio::select! {
            biased;
            () = self.cancel.cancelled() => Ok(InteractionDecision::Cancelled),
            decision = self.delegate.ask(req) => decision,
        };
        match decision {
            Ok(decision) => {
                self.close(&interaction_id, &decision);
                Ok(decision)
            }
            Err(error) => {
                self.close(&interaction_id, &InteractionDecision::Cancelled);
                Err(error)
            }
        }
    }
}

pub(crate) struct RoundResult {
    pub outcome: RoundOutcome,
    pub stats: RoundStats,
    /// The assistant text of this round.
    pub text: String,
    pub finish: FinishReason,
    /// How many summaries this round wrote (threshold plus any overflow retry).
    pub compactions: u32,
    /// The round stopped because the conversation was cancelled.
    /// Its own flag rather than a `RoundOutcome` variant: the outcome says what the *model* did
    /// (a final answer, tool calls, truncation), and cancellation can arrive during any of those.
    pub cancelled: bool,
    pub interactions: u32,
    pub interaction_wait_ms: u64,
}

impl RoundResult {
    /// Why a truncated round could not finish.
    pub fn incomplete_reason(&self) -> IncompleteReason {
        match self.finish {
            FinishReason::Length => IncompleteReason::MaxOutputTokens,
            FinishReason::ContentFilter => IncompleteReason::ContentFilter,
            _ => IncompleteReason::Refusal,
        }
    }
}

#[derive(Clone)]
struct RoundTimings {
    request_started_at: DateTime<Utc>,
    since_request: Instant,
    first_token_at: Option<Instant>,
    completed_at: Option<Instant>,
}

impl RoundTimings {
    fn started_now() -> Self {
        Self {
            request_started_at: Utc::now(),
            since_request: Instant::now(),
            first_token_at: None,
            completed_at: None,
        }
    }

    fn mark_first_token(&mut self) {
        if self.first_token_at.is_none() {
            self.first_token_at = Some(Instant::now());
        }
    }

    fn mark_completed(&mut self) {
        if self.completed_at.is_none() {
            self.completed_at = Some(Instant::now());
        }
    }

    fn to_utc(&self, instant: Instant) -> DateTime<Utc> {
        let elapsed = instant.duration_since(self.since_request);
        let elapsed = chrono::Duration::from_std(elapsed).unwrap_or_default();
        self.request_started_at + elapsed
    }

    fn first_token_utc(&self) -> Option<DateTime<Utc>> {
        self.first_token_at.map(|at| self.to_utc(at))
    }

    fn completed_utc(&self) -> Option<DateTime<Utc>> {
        self.completed_at.map(|at| self.to_utc(at))
    }
}

impl RoundCtx<'_> {
    fn interaction_port(&self, delegate: Arc<dyn InteractionPort>) -> Arc<dyn InteractionPort> {
        Arc::new(RoundInteractions {
            delegate,
            store: self.core.services().store.clone(),
            emitter: self.emitter.clone(),
            session_id: self.core.session_id(),
            turn_id: self.turn_id,
            turn_seq: self.turn_seq,
            cancel: self.cancel.clone(),
        })
    }

    async fn run_hook(
        &self,
        event: HookEvent,
        payload: serde_json::Value,
        matchers: BTreeMap<String, String>,
    ) -> HookOutcome {
        let Some(runner) = self.core.hooks() else {
            return HookOutcome::default();
        };
        let request = HookRequest {
            event,
            session_id: self.core.session_id().to_string(),
            cwd: self
                .core
                .exec_cwd()
                .unwrap_or_else(|_| self.core.root().to_path_buf()),
            payload,
            matchers,
        };
        let outcome = runner.run(&request, self.cancel).await;
        for warning in &outcome.warnings {
            self.emitter.notice(
                NoticeLevel::Warn,
                "hook_failed",
                warning.clone(),
                std::collections::BTreeMap::new(),
            );
        }
        outcome
    }

    pub(crate) async fn run(self) -> Result<RoundResult> {
        let round_id = RoundId::new();
        let started = Instant::now();

        // Nothing has been emitted yet, so a round cancelled here leaves no trace of having
        // started — which is what the user asked for.
        if self.cancel.is_cancelled() {
            return Ok(self.stopped(round_id, started, RoundStats::default()));
        }

        self.emitter.send(StreamPayload::RoundStart {
            round_id: round_id.to_string(),
            round_seq: self.round_seq,
            model: self.plan.model_ref(),
        });

        let mut compactions = 0;
        // The lagging threshold check. Deliberately before the request is built, so the summary is
        // already in the history the request is assembled from.
        if !self.cancel.is_cancelled() && self.core.compaction_due(self.plan)? {
            match self.compact(CompactionReason::Threshold).await {
                Ok(true) => compactions += 1,
                // A failed summary is not a failed turn when the trigger was only a threshold: the
                // request will very likely still fit. The one exception is a summary that proves
                // the tail itself cannot fit — retrying would resend the same oversized request.
                Err(e @ CoreError::ContextUncompressible(_)) => return Err(e),
                Err(e) => {
                    self.emitter.notice(
                        NoticeLevel::Warn,
                        "compaction_failed",
                        format!("could not compact the context: {e}"),
                        std::collections::BTreeMap::new(),
                    );
                    compactions += 0;
                }
                Ok(false) => {}
            }
        }

        // A mid-stream failure (the transport died after output had already started) used to be
        // terminal: the blocks it produced were already in the database and on the screen, so
        // re-sending the same request would duplicate them. Now it is retried **as a continuation**:
        // we flush whatever streamed in, re-issue with a freshly built context (which includes
        // what this round already committed), and give the attempt a fresh round identity so its UI
        // block ids (`round:index`) don't collide with a previous attempt's. The model carries on
        // from where it stopped instead of repeating, and each retry is announced with a persisted
        // notice so the user understands why the reply looks the way it does.
        const MAX_MIDSTREAM_RETRIES: usize = 2;

        let mut stats = RoundStats::default();
        let mut calls: Vec<ToolCall> = Vec::new();
        let mut text = String::new();
        let mut finish = FinishReason::Stop;
        // Parts the user has watched stream in but whose `PartEnd` has not arrived yet. Kept so an
        // interrupted stream (cancel, transport error) can still persist what was on screen — a
        // reply that visibly existed must not vanish when the session is reopened.
        let mut open_parts: BTreeMap<u32, (PartKind, String)> = BTreeMap::new();
        // Whether a `ResponseEnd` was actually received. If the stream closes without one, it was
        // truncated rather than finished, and counts as an interruption worth retrying.
        let mut got_response_end = false;
        // The round identity this attempt reports under. A retry gets a fresh one so its blocks
        // get unique ids and group separately from a previous attempt's.
        let mut response_round_id = round_id;
        let mut midstream_retries = 0usize;

        // Each iteration is one full attempt: resolve the stream, consume it, and — if it died
        // mid-way without a `ResponseEnd` — rebuild the request and go again.
        'attempt: loop {
            let mut stream = None;
            let mut attempts = 0;
            let mut timings = RoundTimings::started_now();
            let mut last_usage: Option<(
                RoundId,
                UsageReport,
                Option<zlogic_protocol::usage::CostView>,
            )> = None;
            while stream.is_none() && !self.cancel.is_cancelled() {
                let request = self.build_request(response_round_id)?;
                // The outer error is a handshake failure: nothing was produced, so the same request
                // may safely be sent again.
                let s = tokio::select! {
                    biased;
                    s = self.plan.client.stream(request) => s,
                    () = self.cancel.cancelled() => {
                        tracing::info!(
                            target: "zlogic::core",
                            turn_id = %self.turn_id,
                            turn_seq = self.turn_seq,
                            round_seq = self.round_seq,
                            phase = "connecting",
                            "round stopped by cancel (no stream yet)"
                        );
                        return Ok(self.stopped(response_round_id, started, stats));
                    }
                };
                match s {
                    Ok(s) => stream = Some(s),
                    Err(e) => {
                        let err = CoreError::from(e);
                        // The one failure compaction can actually fix. Anything else, including a
                        // second overflow, goes up.
                        if !compact::is_context_overflow(&err)
                            || attempts >= self.core.services().context.overflow_retries
                        {
                            return Err(err);
                        }
                        attempts += 1;
                        if self.compact(CompactionReason::ContextOverflow).await? {
                            compactions += 1;
                        } else {
                            // Nothing left to summarise, so retrying would send the same thing again.
                            return Err(err);
                        }
                    }
                }
            }
            // The only way out of the loop above without a stream is the cancel check.
            if stream.is_none() {
                return Ok(self.stopped(response_round_id, started, stats));
            }
            let mut stream = stream.expect("the loop only exits with a stream");

            // Race the token against the next chunk rather than checking between them: a model
            // part-way through a long answer can go seconds without producing one, and a "stop"
            // that waits on the provider is not stopping.
            // **The stream is biased first, on purpose.** When a chunk is already in hand and the
            // token is set, the chunk wins: the provider sent it, the user has already watched its
            // deltas appear, and its `raw` payload is something we paid for. Discarding it would
            // lose a block that visibly exists. Cancellation takes effect at the first moment the
            // stream has nothing ready — for real SSE that is milliseconds away.
            let (exit, exit_error) = loop {
                let event = tokio::select! {
                    biased;
                    event = stream.next() => match event {
                        // An error inside the stream interrupts this attempt. What already streamed
                        // to the screen must survive the reload — flushed before any retry.
                        Some(Err(e)) => break (AttemptExit::Interrupted, Some(e.into())),
                        Some(Ok(event)) => event,
                        // Stream closed without a `ResponseEnd`: truncated rather than finished.
                        None => break (AttemptExit::Interrupted, None),
                    },
                    () = self.cancel.cancelled() => {
                        tracing::info!(
                            target: "zlogic::core",
                            turn_id = %self.turn_id,
                            turn_seq = self.turn_seq,
                            round_seq = self.round_seq,
                            phase = "streaming",
                            "round stopped by cancel (mid-stream)"
                        );
                        break (AttemptExit::Cancelled, None);
                    }
                };
                match event {
                    zlogic_protocol::llm::LlmEvent::PartStart { index, kind } => {
                        timings.mark_first_token();
                        open_parts.insert(index, (kind, String::new()));
                        self.emitter.send(StreamPayload::BlockStart {
                            block_id: block_id(response_round_id, index),
                            index,
                            kind: block_kind(kind),
                        });
                    }
                    zlogic_protocol::llm::LlmEvent::PartDelta { index, delta } => {
                        if let Some((_, buffered)) = open_parts.get_mut(&index) {
                            buffered.push_str(&delta);
                        }
                        self.emitter.send(StreamPayload::BlockDelta {
                            block_id: block_id(response_round_id, index),
                            delta,
                        });
                    }
                    zlogic_protocol::llm::LlmEvent::ToolCallDetected {
                        index,
                        call_index,
                        name,
                        ..
                    } => {
                        self.emitter.send(StreamPayload::ToolDetected {
                            block_id: block_id(response_round_id, index),
                            call_index,
                            name,
                        });
                    }
                    zlogic_protocol::llm::LlmEvent::PartEnd { index, part } => {
                        open_parts.remove(&index);
                        // The authoritative value first: the UI replaces whatever the deltas built.
                        if let Some(block) = block_final(&part) {
                            self.emitter.send(StreamPayload::BlockEnd {
                                block_id: block_id(response_round_id, index),
                                block,
                            });
                        }
                        match &part {
                            ContentPart::ToolCall(group) => {
                                calls.extend(group.calls.iter().cloned())
                            }
                            ContentPart::Text(t) => text.push_str(&t.text),
                            _ => {}
                        }
                        let entry = entry_data::to_entry(
                            self.core.session_id(),
                            self.turn_id,
                            self.turn_seq,
                            Some(response_round_id),
                            &entry_data::Author::Model(self.plan.model.source.clone()),
                            part,
                        )?
                        .with_round_seq(self.round_seq);
                        self.core.append(entry)?;
                    }
                    zlogic_protocol::llm::LlmEvent::Usage(report) => {
                        let (tokens, cost) =
                            self.record_usage(response_round_id, &report, &timings)?;
                        stats.usage = tokens;
                        stats.cost = cost.clone();
                        last_usage = Some((response_round_id, report, cost));
                    }
                    zlogic_protocol::llm::LlmEvent::Notice { code, message } => {
                        self.emitter.notice(
                            NoticeLevel::Warn,
                            &code,
                            message,
                            std::collections::BTreeMap::new(),
                        );
                    }
                    zlogic_protocol::llm::LlmEvent::ResponseEnd { finish_reason } => {
                        got_response_end = true;
                        finish = finish_reason;
                        timings.mark_completed();
                        if let Some((round, report, cost)) = &last_usage {
                            self.persist_usage(*round, report, &timings, cost.as_ref())?;
                        }
                    }
                }
            };
            // Dropping the stream here is the whole of "stop reading from the provider" — the llm
            // layer has no cancellation API by design. What was already persisted stays.
            drop(stream);

            match exit {
                // The user stopped the round; the cancellation bookkeeping below still applies.
                AttemptExit::Cancelled => break 'attempt,
                AttemptExit::Interrupted => {
                    // The stream may have sent `ResponseEnd` then closed — that is a completion.
                    if got_response_end {
                        break 'attempt;
                    }
                    // The user watched some of this attempt stream in — persist it truncated (same
                    // rule as the terminal failure path) so the retry's freshly built context
                    // carries it and the model continues rather than repeating the opening.
                    self.flush_interrupted_parts(response_round_id, &mut open_parts, &mut text);
                    if midstream_retries >= MAX_MIDSTREAM_RETRIES {
                        return Err(exit_error.unwrap_or_else(interrupted_error));
                    }
                    midstream_retries += 1;
                    // Persisted notice so the user understands why the reply is being retried and
                    // that whatever they saw may be continued rather than repeated.
                    self.emitter.notice(
                        NoticeLevel::Warn,
                        "llm_interrupted_retry",
                        "The reply was interrupted mid-generation and is being continued automatically (retried {n} times so far). What you already saw stays; the model picks up from where it stopped.",
                        std::collections::BTreeMap::from([(
                            "n".to_string(),
                            serde_json::json!(midstream_retries),
                        )]),
                    );
                    // Fresh identity so this attempt's blocks get unique ids and group separately.
                    response_round_id = RoundId::new();
                    continue 'attempt;
                }
            }
        }

        // Asked after the loop rather than tracked inside it, because the loop can exit either way
        // round: cancelled while waiting for a chunk, or cancelled just after the last one arrived.
        // Both mean the same thing here.
        if self.cancel.is_cancelled() {
            // The part the user watched stream in has no `PartEnd` yet — persist it truncated, or
            // the visible half-reply vanishes when the session is reopened.
            self.flush_interrupted_parts(response_round_id, &mut open_parts, &mut text);
            // Any tool calls this round persisted have no results yet. Completing them keeps the
            // group replayable; leaving them would make `build_context` drop the whole group,
            // including the model's own reasoning, on the next turn.
            let tools = self.cancel_pending(response_round_id, &calls)?;
            stats.tools = tools;
            let mut result = self.stopped(response_round_id, started, stats);
            result.text = text;
            return Ok(result);
        }

        let outcome = match finish {
            // A truncated response ends the turn. Any tool-call part the client had open was
            // dropped rather than closed — half-parsed arguments are a guaranteed 400 on replay.
            FinishReason::Length | FinishReason::ContentFilter => RoundOutcome::Truncated,
            _ if !calls.is_empty() => {
                let (tools, stopped) = self.run_batch(response_round_id, &calls).await?;
                stats.tools = tools;
                if stopped {
                    let mut result = self.stopped(response_round_id, started, stats);
                    result.text = text;
                    return Ok(result);
                }
                // Checkpoint 1: every tool result of this batch is in the history now, so a user
                // message can be injected without landing between the calls and their results.
                self.core
                    .drain_steering(self.emitter, self.turn_id, self.turn_seq)
                    .await?;
                RoundOutcome::ToolCalls
            }
            // Checkpoint 2: the turn was about to end. A message that arrived while the model was
            // answering continues the *same* turn rather than starting a new one.
            _ if !self
                .core
                .drain_steering_or_close(self.emitter, self.turn_id, self.turn_seq)
                .await?
                .is_empty() =>
            {
                RoundOutcome::FinalAnswerWithMailbox
            }
            _ => RoundOutcome::FinalAnswer,
        };

        stats.duration_ms = self.execution_ms(started);
        self.emitter.send(StreamPayload::RoundEnd {
            round_id: response_round_id.to_string(),
            outcome,
            stats: stats.clone(),
        });

        Ok(RoundResult {
            outcome,
            stats,
            text,
            finish,
            compactions,
            cancelled: false,
            interactions: self.asked.load(Ordering::Relaxed),
            interaction_wait_ms: self.wait_ms.load(Ordering::Relaxed),
        })
    }

    /// Assembles the request from the history as it stands **now**.
    /// Called again after a compaction or a steering injection, rather than being built once: both
    /// of those change what the history says, and reusing a stale message list would send a request
    /// that does not match what was persisted.
    fn build_request(&self, round_id: RoundId) -> Result<LlmRequest> {
        let prepared = self.build_context()?;

        Ok(LlmRequest {
            model: self.plan.model.wire_model.clone(),
            system: context::build_system(self.plan.system.clone()),
            messages: prepared.messages,
            tools: self.tools.definitions(),
            cache: LlmCacheSpec {
                prompt_key: Some(self.core.session_id().to_string()),
                ..Default::default()
            },
            thinking: self.plan.thinking,
            params: self.plan.model.default_params.clone(),
            response_format: None,
            meta: RequestMeta {
                session_id: self.core.session_id().to_string(),
                turn_id: self.turn_id.to_string(),
                round_id: round_id.to_string(),
                purpose: self.plan.purpose.clone(),
            },
        })
    }

    fn build_context(&self) -> Result<context::PreparedContext> {
        let objects = self.core.services().objects.clone();
        let session = self.core.session_id();
        let target = self.plan.model.source.clone();

        self.core
            .services()
            .store
            .with(|db| -> Result<context::PreparedContext> {
                let entries = db.entries();
                let loader = context::store_loader(&entries, objects.as_ref());
                let compact_entries =
                    entries.list_kind(session, zlogic_store::EntryKind::Compaction)?;
                let summaries = compact::effective_summaries(&compact_entries, &loader)?;
                let list = match compact::covered_prefix_end(&summaries) {
                    Some(end) => entries.list_for_context_after(session, end)?,
                    None => entries.list_for_context(session)?,
                };
                let object_loader = context::object_loader(objects.as_ref());
                context::build_context(&list, &target, &loader, &object_loader)
            })
    }

    /// Normalised tokens in, a priced row and a `Usage` event out.
    fn record_usage(
        &self,
        round_id: RoundId,
        report: &UsageReport,
        timings: &RoundTimings,
    ) -> Result<(TokenUsage, Option<zlogic_protocol::usage::CostView>)> {
        let cost = cost::cost_of(report, self.plan.model.pricing.as_ref());
        self.persist_usage(round_id, report, timings, cost.as_ref())?;

        let session = self.core.session_id();
        let session_total = self
            .core
            .services()
            .store
            .with(|db| -> Result<TokenUsage> { Ok(db.usage().total_for_session(session)?) })?;

        let mut turn_total = self.turn_usage;
        turn_total.add(&report.tokens);

        self.emitter.send(StreamPayload::Usage {
            round_id: round_id.to_string(),
            round: report.tokens,
            turn: turn_total,
            session: session_total,
            context: ContextUsage {
                // The figure the provider just reported, not an estimate: it answers "roughly how
                // close to the compaction threshold" and nothing more.
                used: Some(report.tokens.input),
                window: self.plan.model.context_window,
            },
            cost: cost.clone(),
        });

        Ok((report.tokens, cost))
    }

    fn persist_usage(
        &self,
        round_id: RoundId,
        report: &UsageReport,
        timings: &RoundTimings,
        cost: Option<&zlogic_protocol::usage::CostView>,
    ) -> Result<()> {
        let mut usage = NewUsage::new(
            self.core.session_id(),
            self.plan.purpose.clone(),
            report.tokens,
        )
        .in_round(self.turn_id, round_id);
        usage.model_ref = Some(self.plan.model_key());
        if let Some(c) = cost {
            usage = usage.with_cost(c.amount, &c.currency, c.source);
        }
        if let Some(raw) = &report.raw {
            // Diagnostics only, and deliberately without columns of its own.
            usage = usage.with_metadata(raw.clone());
        }
        usage = usage.with_timing(
            Some(timings.request_started_at),
            timings.first_token_utc(),
            timings.completed_utc(),
        );

        self.core
            .services()
            .store
            .with(|db| db.usage().upsert_round(usage))?;
        Ok(())
    }

    async fn run_batch(&self, round_id: RoundId, calls: &[ToolCall]) -> Result<(ToolStats, bool)> {
        let max = self.core.services().limits.max_parallel_tools.max(1);
        let mut stats = ToolStats::default();
        let mut started = 0usize;
        let mut in_flight = futures_util::stream::FuturesUnordered::new();
        let mut first_err: Option<CoreError> = None;

        loop {
            while in_flight.len() < max
                && started < calls.len()
                && first_err.is_none()
                && !self.cancel.is_cancelled()
            {
                let i = started;
                started += 1;
                in_flight.push(async move { (i, self.run_one(round_id, &calls[i]).await) });
            }
            if in_flight.is_empty() {
                break;
            }
            let (_, result) = in_flight
                .next()
                .await
                .expect("loop only exits when in_flight is empty");
            match result {
                Ok(r) => stats.record(wire_status(r.status)),
                Err(e) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
            }
        }

        if started < calls.len() {
            let rest = self.cancel_pending(round_id, &calls[started..])?;
            stats.add(&rest);
        }

        if let Some(e) = first_err {
            return Err(e);
        }
        Ok((stats, self.cancel.is_cancelled()))
    }

    /// Completes calls that will not run, so their group stays replayable.
    fn cancel_pending(&self, round_id: RoundId, calls: &[ToolCall]) -> Result<ToolStats> {
        let mut stats = ToolStats::default();
        for call in calls {
            let result = ToolExecResult::cancelled(format!(
                "`{}` was not run: the user stopped the turn",
                call.name
            ));
            self.persist_result(round_id, call, &result, 0)?;
            self.emitter.send(StreamPayload::ToolExecEnd {
                call_id: call.id.clone(),
                status: ToolStatus::Cancelled,
                display: Vec::new(),
                summary: result.model_text(),
                duration_ms: 0,
            });
            stats.record(ToolStatus::Cancelled);
        }
        Ok(stats)
    }

    /// Persists text / reasoning the user watched stream in but whose `PartEnd` never arrived
    /// (cancel, transport error, an in-stream error frame). Same policy as the emitter's
    /// truncation path: stored `truncated`, **no `raw`** (an incomplete part must not be replayed
    /// as provider-native), and open tool-call fragments are dropped — half-parsed args are a
    /// guaranteed 400 on replay.
    /// Best-effort on purpose: this runs on the way out of an already-failing round, and turning
    /// "could not save the interrupted reply" into a second error would mask the first.
    fn flush_interrupted_parts(
        &self,
        round_id: RoundId,
        open_parts: &mut BTreeMap<u32, (PartKind, String)>,
        text: &mut String,
    ) {
        for (_, (kind, buffered)) in std::mem::take(open_parts) {
            if buffered.is_empty() {
                continue;
            }
            let part = match kind {
                PartKind::Text => {
                    text.push_str(&buffered);
                    ContentPart::Text(TextPart {
                        text: buffered,
                        raw: None,
                        truncated: true,
                    })
                }
                PartKind::Reasoning => ContentPart::Reasoning(ReasoningPart {
                    text: buffered,
                    raw: None,
                    truncated: true,
                }),
                PartKind::ToolCall => continue,
            };
            let appended = entry_data::to_entry(
                self.core.session_id(),
                self.turn_id,
                self.turn_seq,
                Some(round_id),
                &entry_data::Author::Model(self.plan.model.source.clone()),
                part,
            )
            .map(|entry| entry.with_round_seq(self.round_seq))
            .and_then(|entry| self.core.append(entry));
            if let Err(e) = appended {
                tracing::warn!(
                    target: "zlogic::core",
                    "interrupted part was not persisted: {e}"
                );
            }
        }
    }

    /// How long this round spent **working**, which is not how long it lasted.
    /// The approval gate awaits the user in the middle of the tool batch, so that wait sits inside
    /// the round's wall clock. It is reported separately (`TurnStats::interaction_wait_ms`), and
    /// `TurnStats::elapsed_ms` is defined as the sum of the two — so leaving it in here counts it
    /// twice and makes a turn look slow because somebody went to lunch with a prompt open.
    fn execution_ms(&self, started: Instant) -> u64 {
        (started.elapsed().as_millis() as u64).saturating_sub(self.wait_ms.load(Ordering::Relaxed))
    }

    /// A round that stopped early. Emits `RoundEnd` so a client is not left with an open round.
    fn stopped(&self, round_id: RoundId, started: Instant, mut stats: RoundStats) -> RoundResult {
        stats.duration_ms = self.execution_ms(started);
        self.emitter.send(StreamPayload::RoundEnd {
            round_id: round_id.to_string(),
            outcome: RoundOutcome::Paused,
            stats: stats.clone(),
        });
        RoundResult {
            outcome: RoundOutcome::Paused,
            stats,
            text: String::new(),
            finish: FinishReason::Other,
            compactions: 0,
            cancelled: true,
            interactions: self.asked.load(Ordering::Relaxed),
            interaction_wait_ms: self.wait_ms.load(Ordering::Relaxed),
        }
    }

    /// Compaction, but never left running after a cancel.
    /// Dropping the future is safe: nothing is written until the summary is complete, so a
    /// half-produced summary simply never exists.
    async fn compact(&self, reason: CompactionReason) -> Result<bool> {
        let before = self
            .run_hook(
                HookEvent::ContextCompactBefore,
                serde_json::json!({"reason": reason}),
                BTreeMap::new(),
            )
            .await;
        if before.blocked.is_some() {
            return Ok(false);
        }
        let written = tokio::select! {
            biased;
            () = self.cancel.cancelled() => Ok(false),
            written = self.core.compact(
                self.plan,
                self.tools,
                self.emitter,
                self.turn_id,
                self.turn_seq,
                reason,
            ) => written,
        }?;
        let _ = self
            .run_hook(
                HookEvent::ContextCompactAfter,
                serde_json::json!({"reason": reason, "written": written}),
                BTreeMap::new(),
            )
            .await;
        Ok(written)
    }

    /// Writes one tool result into the history.
    /// Shared with the cancellation path, which has to persist results for calls that never ran —
    /// two copies of this would eventually disagree about the entry's shape.
    fn persist_result(
        &self,
        round_id: RoundId,
        call: &ToolCall,
        result: &ToolExecResult,
        elapsed_ms: u64,
    ) -> Result<()> {
        let part = ContentPart::ToolResult(ToolResultPart {
            call_id: call.id.clone(),
            name: call.name.clone(),
            content: result.model_text(),
            files: result
                .content
                .iter()
                .filter_map(|content| match content {
                    ToolContent::File(file) => Some(ToolResultFile {
                        name: file.name().to_string(),
                        mime_type: file.mime_type().to_string(),
                        object_id: file.object_id().to_string(),
                        bytes: file.bytes(),
                    }),
                    ToolContent::Text(_) => None,
                })
                .collect(),
            is_error: result.is_error(),
        });
        let mut entry = entry_data::to_entry(
            self.core.session_id(),
            self.turn_id,
            self.turn_seq,
            Some(round_id),
            &entry_data::Author::Tool,
            part,
        )?
        .with_round_seq(self.round_seq)
        .with_display(entry_data::tool_display(result, elapsed_ms));

        // Structured file content is itself authoritative persistence metadata. Derive its object
        // references at the write boundary so a future tool cannot create a GC-dangling file merely
        // by forgetting to copy the third facet of `CapturedFile` into `result.objects`.
        for content in &result.content {
            if let ToolContent::File(file) = content {
                entry = entry.references(zlogic_objects::ObjectRef::keyed(
                    file.object_id().clone(),
                    zlogic_objects::ObjectRole::Output,
                    file.name(),
                ));
            }
        }
        for display in &result.display {
            let Some(object_id) = display.object_id() else {
                continue;
            };
            /* A widget is classified apart from other outputs, and carries the label and meta the
             * renderer needs (title, `libraries`, `height`): the carousel card needs them, and they
             * are only available right here on this card — re-parsing them back out of `display`
             * later is both slow and unreliable (older widget cards never landed in `display`). */
            if let zlogic_tools::ToolDisplay::Widget {
                title,
                height,
                libraries,
                ..
            } = display
            {
                entry = entry.references(zlogic_objects::ObjectRef::classified(
                    object_id.clone(),
                    zlogic_objects::ObjectRole::Output,
                    "widget",
                    Some(title.clone()),
                    Some(
                        serde_json::json!({ "height": height, "libraries": libraries }).to_string(),
                    ),
                ));
                continue;
            }
            let (role, key) = match display {
                zlogic_tools::ToolDisplay::Diff { path, .. } => {
                    (zlogic_objects::ObjectRole::Diff, Some(path.as_str()))
                }
                zlogic_tools::ToolDisplay::File { path, .. } => {
                    (zlogic_objects::ObjectRole::Output, Some(path.as_str()))
                }
                zlogic_tools::ToolDisplay::Output { .. } => {
                    (zlogic_objects::ObjectRole::Output, None)
                }
                zlogic_tools::ToolDisplay::Text { .. }
                | zlogic_tools::ToolDisplay::Table { .. }
                | zlogic_tools::ToolDisplay::Agent { .. }
                | zlogic_tools::ToolDisplay::Task { .. }
                | zlogic_tools::ToolDisplay::Widget { .. } => continue,
            };
            entry = entry.references(match key {
                Some(key) => zlogic_objects::ObjectRef::keyed(object_id.clone(), role, key),
                None => zlogic_objects::ObjectRef::new(object_id.clone(), role),
            });
        }
        for obj in &result.objects {
            entry = entry.references(obj.clone());
        }
        self.core.append(entry)?;
        if let Some(loaded) = &result.skill_load {
            let stamp = (loaded.revision.clone(), loaded.path.clone());
            let already_loaded = crate::steer::latest_skill_load_stamps(self.core)?
                .get(&loaded.name)
                == Some(&stamp);
            if !already_loaded {
                self.core.append(entry_data::skill_load_entry_from_body(
                    self.core.session_id(),
                    self.turn_id,
                    self.turn_seq,
                    loaded.name.clone(),
                    loaded.revision.clone(),
                    loaded.raw_body.clone(),
                    loaded.path.clone(),
                    zlogic_protocol::SkillLoadSource::Model,
                    loaded.unsupported.clone(),
                    self.core.services().objects.as_ref(),
                )?)?;
            }
        }
        // `load_tool`'s effect is session state, not just this turn's in-memory set: the next turn
        // materialises its registry from these rows, so the loaded tools stay available. Only the
        // names are recorded — the definitions are reconstructed from the registry by name.
        if !result.loaded_tools.is_empty() {
            self.core.append(entry_data::tool_load_entry(
                self.core.session_id(),
                self.turn_id,
                self.turn_seq,
                result.loaded_tools.clone(),
            )?)?;
        }
        Ok(())
    }

    async fn run_one(&self, round_id: RoundId, call: &ToolCall) -> Result<ToolExecResult> {
        self.emitter.send(StreamPayload::ToolExecStart {
            call_id: call.id.clone(),
            name: call.name.clone(),
        });

        let started = Instant::now();
        let result = self.execute(call).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;

        if !result.dangling_display_objects().is_empty() {
            // The persistence boundary repairs structured file references, but this still catches
            // malformed hand-built results (especially display-only objects) while the responsible
            // tool name is available.
            tracing::warn!(
                target: "zlogic::core",
                tool = %call.name,
                "display card references an object the result does not keep alive"
            );
        }

        self.persist_result(round_id, call, &result, elapsed_ms)?;

        self.emitter.send(StreamPayload::ToolExecEnd {
            call_id: call.id.clone(),
            status: wire_status(result.status),
            display: result.display.iter().map(display_to_wire).collect(),
            summary: result.model_text(),
            duration_ms: elapsed_ms,
        });

        Ok(result)
    }

    /// A tool panic, as a result the model can read instead of a turn that dies silently.
    /// The tool boundary is where untrusted data enters (web pages, MCP payloads, command output),
    /// so a parsing panic here must not unwind through the round. The panic payload is included —
    /// it is usually benign ("byte index out of bounds"), and naming it lets the model react.
    fn panic_result(call: &ToolCall, panic: Box<dyn std::any::Any + Send>) -> ToolExecResult {
        let payload = panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| {
                panic
                    .downcast_ref::<&'static str>()
                    .map(|s| (*s).to_string())
            })
            .unwrap_or_else(|| "(non-string panic)".to_string());
        ToolExecResult::failed(format!(
            "`{}` panicked while running and did not complete — this is a bug in the tool, \
             not a problem with the request. Panic message: {payload}",
            call.name
        ))
    }

    /// The gate, then the tool. Every failure path produces a result the model can read.
    async fn execute(&self, call: &ToolCall) -> ToolExecResult {
        let Some(tool) = self.tools.get(&call.name) else {
            // Never an `Err`: the model invented a name and can pick a real one if told which.
            return ToolExecResult::new(ToolExecStatus::PrecheckFailed).with_text(format!(
                "unknown tool `{}`. Available: {}",
                call.name,
                self.tools.visible_names().join(", ")
            ));
        };
        let meta = tool.meta();
        let mut effective = call.clone();
        let mut matchers = BTreeMap::new();
        matchers.insert("tool".into(), call.name.clone());
        matchers.insert("source".into(), meta.source.into());
        let before = self
            .run_hook(
                HookEvent::ToolExecuteBefore,
                serde_json::from_str(&call.args)
                    .unwrap_or_else(|_| serde_json::Value::String(call.args.clone())),
                matchers.clone(),
            )
            .await;
        if let Some(reason) = before.blocked {
            return ToolExecResult::denied(reason);
        }
        if let Some(input) = before.input {
            effective.args = input.to_string();
            self.emitter.notice(
                NoticeLevel::Info,
                "hook_modified_tool_input",
                format!("hook changed the input for `{}`", call.name),
                std::collections::BTreeMap::new(),
            );
        }

        if let Some(reason) = schema_violation(tool.as_ref(), &effective.args) {
            return ToolExecResult::new(ToolExecStatus::PrecheckFailed).with_text(format!(
                "invalid arguments for `{}`: {reason}",
                effective.name
            ));
        }

        match self.gate(&meta, &effective).await {
            Gate::Allow => {}
            Gate::Refused(reason) => return ToolExecResult::denied(reason),
            // Same shape as a tool's own `BadArgs` (see the `execute` outcome below): the model
            // wrote the call, so the parse error comes back to it and it can fix and retry.
            Gate::Rejected(reason) => {
                return ToolExecResult::new(ToolExecStatus::PrecheckFailed).with_text(format!(
                    "invalid arguments for `{}`: {reason}",
                    effective.name
                ));
            }
            // Not `Denied`: nobody refused this, the user stopped the turn while being asked.
            Gate::Cancelled => {
                return ToolExecResult::cancelled(format!(
                    "`{}` was not run: the turn was stopped while waiting for approval",
                    call.name
                ));
            }
        }

        let exec_cwd = match self.core.exec_cwd() {
            Ok(p) => p,
            Err(e) => {
                return ToolExecResult::failed(format!("cannot resolve a working directory: {e}"));
            }
        };

        let ctx = ToolCtx {
            exec_cwd,
            root: self.core.root().to_path_buf(),
            session_id: self.core.session_id(),
            turn_id: self.turn_id,
            call_id: CallId::new(&effective.id),
            objects: self.core.services().objects.clone(),
            spawner: self.core.spawner(),
            tasks: self.core.services().tasks.clone(),
            skills: self.core.skills(),
            worktree: self.core.worktree(),
            interaction: self
                .core
                .services()
                .interaction
                .clone()
                .map(|port| self.interaction_port(port)),
            output: Some(self.emitter.tool_output(&effective.id)),
            max_result_chars: self.core.services().limits.max_result_chars,
            runtime_paths: self
                .core
                .services()
                .runtime_paths
                .as_ref()
                .map(|provider| provider.bin_dirs())
                .unwrap_or_default(),
            // The same token, not a child: a tool that spawns work of its own must be stoppable by
            // the one thing the user pressed.
            cancel: self.cancel.clone(),
        };

        let limit = self.core.services().limits.tool_timeout_secs;
        let tool_fut =
            std::panic::AssertUnwindSafe(tool.execute(&ctx, &effective.args)).catch_unwind();
        let outcome = if limit == 0 {
            match tool_fut.await {
                Ok(r) => r,
                Err(panic) => Ok(Self::panic_result(&effective, panic)),
            }
        } else {
            match tokio::time::timeout(std::time::Duration::from_secs(limit), tool_fut).await {
                Ok(Ok(r)) => r,
                Ok(Err(panic)) => Ok(Self::panic_result(&effective, panic)),
                Err(_) => Ok(ToolExecResult::timeout(format!(
                    "`{}` exceeded its {limit}s budget and was stopped",
                    effective.name
                ))),
            }
        };

        let mut result = match outcome {
            Ok(r) => r,
            // Arguments the tool could not parse: the model wrote the call, so it can fix it.
            Err(ToolError::BadArgs(m)) => ToolExecResult::new(ToolExecStatus::PrecheckFailed)
                .with_text(format!("invalid arguments for `{}`: {m}", effective.name)),
            Err(e) => ToolExecResult::failed(format!("`{}` failed: {e}", effective.name)),
        };
        let event = if result.status == ToolExecStatus::Success {
            HookEvent::ToolExecuteAfter
        } else {
            HookEvent::ToolExecuteFailed
        };
        let after = self
            .run_hook(
                event,
                serde_json::json!({
                    "tool": effective.name,
                    "input": serde_json::from_str::<serde_json::Value>(&effective.args)
                        .unwrap_or_else(|_| serde_json::Value::String(effective.args.clone())),
                    "status": result.status,
                    "result": result.model_text(),
                }),
                matchers,
            )
            .await;
        if let Some(reason) = after.blocked {
            result = result.with_text(format!(
                "post-execution hook blocked continuation: {reason}"
            ));
        }
        for context in after.contexts {
            result = result.with_text(context);
        }
        result
    }

    async fn gate(&self, meta: &ToolMeta, call: &ToolCall) -> Gate {
        let request = PolicyRequest {
            session_id: self.core.session_id(),
            turn_id: self.turn_id,
            tool: meta.clone(),
            args: call.args.clone(),
            exec_cwd: self
                .core
                .exec_cwd()
                .unwrap_or_else(|_| self.core.root().to_path_buf()),
            root: self.core.root().to_path_buf(),
            budget: self.plan.budget.clone(),
        };

        let body = match tokio::select! {
            () = self.cancel.cancelled() => return Gate::Cancelled,
            decision = self.core.services().policy.evaluate(&request) => decision,
        } {
            PolicyDecision::Allow => return Gate::Allow,
            PolicyDecision::Deny { reason } => {
                let mut matchers = BTreeMap::new();
                matchers.insert("tool".into(), call.name.clone());
                let _ = self
                    .run_hook(
                        HookEvent::PermissionDenied,
                        serde_json::json!({"tool": call.name, "reason": reason}),
                        matchers,
                    )
                    .await;
                return Gate::Refused(reason);
            }
            // Unparseable arguments are the model's mistake, not a policy question for the user:
            // the parse error goes back to the model as a precheck failure, so it can fix the
            // JSON and retry. No `PermissionDenied` hook — nothing was refused.
            PolicyDecision::PrecheckFailed { reason } => return Gate::Rejected(reason),
            PolicyDecision::Ask { body } => {
                // Grants are deliberately consulted only after deterministic policy. A stale
                // session grant may settle an uncertain call, but it can never punch through a
                // newly-added hard deny.
                if self.grants.allows(&call.name, &call.args) {
                    return Gate::Allow;
                }
                body
            }
        };

        let mut permission_matchers = BTreeMap::new();
        permission_matchers.insert("tool".into(), call.name.clone());
        let permission = self
            .run_hook(
                HookEvent::PermissionRequested,
                serde_json::json!({"tool": call.name, "input": call.args}),
                permission_matchers,
            )
            .await;
        if let Some(reason) = permission.blocked {
            return Gate::Refused(reason);
        }
        match permission.permission {
            Some(PermissionDecision::Allow(_)) => return Gate::Allow,
            Some(PermissionDecision::Deny(reason)) => {
                return Gate::Refused(reason.unwrap_or_else(|| "refused by hook".into()));
            }
            None => {}
        }

        // No UI to ask means no approval. Inventing "yes" would make a batch run destructive.
        let Some(port) = self.core.services().interaction.clone() else {
            return Gate::Refused(format!(
                "`{}` needs approval and there is no interface attached to ask",
                call.name
            ));
        };

        let waited = Instant::now();
        let interaction_id = EntryId::new().to_string();
        let _approval = self.approval.lock().await;
        let decision = self
            .interaction_port(port)
            .ask(InteractionRequest {
                interaction_id,
                session_id: self.core.session_id(),
                turn_id: self.turn_id,
                call_id: Some(CallId::new(&call.id)),
                body,
            })
            .await;
        drop(_approval);
        // A user thinking is not the agent being slow, so this is tracked apart from execution.
        self.wait_ms
            .fetch_add(waited.elapsed().as_millis() as u64, Ordering::Relaxed);
        self.asked.fetch_add(1, Ordering::Relaxed);

        let decision = match decision {
            Ok(d) => d,
            // A broken transport is not an implicit "no" to the user, but it is to the call.
            Err(e) => {
                return Gate::Refused(format!("could not ask for approval: {e}"));
            }
        };

        match decision {
            InteractionDecision::Allow { scope, .. } => {
                self.grants.record(scope, &call.name, &call.args);
                if scope.is_durable() {
                    self.core
                        .services()
                        .policy
                        .record_grant(&request, scope)
                        .await;
                }
                Gate::Allow
            }
            InteractionDecision::Deny { reason } => {
                Gate::Refused(reason.unwrap_or_else(|| "refused by the user".into()))
            }
            InteractionDecision::Cancelled => Gate::Cancelled,
            // A form answer where an approval was asked for: treat as no rather than guess.
            InteractionDecision::Submitted(_) => {
                Gate::Refused("a form answer is not an approval".into())
            }
        }
    }
}

/// What the permission gate decided.
/// `Cancelled` is separate from `Refused` because the audit trail and the model are both misled by
/// conflating them: nobody refused the call, the user stopped the work.
enum Gate {
    Allow,
    Refused(String),
    /// Malformed call (unparseable arguments), rejected before running. Not a refusal: nobody
    /// denied it — the model gets the parse error back as a precheck failure and can fix it.
    Rejected(String),
    Cancelled,
}

fn schema_violation(tool: &dyn Tool, args: &str) -> Option<String> {
    let schema = &tool.definition().parameters;
    if schema.is_null() || schema.as_object().map_or(true, |object| object.is_empty()) {
        return None;
    }
    let instance = match serde_json::from_str::<serde_json::Value>(args) {
        Ok(value) => value,
        Err(error) => return Some(error.to_string()),
    };
    match jsonschema::validator_for(schema) {
        Ok(validator) => match validator.validate(&instance) {
            Ok(()) => None,
            Err(error) => Some(error.to_string()),
        },
        Err(error) => {
            tracing::warn!(
                target: "zlogic::core",
                tool = %tool.meta().name,
                "tool-declared JSON Schema could not compile, skipping argument validation for this call: {error}"
            );
            None
        }
    }
}

/// How one stream attempt exited, deciding whether the round may retry it.
/// A normal completion is not a distinct case: the codec always delivers
/// [`LlmEvent::ResponseEnd`] before the stream closes, so the `Interrupted` arm checks
/// `got_response_end` to tell "finished properly" apart from "died mid-way".
enum AttemptExit {
    /// The stream closed or errored before a [`LlmEvent::ResponseEnd`] was guaranteed.
    Interrupted,
    /// The user cancelled; the round stops without retrying.
    Cancelled,
}

/// Fallback error for an attempt that was interrupted without an `Err` surfacing (e.g. a silent
/// EOF with no `ResponseEnd` left a muddled end-of-round). Conventionally non-retryable: by the
/// time this is reached the retry budget is exhausted anyway.
fn interrupted_error() -> CoreError {
    CoreError::Llm(zlogic_protocol::llm::LlmError {
        kind: zlogic_protocol::llm::LlmErrorKind::Server,
        retryable: false,
        message: "response stream was interrupted before completing".into(),
        status: None,
        request_id: None,
    })
}

fn block_id(round: RoundId, index: u32) -> String {
    format!("{round}:{index}")
}

fn block_kind(kind: PartKind) -> BlockKind {
    match kind {
        PartKind::Reasoning => BlockKind::Reasoning,
        PartKind::Text => BlockKind::Text,
        PartKind::ToolCall => BlockKind::ToolCall,
    }
}

/// The UI's view of a finished part. **There is no `raw` field** — the UI cannot see provider
/// state because the type has nowhere to put it.
fn block_final(part: &ContentPart) -> Option<BlockFinal> {
    Some(match part {
        ContentPart::Reasoning(r) => BlockFinal::Reasoning {
            text: r.text.clone(),
            truncated: r.truncated,
        },
        ContentPart::Text(t) => BlockFinal::Text {
            text: t.text.clone(),
            truncated: t.truncated,
        },
        ContentPart::ToolCall(g) => BlockFinal::ToolCall {
            calls: g
                .calls
                .iter()
                .map(|c| UiToolCall {
                    call_id: c.id.clone(),
                    name: c.name.clone(),
                    args: c.args.clone(),
                })
                .collect(),
        },
        // A tool result is its own event, and an image only ever arrives from the user.
        ContentPart::ToolResult(_) | ContentPart::Image(_) => return None,
    })
}

/// `pub(crate)` because `entry_data::tool_display` records the same value in the `display`
/// column: the stream and the stored projection must agree on the status, so there is one mapping.
pub(crate) fn wire_status(status: ToolExecStatus) -> ToolStatus {
    match status {
        ToolExecStatus::Success => ToolStatus::Completed,
        ToolExecStatus::Failed => ToolStatus::Error,
        ToolExecStatus::Denied => ToolStatus::Denied,
        ToolExecStatus::Timeout => ToolStatus::Timeout,
        ToolExecStatus::PrecheckFailed => ToolStatus::PrecheckFailed,
        ToolExecStatus::Cancelled => ToolStatus::Cancelled,
    }
}
