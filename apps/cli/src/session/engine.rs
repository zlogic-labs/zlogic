//! `EngineSession` — the real backend, wired to the in-process engine.
//! This replaces the deferred `BridgeSession` plan: hosts run the engine
//! **in-process** (this CLI is one of them, linked against the engine crate), so there is no
//! stdio bridge any more. The flow is the one every host uses:
//! ```text
//! bootstrap(host="cli-rs") → workspace.open_at(cwd) → session_open
//!   → hub.subscribe_turns (BEFORE submit) → engine.submit → stream
//! ```
//! `CoreSession` stays the UI's only backend surface; this is a thin adapter:
//! every method forwards to `zlogic_engine::EngineApi`, and the event stream is the
//! hub's `StreamEvent` broadcast translated into the UI's `CoreEvent` DTOs. The
//! sync facade wraps async ops in one owned multi-thread tokio runtime; the pump
//! threads run inside that runtime and never call back into it.
//! # Known gaps
//! - Plan mode (`/plan`) is a UI-local posture; the engine has no plan op yet.
//! - `/archive` has no engine op (`session_delete` is the only session mutation);
//!   `archive_session` is a no-op.
//! - Turn summary is synthesized from `TurnEnd` stats, not a separate RPC.
//! - `usage_timeline` is approximated from the session-level usage groups (the
//!   engine has no per-turn usage listing op).
//! - `command_catalog` returns the frontend built-ins only (no skill catalog op yet).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};

use zlogic_engine::bootstrap::BootstrapOptions;
use zlogic_engine::{Engine, EngineApi};
use zlogic_protocol::config::ProviderConfig;
use zlogic_protocol::interaction::{
    Control, DecisionSource, FieldValue, FormAnswer, GrantScope as ProtoScope, InteractionBody,
    InteractionDecision,
};
use zlogic_protocol::query::{
    ConfigRemoveProviderReq, CredentialDeleteReq, CredentialSetReq, CredentialVerifyReq,
    EntriesReq, OpenAiCompatibleProviderReq, SessionListReq, SessionOpenReq, SessionRenameReq,
    SessionSearchReq, TranscriptReq, TurnsReq, UsageSummaryReq, WorkspaceSelector,
};
use zlogic_protocol::stream::{BlockFinal, BlockKind, StreamEvent, StreamPayload, ToolStatus};
use zlogic_protocol::{
    Command as ProtoCommand, Delivery, MessagePart, Submission, SubmitAck, TokenUsage,
};

use crate::i18n::{I18n, Locale};

use super::dto::*;
use super::CoreSession;

/// What `connect` needs from the CLI layer.
pub struct ConnectOptions {
    /// Where to open the workspace (defaults to the process cwd).
    pub cwd: PathBuf,
    /// Resume an existing session instead of opening a new one.
    pub session_id: Option<String>,
    /// Bare `--resume`: open the workspace's most recent session. Ignored when
    /// `session_id` is set.
    pub resume_latest: bool,
    /// God mode (full access) — maps to `BootstrapOptions::approval_mode` (bypass).
    pub god: bool,
    /// UI language, used to localize replies surfaced at run time (e.g. the
    /// no-usable-model error shown when the user sends a message).
    pub locale: crate::i18n::Locale,
    pub pick: WorkspacePick,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspacePick {
    Auto,
    Interactive,
}

fn locale_of(stored: &str, fallback: crate::i18n::Locale) -> crate::i18n::Locale {
    crate::i18n::Locale::resolve(stored).unwrap_or(fallback)
}

fn startup_err<const N: usize>(i18n: &I18n, id: &str, args: [(&str, String); N]) -> anyhow::Error {
    let mut fluent_args = fluent_bundle::FluentArgs::new();
    for (name, value) in &args {
        fluent_args.set(*name, value.as_str());
    }
    anyhow::anyhow!("{}", i18n.format(id, Some(&fluent_args)))
}

/// State shared with the pump threads (they translate stream events into
/// `CoreEvent`s and need the renderer's block-classification state).
struct PumpState {
    events_tx: Sender<CoreEvent>,
    bus_tx: Sender<CoreBusEvent>,
    /// Stream events are delta-shaped; a `BlockDelta` needs its `BlockStart` kind.
    open_blocks: Mutex<HashMap<String, BlockKind>>,
    /// Round summaries for the synthesized `TurnSummary` (design: core-owned stats).
    round_summaries: Mutex<Vec<RoundSummary>>,
    thinking_chars: Mutex<u64>,
    turn_tokens: Mutex<TokenUsage>,
    /// `(used, window)` from the last `Usage` stream event.
    current_context: Mutex<Option<(u64, u64)>>,
    /// Pending (undelivered) mailbox entries, seeded from `session_open` and
    /// maintained from `MailboxConsumed`.
    mailbox: Mutex<Vec<MailboxEntry>>,
    active_turn_id: Arc<Mutex<Option<String>>>,
    locale: Mutex<Locale>,
}

pub struct EngineSession {
    engine: Arc<dyn EngineApi>,
    hub: Arc<zlogic_engine::hub::EventHub>,
    /// Kept alive for the rolling-file writer guard.
    _logs: Option<zlogic_logging::LogGuard>,
    runtime: Arc<tokio::runtime::Runtime>,
    workspace_id: Mutex<zlogic_protocol::WorkspaceId>,
    exec_cwd: Mutex<String>,
    models_path: PathBuf,
    session_id: Mutex<String>,
    turn_pump: Mutex<Option<tokio::task::JoinHandle<()>>>,
    events: Mutex<Option<Receiver<CoreEvent>>>,
    bus_events: Mutex<Option<Receiver<CoreBusEvent>>>,
    submit_seq: AtomicU64,
    plan_enabled: Mutex<bool>,
    locale: Mutex<String>,
    opened_model: Mutex<Option<String>>,
    usage_cache: Mutex<Option<(std::time::Instant, zlogic_protocol::UsageSummary)>>,
    pump: Arc<PumpState>,
}

const USAGE_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(30);

const EXIT_TURN_GRACE: std::time::Duration = std::time::Duration::from_secs(2);
const EXIT_TURN_POLL: std::time::Duration = std::time::Duration::from_millis(50);

impl Drop for EngineSession {
    fn drop(&mut self) {
        self.stop_turns_on_exit();
    }
}

impl EngineSession {
    fn stop_turns_on_exit(&self) {
        let engine = self.engine.clone();
        let outcome = self.runtime.block_on(async move {
            let cancelled = engine.cancel_all_turns().await.map_err(|e| e.to_string())?;
            let deadline = tokio::time::Instant::now() + EXIT_TURN_GRACE;
            while tokio::time::Instant::now() < deadline {
                match engine.live_turn_count().await {
                    Ok(0) => break,
                    Ok(_) => tokio::time::sleep(EXIT_TURN_POLL).await,
                    Err(e) => return Err(e.to_string()),
                }
            }
            Ok::<usize, String>(cancelled)
        });
        match outcome {
            Ok(0) => {}
            Ok(n) => tracing::info!(
                target: "zlogic::cli",
                turns = n,
                "cancelled live turns before exit"
            ),
            Err(error) => tracing::warn!(
                target: "zlogic::cli",
                "could not cancel live turns before exit: {error}"
            ),
        }
    }

    /// Bootstrap the engine, open the workspace + session, subscribe to the hub,
    /// and start the event pumps. **Subscribe happens here, before any submit** —
    /// the stream has no replay (same rule as apps/cli).
    pub fn connect(opts: ConnectOptions) -> anyhow::Result<Self> {
        let locale = opts.locale;
        let i18n = I18n::new(locale);

        // ① Logging first: every `tracing::` line from bootstrap onwards needs a sink.
        // A TUI owns its terminal, so `to_file: true` maps to per-day/per-module files
        // the screen is live.
        let dirs = zlogic_config::Dirs::discover()
            .map_err(|e| startup_err(&i18n, "startup-dirs-failed", [("error", e.to_string())]))?;
        let log_cfg = zlogic_config::ConfigFiles::read(&dirs)
            .ok()
            .and_then(|files| files.config.and_then(|config| config.log))
            .unwrap_or_default();
        let sink = zlogic_logging::Sink::from_config(&log_cfg, &dirs.logs(), true);
        let _logs = match zlogic_logging::init(&log_cfg, sink) {
            Ok(Some(logs)) => Some(logs),
            Ok(None) => None, // already installed (tests / embedding)
            Err(error) => {
                eprintln!("failed to initialize logging, continuing startup: {error}");
                None
            }
        };

        // ② Runtime first: multi-thread is required — snapshot traversal uses
        // `spawn_blocking`, and a current-thread runtime has a single blocking pool
        // thread that the first full snapshot would saturate.
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|e| {
                    startup_err(&i18n, "startup-runtime-failed", [("error", e.to_string())])
                })?,
        );

        let (boot, workspace, opened) = runtime.block_on(async move {
            // `"cli-rs"` enters the session lock — when the lock is held elsewhere
            // the UI can say who is running. No html_widgets / renders_math: this is
            // a terminal, widget cards and `$$…$$` would be unreadable.
            let boot = Engine::bootstrap(BootstrapOptions::new("cli-rs").approval_mode(opts.god))
                .map_err(|e| {
                startup_err(&i18n, "startup-engine-failed", [("error", e.to_string())])
            })?;
            for warning in &boot.warnings {
                eprintln!("config notice: {warning}");
            }

            let (workspace, deviation) = match opts.pick {
                WorkspacePick::Auto => boot.workspaces.open_at(&opts.cwd).map_err(|e| {
                    startup_err(
                        &i18n,
                        "startup-workspace-failed",
                        [("error", e.to_string())],
                    )
                })?,
                WorkspacePick::Interactive => {
                    if boot.workspaces.locate(&opts.cwd).is_some() {
                        boot.workspaces.open_at(&opts.cwd).map_err(|e| {
                            startup_err(
                                &i18n,
                                "startup-workspace-failed",
                                [("error", e.to_string())],
                            )
                        })?
                    } else {
                        let list = boot.engine.workspace_list(false).await.map_err(|e| {
                            startup_err(
                                &i18n,
                                "startup-workspace-list-failed",
                                [("error", e.to_string())],
                            )
                        })?;
                        match crate::cli::workspace_pick::choose(&opts.cwd, &list, &i18n) {
                            Some(crate::cli::workspace_pick::Choice::CreateHere) => {
                                boot.workspaces.open_at(&opts.cwd).map_err(|e| {
                                    startup_err(
                                        &i18n,
                                        "startup-workspace-failed",
                                        [("error", e.to_string())],
                                    )
                                })?
                            }
                            Some(crate::cli::workspace_pick::Choice::PickExisting {
                                workspace_id,
                            }) => {
                                let workspace_id = workspace_id
                                    .parse::<zlogic_protocol::WorkspaceId>()
                                    .map_err(|e| {
                                        startup_err(
                                            &i18n,
                                            "startup-workspace-id-invalid",
                                            [
                                                ("raw", workspace_id.clone()),
                                                ("error", e.to_string()),
                                            ],
                                        )
                                    })?;
                                let ws =
                                    boot.engine.workspace_get(workspace_id).await.map_err(|e| {
                                        startup_err(
                                            &i18n,
                                            "startup-workspace-get-failed",
                                            [("error", e.to_string())],
                                        )
                                    })?;
                                (ws, None)
                            }
                            None => {
                                return Err(startup_err(&i18n, "startup-workspace-cancelled", []))
                            }
                        }
                    }
                }
            };
            if let Some(dev) = &deviation {
                eprintln!(
                    "{}",
                    startup_err(
                        &i18n,
                        "startup-deviation-note",
                        [
                            ("root", workspace.root.clone()),
                            ("dir", dev.display().to_string()),
                        ]
                    )
                );
            }

            let session_id_arg = match opts.session_id.as_deref() {
                Some(raw) => Some(raw.parse::<zlogic_protocol::SessionId>().map_err(|e| {
                    startup_err(
                        &i18n,
                        "startup-session-id-invalid",
                        [("raw", raw.to_string()), ("error", e.to_string())],
                    )
                })?),
                None if opts.resume_latest => {
                    let page = boot
                        .engine
                        .session_list(SessionListReq {
                            workspace: WorkspaceSelector::Id {
                                workspace_id: workspace.workspace_id,
                            },
                            include_sub_agents: false,
                            include_archived: false,
                            limit: Some(1),
                            offset: None,
                        })
                        .await
                        .map_err(|e| {
                            startup_err(&i18n, "startup-latest-failed", [("error", e.to_string())])
                        })?;
                    match page.items.into_iter().next() {
                        Some(latest) => Some(latest.session_id),
                        None => return Err(startup_err(&i18n, "startup-no-session", [])),
                    }
                }
                None => None,
            };
            let opened = boot
                .engine
                .session_open(SessionOpenReq {
                    workspace: WorkspaceSelector::Id {
                        workspace_id: workspace.workspace_id,
                    },
                    session_id: session_id_arg,
                })
                .await
                .map_err(|e| {
                    startup_err(
                        &i18n,
                        "startup-open-session-failed",
                        [("error", e.to_string())],
                    )
                })?;
            Ok::<_, anyhow::Error>((boot, workspace, opened))
        })?;

        let session_id = opened.session.session_id.to_string();
        let opened_model = opened.model.model_ref.clone();

        // ⑥ Subscribe BEFORE anything is submitted (no replay).
        let hub = boot.engine.hub.clone();
        let engine: Arc<dyn EngineApi> = Arc::new(boot.engine);
        let turn_rx = hub.subscribe_turns(&session_id);
        let notice_rx = hub.subscribe_notices();

        let (events_tx, events_rx) = mpsc::channel();
        let (bus_tx, bus_rx) = mpsc::channel();
        let active_turn_id = Arc::new(Mutex::new(None));
        let pump = Arc::new(PumpState {
            events_tx,
            bus_tx,
            open_blocks: Mutex::new(HashMap::new()),
            round_summaries: Mutex::new(Vec::new()),
            thinking_chars: Mutex::new(0),
            turn_tokens: Mutex::new(TokenUsage::default()),
            current_context: Mutex::new(None),
            mailbox: Mutex::new(Vec::new()),
            active_turn_id: Arc::clone(&active_turn_id),
            locale: Mutex::new(locale),
        });
        seed_mailbox(&pump.mailbox, &opened);

        let turn_pump = {
            let pump = Arc::clone(&pump);
            Some(runtime.spawn(pump_turns(turn_rx, pump)))
        };
        {
            let pump = Arc::clone(&pump);
            runtime.spawn(pump_bus(notice_rx, pump));
        }

        Ok(Self {
            engine,
            hub,
            _logs,
            runtime,
            workspace_id: Mutex::new(workspace.workspace_id),
            exec_cwd: Mutex::new(opened.exec_cwd),
            models_path: dirs.models_file(),
            session_id: Mutex::new(session_id),
            turn_pump: Mutex::new(turn_pump),
            events: Mutex::new(Some(events_rx)),
            bus_events: Mutex::new(Some(bus_rx)),
            submit_seq: AtomicU64::new(1),
            plan_enabled: Mutex::new(false),
            locale: Mutex::new("auto".into()),
            opened_model: Mutex::new(opened_model),
            usage_cache: Mutex::new(None),
            pump,
        })
    }

    fn current_session(&self) -> String {
        self.session_id.lock().unwrap().clone()
    }

    fn current_workspace_id(&self) -> zlogic_protocol::WorkspaceId {
        *self.workspace_id.lock().unwrap()
    }

    fn rearm_turn_pump(&self, session_id: &str) {
        if let Some(handle) = self.turn_pump.lock().unwrap().take() {
            handle.abort();
        }
        let rx = self.hub.subscribe_turns(session_id);
        let pump = Arc::clone(&self.pump);
        let handle = self.runtime.spawn(pump_turns(rx, pump));
        *self.turn_pump.lock().unwrap() = Some(handle);
    }

    fn refresh_after_open(&self, opened: &zlogic_protocol::SessionOpened) {
        *self.opened_model.lock().unwrap() = opened.model.model_ref.clone();
        *self.exec_cwd.lock().unwrap() = opened.exec_cwd.clone();
        seed_mailbox(&self.pump.mailbox, opened);
        *self.usage_cache.lock().unwrap() = None;
    }

    fn session_i18n(&self) -> I18n {
        let stored = self.locale.lock().unwrap().clone();
        I18n::new(locale_of(&stored, Locale::detect()))
    }

    fn set_session(&self, id: &str) {
        *self.session_id.lock().unwrap() = id.to_string();
    }

    /// Push an error into the event stream so the UI surfaces it like any other
    /// core error (oneshot exits 1, the TUI shows a notification).
    fn emit_error(&self, message: impl Into<String>) {
        let _ = self.pump.events_tx.send(CoreEvent::Error {
            message: message.into(),
        });
    }

    fn submit_parts(
        &self,
        parts: Vec<MessagePart>,
        delivery: Delivery,
        model_ref: Option<String>,
    ) -> bool {
        let session_id = self.current_session();
        let seq = self.submit_seq.fetch_add(1, Ordering::Relaxed);
        let submission = Submission {
            submission_id: String::new(),
            session_id: session_id.clone(),
            client_request_id: format!("cli-rs-{}-{seq}", std::process::id()),
            parts,
            delivery,
            model_ref,
            thinking: None,
        };
        let i18n = self.session_i18n();
        let messages: Vec<Message> = submission
            .parts
            .iter()
            .map(|part| message_part_to_message(part, &i18n))
            .collect();
        match self.runtime.block_on(self.engine.submit(submission)) {
            Ok(SubmitAck::Accepted {
                submission_id,
                started_turn_id,
                notice,
            }) => {
                if started_turn_id.is_none() {
                    self.pump.mailbox.lock().unwrap().push(MailboxEntry {
                        id: submission_id,
                        messages,
                    });
                }
                if let Some(notice) = notice {
                    self.emit_error(self.session_i18n().wire(&notice));
                }
                started_turn_id.is_some()
            }
            Ok(SubmitAck::Duplicate { .. }) => false,
            Ok(SubmitAck::UnsupportedInput {
                missing_capabilities,
                suggested_models,
            }) => {
                let caps = missing_capabilities.join(", ");
                let models = suggested_models.join(", ");
                let mut args = fluent_bundle::FluentArgs::new();
                args.set("caps", caps.as_str());
                args.set("models", models.as_str());
                self.emit_error(
                    self.session_i18n()
                        .format("submit-unsupported-input", Some(&args)),
                );
                false
            }
            Ok(SubmitAck::Rejected { reason }) => {
                self.emit_error(self.session_i18n().wire(&reason));
                false
            }
            Err(error) => {
                self.emit_error(error.to_string());
                false
            }
        }
    }

    fn control(&self, command: ProtoCommand) {
        if let Err(error) = self.runtime.block_on(self.engine.control(command)) {
            let detail = error.to_string();
            let mut args = fluent_bundle::FluentArgs::new();
            args.set("error", detail.as_str());
            self.emit_error(self.session_i18n().format("control-failed", Some(&args)));
        }
    }

    /// `provider:model` split — the model overlay addresses models by their full ref.
    fn split_model_ref(model: &str) -> Option<(&str, &str)> {
        model.rsplit_once(':')
    }

    /// The model currently selected for the session, if any (session preference
    /// wins over the global default).
    fn current_model_ref(&self) -> Option<String> {
        let session_id = self.current_session();
        if let Ok(page) = self
            .runtime
            .block_on(self.engine.session_list(SessionListReq {
                workspace: WorkspaceSelector::Id {
                    workspace_id: self.current_workspace_id(),
                },
                include_sub_agents: false,
                include_archived: false,
                limit: None,
                offset: None,
            }))
        {
            if let Some(session) = page
                .items
                .into_iter()
                .find(|s| s.session_id.to_string() == session_id)
            {
                if session.model_ref.is_some() {
                    return session.model_ref;
                }
            }
        }
        let opened = self.opened_model.lock().unwrap().clone();
        if opened.is_some() {
            return opened;
        }
        let view = self.runtime.block_on(self.engine.config_get()).ok()?;
        view.default_model
    }

    fn catalog(&self) -> Option<Vec<ProviderConfig>> {
        let merged = self
            .runtime
            .block_on(self.engine.config_reload())
            .or_else(|_| self.runtime.block_on(self.engine.config_get()))
            .ok()?;
        let catalog = self.runtime.block_on(self.engine.config_catalog()).ok();

        let mut ids: Vec<String> = catalog
            .as_ref()
            .map(|c| c.providers.iter().map(|p| p.provider_id.clone()).collect())
            .unwrap_or_default();
        for provider in &merged.providers {
            if !ids.iter().any(|id| id == &provider.provider_id) {
                ids.push(provider.provider_id.clone());
            }
        }

        let mut by_id: BTreeMap<String, ProviderConfig> = BTreeMap::new();
        if let Some(c) = &catalog {
            for p in &c.providers {
                by_id.insert(p.provider_id.clone(), p.clone());
            }
        }
        for p in merged.providers {
            by_id.insert(p.provider_id.clone(), p);
        }

        Some(ids.into_iter().filter_map(|id| by_id.remove(&id)).collect())
    }

    fn credentials(&self) -> Vec<zlogic_protocol::CredentialState> {
        self.runtime
            .block_on(self.engine.credential_list())
            .unwrap_or_default()
    }

    /// Re-open the session service so the adapter's current-session pointer follows.
    fn open_session(&self, session_id: Option<&str>) -> Option<String> {
        let opened = self
            .runtime
            .block_on(self.engine.session_open(SessionOpenReq {
                workspace: WorkspaceSelector::Id {
                    workspace_id: self.current_workspace_id(),
                },
                session_id: session_id.and_then(|s| s.parse().ok()),
            }))
            .ok()?;
        let id = opened.session.session_id.to_string();
        self.set_session(&id);
        self.refresh_after_open(&opened);
        self.rearm_turn_pump(&id);
        Some(id)
    }
}

// ─────────────────────────────── pumps ───────────────────────────────

async fn pump_turns(mut rx: tokio::sync::broadcast::Receiver<StreamEvent>, pump: Arc<PumpState>) {
    loop {
        match rx.recv().await {
            Ok(event) => {
                for ev in translate(&event, &pump) {
                    if pump.events_tx.send(ev).is_err() {
                        return; // UI went away
                    }
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                // Dropped: the stream has no replay; authoritative state is in the
                // store and the UI re-reads it via query APIs.
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        }
    }
}

async fn pump_bus(
    mut rx: tokio::sync::broadcast::Receiver<zlogic_protocol::stream::StateNotice>,
    pump: Arc<PumpState>,
) {
    use zlogic_protocol::stream::StateChange;
    loop {
        match rx.recv().await {
            Ok(notice) => {
                if notice.change == StateChange::MailboxChanged
                    && pump.bus_tx.send(CoreBusEvent::MailboxChanged).is_err()
                {
                    return;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        }
    }
}

// ─────────────────── stream translation (hub → CoreEvent) ───────────────────

fn i18n(pump: &PumpState) -> I18n {
    I18n::new(*pump.locale.lock().unwrap_or_else(PoisonError::into_inner))
}

/// One `StreamEvent` may expand to several `CoreEvent`s (e.g. `TurnEnd` →
/// `TurnDone` + synthesized `TurnSummaryLoaded`).
fn translate(event: &StreamEvent, pump: &PumpState) -> Vec<CoreEvent> {
    use zlogic_protocol::stream::SteeringContent;
    let mut out = Vec::new();
    let turn_id = event.turn_id.to_string();
    match &event.payload {
        StreamPayload::TurnStart { proactive, .. } => {
            *pump.active_turn_id.lock().unwrap() = Some(turn_id.clone());
            *pump.round_summaries.lock().unwrap() = Vec::new();
            *pump.thinking_chars.lock().unwrap() = 0;
            *pump.turn_tokens.lock().unwrap() = TokenUsage::default();
            *pump.current_context.lock().unwrap() = None;
            out.push(CoreEvent::TurnStart {
                turn_id,
                proactive: *proactive,
            });
        }

        StreamPayload::TurnEnd {
            status: _, stats, ..
        } => {
            let mut rounds = pump.round_summaries.lock().unwrap().clone();
            if rounds.is_empty() && stats.rounds > 0 {
                // Attached mid-turn: no RoundStart observed; synthesize one row.
                rounds.push(RoundSummary {
                    round_id: String::new(),
                    thinking_chars: 0,
                    tool_count: stats.tools.total as u64,
                    failed_tool_count: (stats.tools.failed + stats.tools.denied) as u64,
                });
            }
            let tokens = *pump.turn_tokens.lock().unwrap();
            let (ctx_used, ctx_limit) = pump.current_context.lock().unwrap().unwrap_or((0, 0));
            out.push(CoreEvent::TurnDone {
                turn_id: turn_id.clone(),
            });
            out.push(CoreEvent::TurnSummaryLoaded {
                summary: TurnSummary {
                    turn_id,
                    rounds,
                    thinking_chars: *pump.thinking_chars.lock().unwrap(),
                    tool_count: stats.tools.total as u64,
                    failed_tool_count: (stats.tools.failed + stats.tools.denied) as u64,
                    input_tokens: tokens.input,
                    output_tokens: tokens.output,
                    context_used_tokens: ctx_used,
                    context_limit_tokens: ctx_limit,
                },
            });
            *pump.active_turn_id.lock().unwrap() = None;
        }

        StreamPayload::RoundStart { round_id, .. } => {
            pump.round_summaries.lock().unwrap().push(RoundSummary {
                round_id: round_id.to_string(),
                ..Default::default()
            });
            out.push(CoreEvent::RoundStart {
                turn_id: turn_id.clone(),
                round_id: round_id.to_string(),
            });
        }

        StreamPayload::RoundEnd {
            round_id, stats, ..
        } => {
            if let Some(round) = pump
                .round_summaries
                .lock()
                .unwrap()
                .iter_mut()
                .find(|r| r.round_id == round_id.to_string())
            {
                round.tool_count = stats.tools.total as u64;
                round.failed_tool_count = (stats.tools.failed + stats.tools.denied) as u64;
            }
            let mut tokens = pump.turn_tokens.lock().unwrap();
            tokens.input += stats.usage.input;
            tokens.output += stats.usage.output;
            if let Some(cache_read) = stats.usage.cache_read {
                tokens.cache_read = Some(tokens.cache_read.unwrap_or(0) + cache_read);
            }
            if let Some(cache_write) = stats.usage.cache_write {
                tokens.cache_write = Some(tokens.cache_write.unwrap_or(0) + cache_write);
            }
        }

        StreamPayload::BlockStart { block_id, kind, .. } => {
            pump.open_blocks
                .lock()
                .unwrap()
                .insert(block_id.clone(), *kind);
            if matches!(kind, BlockKind::Reasoning) {
                out.push(CoreEvent::ThinkingStart { depth: None });
            }
        }

        StreamPayload::BlockDelta { block_id, delta } => {
            match pump.open_blocks.lock().unwrap().get(block_id).copied() {
                Some(BlockKind::Reasoning) => {
                    *pump.thinking_chars.lock().unwrap() += delta.chars().count() as u64;
                    out.push(CoreEvent::ThinkingDelta {
                        text: delta.clone(),
                        depth: None,
                    });
                }
                Some(BlockKind::Text) | None => out.push(CoreEvent::TextDelta {
                    text: delta.clone(),
                    depth: None,
                    agent_id: event.agent.agent_id.clone(),
                }),
                // Tool-call args stream through `BlockDelta` but are not rendered.
                Some(BlockKind::ToolCall) => {}
            }
        }

        StreamPayload::BlockEnd { block_id, block } => {
            pump.open_blocks.lock().unwrap().remove(block_id);
            match block {
                BlockFinal::Reasoning { .. } => {
                    out.push(CoreEvent::ThinkingEnd { depth: None });
                }
                // Text was already streamed via deltas; BlockEnd carries the
                // authoritative final value but appending it would double-render.
                BlockFinal::Text { .. } => {}
                BlockFinal::ToolCall { calls } => {
                    for call in calls {
                        let args = serde_json::from_str(&call.args)
                            .unwrap_or_else(|_| serde_json::json!({}));
                        out.push(CoreEvent::ToolCallStart {
                            id: call.call_id.clone(),
                            name: call.name.clone(),
                            args,
                        });
                    }
                }
            }
        }

        StreamPayload::ToolDetected { .. } => {}
        StreamPayload::ToolExecStart { .. } => {}
        StreamPayload::ToolOutputDelta { .. } => {}
        StreamPayload::ToolExecEnd {
            call_id,
            status,
            summary,
            ..
        } => {
            out.push(CoreEvent::ToolCallEnd {
                id: call_id.clone(),
                ok: matches!(status, ToolStatus::Completed),
                summary: summary.clone(),
            });
        }

        StreamPayload::Usage { turn, context, .. } => {
            *pump.current_context.lock().unwrap() =
                Some((context.used.unwrap_or(0), context.window));
            out.push(CoreEvent::Usage {
                snapshot: UsageSnapshot {
                    total_tokens: turn.input
                        + turn.output
                        + turn.cache_read.unwrap_or(0)
                        + turn.cache_write.unwrap_or(0),
                    context_used_tokens: context.used.unwrap_or(0),
                    context_limit_tokens: context.window,
                    ..Default::default()
                },
            });
        }

        StreamPayload::MailboxConsumed { submission_ids, .. } => {
            pump.mailbox
                .lock()
                .unwrap()
                .retain(|entry| !submission_ids.contains(&entry.id));
        }

        StreamPayload::SteeringInjected {
            entry_id, content, ..
        } => {
            let messages = match content {
                Some(SteeringContent::User { text }) => vec![Message::text(text.clone())],
                Some(SteeringContent::TaskUpdate { summary, .. }) => vec![Message::text(
                    summary
                        .clone()
                        .unwrap_or_else(|| i18n(pump).t("task-update-fallback")),
                )],
                None => Vec::new(),
            };
            out.push(CoreEvent::Mailbox {
                entry: MailboxEntry {
                    id: entry_id.clone(),
                    messages,
                },
            });
        }

        StreamPayload::InteractionRequired {
            interaction_id,
            body,
        } => out.push(interaction_to_event(
            interaction_id,
            body,
            *pump.locale.lock().unwrap_or_else(PoisonError::into_inner),
        )),

        StreamPayload::InteractionResolved { .. } => {}
        StreamPayload::Notice { level, message, .. } => out.push(CoreEvent::BusNotification {
            level: match level {
                zlogic_protocol::stream::NoticeLevel::Info => NotificationLevel::Info,
                zlogic_protocol::stream::NoticeLevel::Warn => NotificationLevel::Warning,
            },
            title: i18n(pump).wire(&message),
            message: String::new(),
            source: Some("turn".into()),
        }),

        StreamPayload::Error { error } => out.push(CoreEvent::Error {
            message: error.to_string(),
        }),

        StreamPayload::CompactionStart { .. } => out.push(CoreEvent::BusNotification {
            level: NotificationLevel::Info,
            title: i18n(pump).t("notification-compacting"),
            message: String::new(),
            source: Some("turn".into()),
        }),

        StreamPayload::CompactionEnd {
            replaces,
            summary,
            summary_tokens,
        } => out.push(CoreEvent::Compaction {
            replaces: *replaces,
            summary: summary.clone(),
            summary_tokens: *summary_tokens,
        }),
    }
    out
}

/// `InteractionBody` → the UI's interactive event (all variants are BLOCKING:
/// the UI must answer via `Command::Respond`).
fn interaction_to_event(interaction_id: &str, body: &InteractionBody, locale: Locale) -> CoreEvent {
    match body {
        InteractionBody::Permission {
            tool,
            args_preview,
            reason,
            caveats,
            ..
        } => CoreEvent::PermissionRequest {
            id: interaction_id.to_string(),
            category: "tool".into(),
            action: tool.clone(),
            target: args_preview.clone(),
            reason: if caveats.is_empty() {
                reason.clone()
            } else {
                format!("{reason} ({})", caveats.join("; "))
            },
            risk: None,
        },
        InteractionBody::EnvPassthrough { skill, vars, .. } => {
            let vars = vars.join(", ");
            let mut args = fluent_bundle::FluentArgs::new();
            args.set("skill", skill.as_str());
            args.set("vars", vars.as_str());
            CoreEvent::ConfirmationRequest {
                id: interaction_id.to_string(),
                message: I18n::new(locale).format("env-passthrough-request", Some(&args)),
            }
        }
        InteractionBody::Form(form) => form_to_event(interaction_id, form, locale),
    }
}

/// A form degrades into the closest interactive card the TUI has: a single
/// confirm control → ConfirmationRequest, a single free-text input → InputRequest,
/// anything richer → FormRequest.
fn form_to_event(interaction_id: &str, form: &zlogic_protocol::Form, locale: Locale) -> CoreEvent {
    let single_control = (form.fields.len() == 1).then(|| &form.fields[0].control);
    match single_control {
        Some(Control::Confirm { .. }) => CoreEvent::ConfirmationRequest {
            id: interaction_id.to_string(),
            message: form.message.clone().unwrap_or_else(|| form.title.clone()),
        },
        Some(Control::Input { .. } | Control::Textarea { .. } | Control::Path { .. }) => {
            CoreEvent::InputRequest {
                id: interaction_id.to_string(),
                prompt: form.title.clone(),
            }
        }
        _ => CoreEvent::FormRequest {
            id: interaction_id.to_string(),
            title: form.title.clone(),
            fields: form
                .fields
                .iter()
                .map(|f| form_field_to_spec(f, locale))
                .collect(),
            items: Vec::new(),
        },
    }
}

fn form_field_to_spec(field: &zlogic_protocol::FormField, locale: Locale) -> FormFieldSpec {
    let key = field.key.clone();
    let label = field.label.clone();
    let hint = field.help.clone().unwrap_or_default();
    let i18n = &I18n::new(locale);
    match &field.control {
        Control::Input {
            default,
            placeholder,
            max_len,
        } => FormFieldSpec::Text {
            name: key,
            label,
            placeholder: placeholder.clone().unwrap_or_default(),
            value: default.clone().unwrap_or_default(),
            hint: max_len
                .map(|n| {
                    let mut args = fluent_bundle::FluentArgs::new();
                    args.set("n", n as i64);
                    i18n.format("form-max-length", Some(&args))
                })
                .unwrap_or(hint),
            required: field.required,
            format: String::new(),
        },
        Control::Textarea {
            default,
            placeholder,
            max_len,
            ..
        } => FormFieldSpec::Text {
            name: key,
            label,
            placeholder: placeholder.clone().unwrap_or_default(),
            value: default.clone().unwrap_or_default(),
            hint: max_len
                .map(|n| {
                    let mut args = fluent_bundle::FluentArgs::new();
                    args.set("n", n as i64);
                    i18n.format("form-max-length", Some(&args))
                })
                .unwrap_or(hint),
            required: field.required,
            format: String::new(),
        },
        Control::Select {
            options,
            default,
            free_text,
        } => FormFieldSpec::Select {
            name: key,
            label,
            options: options.iter().map(|o| o.label.clone()).collect(),
            selected: default
                .as_ref()
                .and_then(|d| options.iter().position(|o| &o.value == d))
                .unwrap_or(0),
            hint: if *free_text && !hint.is_empty() {
                let mut args = fluent_bundle::FluentArgs::new();
                args.set("hint", hint.as_str());
                i18n.format("form-free-text-hint", Some(&args))
            } else {
                hint
            },
        },
        Control::MultiSelect {
            options, defaults, ..
        } => FormFieldSpec::MultiSelect {
            name: key,
            label,
            options: options.iter().map(|o| o.label.clone()).collect(),
            selected: defaults
                .iter()
                .filter_map(|d| options.iter().position(|o| &o.value == d))
                .collect(),
            hint,
            required: field.required,
        },
        Control::Confirm { default } => FormFieldSpec::Checkbox {
            name: key,
            label,
            checked: default.unwrap_or(false),
            hint,
        },
        Control::Number { default, .. } => FormFieldSpec::Text {
            name: key,
            label,
            placeholder: String::new(),
            value: default.map(|d| d.to_string()).unwrap_or_default(),
            hint,
            required: field.required,
            format: "number".into(),
        },
        Control::Path { default, .. } => FormFieldSpec::Text {
            name: key,
            label,
            placeholder: String::new(),
            value: default.clone().unwrap_or_default(),
            hint,
            required: field.required,
            format: String::new(),
        },
    }
}

fn answer_to_decision(answer: &Answer) -> InteractionDecision {
    match answer {
        Answer::Allow { scope } => InteractionDecision::Allow {
            scope: match scope {
                GrantScope::Once => ProtoScope::Once,
                GrantScope::Session => ProtoScope::Session,
                // "Always" is a standing authorization → the strongest durable scope.
                GrantScope::Always => ProtoScope::Global,
            },
            source: Some(DecisionSource::User),
        },
        Answer::Deny { message } => InteractionDecision::Deny {
            reason: message.clone(),
        },
        Answer::Input { text } => InteractionDecision::Submitted(
            FormAnswer::new().set("text", FieldValue::Text(text.clone())),
        ),
        Answer::Form { values } => {
            let mut answer = FormAnswer::new();
            for (key, value) in values {
                if let Some(field) = json_to_field_value(value) {
                    answer.values.insert(key.clone(), field);
                }
            }
            InteractionDecision::Submitted(answer)
        }
    }
}

fn json_to_field_value(value: &serde_json::Value) -> Option<FieldValue> {
    match value {
        serde_json::Value::String(s) => Some(FieldValue::Text(s.clone())),
        serde_json::Value::Bool(b) => Some(FieldValue::Bool(*b)),
        serde_json::Value::Number(n) => n.as_f64().map(FieldValue::Number),
        serde_json::Value::Array(items) => {
            let choices: Vec<String> = items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect();
            (!choices.is_empty()).then(|| FieldValue::Choices(choices))
        }
        _ => None,
    }
}

/// `TranscriptPart` → the UI's message `Part` (attachments keep their display name).
fn transcript_part_to_part(part: &zlogic_protocol::TranscriptPart) -> Part {
    match part {
        zlogic_protocol::TranscriptPart::Text { text } => Part::Text { text: text.clone() },
        zlogic_protocol::TranscriptPart::File {
            path,
            display_name,
            mime,
            preview,
            ..
        } => Part::File {
            path: path.clone(),
            display: display_name.clone().unwrap_or_else(|| path.clone()),
            preview: preview.clone().or_else(|| mime.clone()),
        },
        zlogic_protocol::TranscriptPart::Attachment {
            object_id,
            display_name,
            mime,
            preview,
            ..
        } => Part::File {
            path: object_id.clone(),
            display: display_name.clone(),
            preview: preview.clone().or_else(|| Some(mime.clone())),
        },
    }
}

fn transcript_part_to_message(part: zlogic_protocol::TranscriptPart) -> Message {
    Message {
        parts: vec![transcript_part_to_part(&part)],
    }
}

// ─────────────────────────────── CoreSession ───────────────────────────────

impl CoreSession for EngineSession {
    fn subscribe(&self) -> Receiver<CoreEvent> {
        self.events
            .lock()
            .unwrap()
            .take()
            .expect("subscribe() may only be called once")
    }

    fn subscribe_bus(&self) -> Receiver<CoreBusEvent> {
        self.bus_events
            .lock()
            .unwrap()
            .take()
            .expect("subscribe_bus() may only be called once")
    }

    fn send(&self, cmd: Command) -> bool {
        match cmd {
            Command::TurnStart {
                messages,
                model,
                cwd: _,
                plan: _,
                permission: _,
            } => {
                let parts: Vec<MessagePart> = messages
                    .into_iter()
                    .flat_map(|message| message.parts)
                    .filter_map(part_to_message_part)
                    .collect();
                if parts.is_empty() {
                    self.emit_error(self.session_i18n().t("submit-empty"));
                    return false;
                }
                self.submit_parts(parts, Delivery::Queue, model)
            }
            Command::TurnCancel => {
                let turn_id = self.pump.active_turn_id.lock().unwrap().clone();
                if let Some(turn_id) = turn_id {
                    self.control(ProtoCommand::CancelTurn { turn_id });
                }
                true
            }
            Command::Respond { id, answer } => {
                self.control(ProtoCommand::AnswerInteraction {
                    interaction_id: id,
                    decision: answer_to_decision(&answer),
                });
                true
            }
            Command::MailboxEnqueue { messages } => {
                let parts: Vec<MessagePart> = messages
                    .into_iter()
                    .flat_map(|message| message.parts)
                    .filter_map(part_to_message_part)
                    .collect();
                if parts.is_empty() {
                    self.emit_error(self.session_i18n().t("submit-empty"));
                    return false;
                }
                self.submit_parts(parts, Delivery::Steer, None)
            }
            Command::CompactContext => {
                self.control(ProtoCommand::CompactContext {
                    session_id: self.current_session(),
                });
                true
            }
            Command::RequestTurnSummary { .. } => true,
        }
    }

    fn current_session_id(&self) -> Option<String> {
        Some(self.current_session())
    }

    fn mailbox(&self) -> Vec<MailboxEntry> {
        self.pump.mailbox.lock().unwrap().clone()
    }

    fn remove_mailbox(&self, id: &str) -> bool {
        let result = self
            .runtime
            .block_on(self.engine.control(ProtoCommand::CancelSubmission {
                submission_id: id.to_string(),
            }));
        if result.is_ok() {
            self.pump
                .mailbox
                .lock()
                .unwrap()
                .retain(|entry| entry.id != id);
            true
        } else {
            false
        }
    }

    // ── models ──

    fn provider_catalog(&self) -> Vec<ProviderEntry> {
        let Some(providers) = self.catalog() else {
            return Vec::new();
        };
        let keys = self.credentials();
        providers
            .into_iter()
            .map(|provider| {
                let key = keys
                    .iter()
                    .find(|k| k.provider_id == provider.provider_id)
                    .map(|k| k.present)
                    .unwrap_or(false);
                ProviderEntry {
                    name: provider.provider_id,
                    sdk: sdk_id(provider.sdk).to_string(),
                    base_url: provider.base_url,
                    key: if key {
                        KeyStatus::Present
                    } else {
                        KeyStatus::Missing
                    },
                }
            })
            .collect()
    }

    fn model_catalog(&self) -> Vec<ModelEntry> {
        let Some(providers) = self.catalog() else {
            return Vec::new();
        };
        let keys = self.credentials();
        let current = self.current_model_ref();
        let mut entries = Vec::new();
        for provider in providers {
            let key = keys
                .iter()
                .find(|k| k.provider_id == provider.provider_id)
                .map(|k| k.present)
                .unwrap_or(false);
            for model in provider.models {
                let name = format!("{}:{}", provider.provider_id, model.model_id);
                entries.push(ModelEntry {
                    name: name.clone(),
                    provider: provider.provider_id.clone(),
                    tier: match model.tier {
                        Some(zlogic_protocol::config::Tier::Light) => Tier::Light,
                        Some(zlogic_protocol::config::Tier::Thinking) => Tier::Thinking,
                        _ => Tier::Main,
                    },
                    vision: model.capabilities.vision == Some(true),
                    key: if key {
                        KeyStatus::Present
                    } else {
                        KeyStatus::Missing
                    },
                    price: model
                        .pricing
                        .map(|p| format!("${:.2}/${:.2} per M", p.input_per_m, p.output_per_m))
                        .unwrap_or_else(|| "—".into()),
                    context_window: model.context_window.min(u32::MAX as u64) as u32,
                    is_current: current.as_deref() == Some(name.as_str()),
                });
            }
        }
        entries
    }

    fn set_session_model(&self, model: &str) {
        let session_id = self.current_session();
        let Ok(session_id) = session_id.parse() else {
            return;
        };
        if let Err(error) = self
            .runtime
            .block_on(self.engine.session_set_model(session_id, model.to_string()))
        {
            let detail = error.to_string();
            let mut args = fluent_bundle::FluentArgs::new();
            args.set("error", detail.as_str());
            self.emit_error(
                self.session_i18n()
                    .format("model-switch-failed", Some(&args)),
            );
        }
    }

    fn test_connectivity(&self, model: &str) -> Option<ConnResult> {
        let (provider, model_id) = Self::split_model_ref(model)?;
        match self
            .runtime
            .block_on(self.engine.credential_verify(CredentialVerifyReq {
                provider_id: provider.to_string(),
                model_id: model_id.to_string(),
            })) {
            Ok(result) => Some(ConnResult {
                ok: true,
                latency_ms: result.duration_ms as u32,
                detail: format!("{} ({})", result.reply.trim(), result.duration_ms),
            }),
            Err(error) => Some(ConnResult {
                ok: false,
                latency_ms: 0,
                detail: error.to_string(),
            }),
        }
    }

    fn key_catalog(&self) -> Vec<KeyEntry> {
        let Some(providers) = self.catalog() else {
            return Vec::new();
        };
        let sdk = |id: &str| {
            providers
                .iter()
                .find(|p| p.provider_id == id)
                .map(|p| sdk_id(p.sdk).to_string())
                .unwrap_or_default()
        };
        self.credentials()
            .into_iter()
            .map(|cred| KeyEntry {
                provider: cred.provider_id.clone(),
                sdk: sdk(&cred.provider_id),
                status: if cred.present {
                    KeyStatus::Present
                } else {
                    KeyStatus::Missing
                },
                preview: cred.hint.clone(),
                storage: Some(format!("{:?}", cred.source)),
                env_var: cred
                    .candidates
                    .iter()
                    .find(|c| c.starts_with("env:"))
                    .cloned(),
            })
            .collect()
    }

    fn models_config(&self) -> Option<String> {
        Some(self.models_path.display().to_string())
    }

    fn add_provider(&self, name: &str, sdk: &str, base_url: Option<&str>) -> Result<(), String> {
        self.upsert_provider(name, sdk, base_url, None)
    }

    fn add_model(&self, provider: &str, model: &str, context_window: u32) -> bool {
        self.upsert_provider(provider, "", None, Some(model.to_string()))
            .is_ok()
            .then(|| self.set_model_context(model, context_window))
            .unwrap_or(false)
    }

    fn update_provider(&self, name: &str, sdk: &str, base_url: Option<&str>) -> Result<(), String> {
        self.upsert_provider(name, sdk, base_url, None)
    }

    fn set_model_context(&self, model: &str, context_window: u32) -> bool {
        let Some((provider, model_id)) = Self::split_model_ref(model) else {
            return false;
        };
        let Some(providers) = self.catalog() else {
            return false;
        };
        let Some(base) = providers.iter().find(|p| p.provider_id == provider) else {
            return false;
        };
        let base_url = base.base_url.clone().unwrap_or_default();
        self.runtime
            .block_on(
                self.engine
                    .config_upsert_openai_compatible(OpenAiCompatibleProviderReq {
                        provider_id: provider.to_string(),
                        base_url,
                        model_id: Some(model_id.to_string()),
                        context_window: Some(context_window as u64),
                        create_scope: None,
                        expected_revision: None,
                        sdk: None,
                        ..empty_upsert()
                    }),
            )
            .is_ok()
    }

    fn remove_provider(&self, provider: &str) -> bool {
        self.runtime
            .block_on(self.engine.config_remove_provider(ConfigRemoveProviderReq {
                provider_id: provider.to_string(),
                model_id: None,
                expected_revision: None,
            }))
            .is_ok()
    }

    fn remove_model(&self, model: &str) -> bool {
        let Some((provider, model_id)) = Self::split_model_ref(model) else {
            return false;
        };
        self.runtime
            .block_on(self.engine.config_remove_provider(ConfigRemoveProviderReq {
                provider_id: provider.to_string(),
                model_id: Some(model_id.to_string()),
                expected_revision: None,
            }))
            .is_ok()
    }

    fn set_api_key(&self, provider: &str, value: &str, _storage: &str) -> bool {
        self.runtime
            .block_on(self.engine.credential_set(CredentialSetReq {
                provider_id: provider.to_string(),
                value: value.to_string(),
            }))
            .is_ok()
    }

    fn delete_api_key(&self, provider: &str) -> bool {
        self.runtime
            .block_on(self.engine.credential_delete(CredentialDeleteReq {
                provider_id: provider.to_string(),
            }))
            .is_ok()
    }

    // ── usage ──

    fn usage_overview(&self) -> UsageSnapshot {
        let Some(summary) = self.usage_summary() else {
            return UsageSnapshot::default();
        };
        let cost = |g: &[zlogic_protocol::UsageGroup]| {
            g.iter()
                .filter_map(|group| group.cost.as_ref())
                .map(|c| c.amount)
                .sum::<f64>()
        };
        let today_cost = summary
            .by_day
            .last()
            .and_then(|g| g.cost.as_ref())
            .map(|c| c.amount)
            .unwrap_or(0.0);
        UsageSnapshot {
            daily_cost: summary
                .by_day
                .iter()
                .filter_map(|g| g.cost.as_ref())
                .map(|c| c.amount)
                .collect(),
            today_cost,
            total_cost: cost(&summary.by_provider),
            total_tokens: summary.tokens.input
                + summary.tokens.output
                + summary.tokens.cache_read.unwrap_or(0)
                + summary.tokens.cache_write.unwrap_or(0),
            input_tokens: summary.tokens.input,
            output_tokens: summary.tokens.output,
            cache_read_tokens: summary.tokens.cache_read.unwrap_or(0),
            cache_write_tokens: summary.tokens.cache_write.unwrap_or(0),
            request_count: summary.calls as u64,
            turn_count: summary.turns as u64,
            by_model: usage_groups_to_models(&summary.by_model),
            context_used_tokens: summary.current_context_tokens.unwrap_or(0),
            context_limit_tokens: 0,
            cache_hit_rate: 0.0,
            cache_savings: 0.0,
        }
    }

    fn usage_by_model(&self) -> Vec<ModelUsage> {
        let Some(summary) = self.usage_summary() else {
            return Vec::new();
        };
        usage_groups_to_models(&summary.by_model)
    }

    fn usage_timeline(&self) -> Vec<TurnUsage> {
        let Some(summary) = self.usage_summary() else {
            return Vec::new();
        };
        summary
            .by_session
            .into_iter()
            .map(|group| TurnUsage {
                turn_id: group.key,
                cost: group.cost.as_ref().map(|c| c.amount).unwrap_or(0.0),
                tokens_in: group.tokens.input,
                tokens_out: group.tokens.output,
            })
            .collect()
    }

    // ── session management ──

    fn list_sessions(&self) -> Vec<SessionSummary> {
        let page = self
            .runtime
            .block_on(self.engine.session_list(SessionListReq {
                workspace: WorkspaceSelector::Id {
                    workspace_id: self.current_workspace_id(),
                },
                include_sub_agents: false,
                include_archived: false,
                limit: None,
                offset: None,
            }))
            .ok()
            .map(|page| page.items)
            .unwrap_or_default();
        let items = page.into_iter().map(protocol_session_to_summary).collect();
        self.fill_session_tokens(items)
    }

    // ── workspace management (`/workspace`, startup pick) ──

    fn list_workspaces(&self) -> Vec<WorkspaceSummary> {
        self.runtime
            .block_on(self.engine.workspace_list(false))
            .unwrap_or_default()
    }

    fn current_workspace(&self) -> String {
        self.current_workspace_id().to_string()
    }

    fn session_cwd(&self) -> Option<String> {
        Some(self.exec_cwd.lock().unwrap().clone())
    }

    fn workspace_sessions(&self, workspace_id: &str) -> Vec<SessionSummary> {
        let Ok(workspace_id) = workspace_id.parse() else {
            return Vec::new();
        };
        let page = self
            .runtime
            .block_on(self.engine.session_list(SessionListReq {
                workspace: WorkspaceSelector::Id { workspace_id },
                include_sub_agents: false,
                include_archived: false,
                limit: None,
                offset: None,
            }))
            .ok()
            .map(|page| page.items)
            .unwrap_or_default();
        page.into_iter().map(protocol_session_to_summary).collect()
    }

    fn switch_workspace(&self, workspace_id: &str, session_id: Option<&str>) -> Option<String> {
        let workspace_id = workspace_id.parse().ok()?;
        let session_id = match session_id {
            Some(raw) => Some(raw.parse().ok()?),
            None => None,
        };
        let opened = self
            .runtime
            .block_on(self.engine.session_open(SessionOpenReq {
                workspace: WorkspaceSelector::Id { workspace_id },
                session_id,
            }))
            .ok()?;
        *self.workspace_id.lock().unwrap() = workspace_id;
        let id = opened.session.session_id.to_string();
        self.set_session(&id);
        self.refresh_after_open(&opened);
        self.rearm_turn_pump(&id);
        Some(id)
    }

    fn search_sessions(&self, query: &str) -> Vec<SessionSummary> {
        let hits = self
            .runtime
            .block_on(self.engine.session_search(SessionSearchReq {
                workspace: Some(WorkspaceSelector::Id {
                    workspace_id: self.current_workspace_id(),
                }),
                session_id: None,
                query: query.to_string(),
                limit: Some(50),
            }))
            .unwrap_or_default();
        let items: Vec<SessionSummary> = hits
            .into_iter()
            .map(|hit| protocol_session_to_summary(hit.session))
            .collect();
        self.fill_session_tokens(items)
    }

    fn session_history(&self, id: &str) -> Vec<HistoryItem> {
        let Ok(session_id) = id.parse() else {
            return Vec::new();
        };
        let page = self
            .runtime
            .block_on(self.engine.session_transcript(TranscriptReq {
                session_id,
                after_turn_seq: None,
                offset: None,
                limit: Some(20),
            }))
            .ok()
            .map(|page| page.items)
            .unwrap_or_default();
        let mut items = Vec::new();
        let locale_store = self.session_i18n();
        let locale = locale_store.locale();
        for entry in page {
            let turn_index = entry.turn_seq as usize;
            match entry.body {
                zlogic_protocol::TranscriptBody::User { parts } => {
                    items.push(HistoryItem::Message {
                        id: entry.entry_id,
                        turn_index,
                        role: MessageRole::User,
                        message: Message {
                            parts: parts.iter().map(transcript_part_to_part).collect(),
                        },
                    });
                }
                zlogic_protocol::TranscriptBody::Text { text, .. } => {
                    items.push(HistoryItem::Message {
                        id: entry.entry_id,
                        turn_index,
                        role: MessageRole::Assistant,
                        message: Message::text(text),
                    });
                }
                zlogic_protocol::TranscriptBody::Reasoning { text, .. } => {
                    items.push(HistoryItem::Event {
                        id: entry.entry_id,
                        turn_index,
                        event: CoreEvent::ThinkingDelta { text, depth: None },
                    });
                }
                zlogic_protocol::TranscriptBody::ToolCall { calls } => {
                    for call in calls {
                        items.push(HistoryItem::Event {
                            id: format!("{}-{}", entry.entry_id, call.call_id),
                            turn_index,
                            event: CoreEvent::ToolCallStart {
                                id: call.call_id,
                                name: call.name,
                                args: serde_json::from_str(&call.args)
                                    .unwrap_or_else(|_| serde_json::json!({})),
                            },
                        });
                    }
                }
                zlogic_protocol::TranscriptBody::ToolResult {
                    call_id,
                    status,
                    summary,
                    ..
                } => items.push(HistoryItem::Event {
                    id: entry.entry_id,
                    turn_index,
                    event: CoreEvent::ToolCallEnd {
                        id: call_id,
                        ok: status == ToolStatus::Completed,
                        summary,
                    },
                }),
                zlogic_protocol::TranscriptBody::InteractionRequest {
                    interaction_id,
                    body,
                } => items.push(HistoryItem::Event {
                    id: entry.entry_id,
                    turn_index,
                    event: interaction_to_event(&interaction_id, &body, locale),
                }),
                zlogic_protocol::TranscriptBody::InteractionResponse { .. }
                | zlogic_protocol::TranscriptBody::Compaction { .. }
                | zlogic_protocol::TranscriptBody::TurnEnd { .. } => {}
                zlogic_protocol::TranscriptBody::Notice { level, message, .. } => {
                    items.push(HistoryItem::Event {
                        id: entry.entry_id,
                        turn_index,
                        event: CoreEvent::BusNotification {
                            level: match level {
                                zlogic_protocol::stream::NoticeLevel::Info => {
                                    NotificationLevel::Info
                                }
                                zlogic_protocol::stream::NoticeLevel::Warn => {
                                    NotificationLevel::Warning
                                }
                            },
                            title: locale_store.wire(&message),
                            message: String::new(),
                            source: Some("turn".into()),
                        },
                    })
                }
                zlogic_protocol::TranscriptBody::Steering { parts } => {
                    items.push(HistoryItem::Message {
                        id: entry.entry_id,
                        turn_index,
                        role: MessageRole::User,
                        message: Message {
                            parts: parts.iter().map(transcript_part_to_part).collect(),
                        },
                    });
                }
                zlogic_protocol::TranscriptBody::TaskUpdate { .. } => {}
            }
        }
        items
    }

    fn session_turns(
        &self,
        id: &str,
        after_turn_seq: Option<u32>,
        offset: Option<u32>,
        limit: Option<u32>,
    ) -> Vec<zlogic_protocol::query::TurnItem> {
        let Ok(session_id) = id.parse() else {
            return Vec::new();
        };
        let page = self.runtime.block_on(self.engine.session_turns(TurnsReq {
            session_id,
            after_turn_seq,
            offset,
            limit,
        }));
        match page {
            Ok(page) => page.items,
            Err(_) => Vec::new(),
        }
    }

    fn session_turn_entries(
        &self,
        id: &str,
        turn_seq: u32,
    ) -> Vec<zlogic_protocol::query::TranscriptEntry> {
        let Ok(session_id) = id.parse() else {
            return Vec::new();
        };
        let page = self
            .runtime
            .block_on(self.engine.session_entries(EntriesReq {
                session_id,
                turn_seq: Some(turn_seq),
                turn_id: None,
                role: None,
                offset: None,
                limit: None,
            }));
        match page {
            Ok(page) => page.items,
            Err(_) => Vec::new(),
        }
    }

    fn rename_session(&self, id: &str, title: &str) {
        let Ok(session_id) = id.parse() else {
            return;
        };
        let _ = self
            .runtime
            .block_on(self.engine.session_rename(SessionRenameReq {
                session_id,
                title: title.to_string(),
            }));
    }

    fn delete_sessions(&self, ids: &[String]) -> Option<String> {
        let current = self.current_session();
        let was_current = ids.iter().any(|id| id == &current);
        for id in ids {
            let Ok(session_id) = id.parse() else {
                continue;
            };
            let _ = self
                .runtime
                .block_on(self.engine.session_delete(session_id));
        }
        if was_current {
            let next = self.list_sessions().into_iter().next().map(|s| s.id);
            if let Some(next) = &next {
                self.set_session(next);
            }
            next
        } else {
            None
        }
    }

    fn reopen_session(&self, id: &str) {
        self.open_session(Some(id));
    }

    fn new_session(&self) -> Option<String> {
        self.open_session(None)
    }

    fn fork_history(&self, session_id: &str, history_id: &str) -> Option<String> {
        let keep = self.history_turn_seq(session_id, history_id)?;
        self.control(ProtoCommand::Fork {
            session_id: session_id.to_string(),
            keep_through_turn: keep,
        });
        let forked = self.list_sessions().into_iter().next().map(|s| s.id)?;
        self.set_session(&forked);
        Some(forked)
    }

    fn rewind_history(&self, session_id: &str, history_id: &str) {
        let Some(keep) = self.history_turn_seq(session_id, history_id) else {
            return;
        };
        self.control(ProtoCommand::Rewind {
            session_id: session_id.to_string(),
            keep_through_turn: keep,
        });
    }

    fn archive_session(&self, _id: &str) {}

    // ── plan mode ──

    fn plan_mode(&self) -> bool {
        *self.plan_enabled.lock().unwrap()
    }

    fn set_plan_mode(&self, enabled: bool) -> bool {
        *self.plan_enabled.lock().unwrap() = enabled;
        enabled
    }

    fn toggle_plan_mode(&self) -> bool {
        let mut plan = self.plan_enabled.lock().unwrap();
        *plan = !*plan;
        *plan
    }

    // ── config / keys ──

    fn config_snapshot(&self) -> ConfigView {
        let view = self.runtime.block_on(self.engine.config_get()).ok();
        ConfigView {
            providers: view
                .as_ref()
                .map(|v| v.providers.iter().map(|p| p.provider_id.clone()).collect())
                .unwrap_or_default(),
            current_model: view.and_then(|v| v.default_model),
        }
    }

    fn has_usable_model(&self) -> bool {
        self.model_catalog()
            .iter()
            .any(|model| model.key == KeyStatus::Present)
    }

    fn config_mutate(&self, op: ConfigOp) {
        match op {
            ConfigOp::AddModel { provider, model } => {
                let _ = self.add_model(&provider, &model, 0);
            }
            ConfigOp::RemoveModel { model } => {
                let _ = self.remove_model(&model);
            }
            ConfigOp::SetProvider {
                name,
                sdk,
                base_url,
            } => {
                let _ = self.update_provider(&name, &sdk, Some(&base_url));
            }
            ConfigOp::KeySet {
                provider,
                value,
                storage,
            } => {
                let _ = self.set_api_key(&provider, &value, &storage);
            }
            ConfigOp::KeyDelete { provider } => {
                let _ = self.delete_api_key(&provider);
            }
        }
    }

    fn set_language(&self, locale: &str) -> bool {
        *self.locale.lock().unwrap() = locale.to_string();
        *self
            .pump
            .locale
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = locale_of(locale, Locale::detect());
        true
    }

    fn command_catalog(&self) -> Vec<CommandSpec> {
        builtins()
    }
}

// ───────────────────────────── helpers ─────────────────────────────

impl EngineSession {
    fn fill_session_tokens(&self, mut items: Vec<SessionSummary>) -> Vec<SessionSummary> {
        let Some(summary) = self.usage_summary() else {
            return items;
        };
        for item in &mut items {
            if let Some(group) = summary.by_session.iter().find(|g| g.key == item.id) {
                item.input_tokens = group.tokens.input;
                item.output_tokens = group.tokens.output;
                item.cache_read_tokens = group.tokens.cache_read.unwrap_or(0);
                item.cache_write_tokens = group.tokens.cache_write.unwrap_or(0);
            }
        }
        items
    }

    fn usage_summary(&self) -> Option<zlogic_protocol::UsageSummary> {
        {
            let cache = self.usage_cache.lock().unwrap();
            if let Some((fetched_at, summary)) = cache.as_ref() {
                if fetched_at.elapsed() < USAGE_CACHE_TTL {
                    return Some(summary.clone());
                }
            }
        }
        let now = chrono::Utc::now();
        let summary = self
            .runtime
            .block_on(self.engine.usage_summary(UsageSummaryReq {
                workspace: Some(WorkspaceSelector::Id {
                    workspace_id: self.current_workspace_id(),
                }),
                session_id: None,
                self_only: false,
                session_kind: None,
                since: Some(now - chrono::Duration::days(30)),
                until: None,
                utc_offset_minutes: local_utc_offset_minutes(),
            }))
            .ok()?;
        *self.usage_cache.lock().unwrap() = Some((std::time::Instant::now(), summary.clone()));
        Some(summary)
    }

    /// Look up the `turn_seq` of a history entry by its entry id — engine
    /// fork/rewind address turns, the overlay addresses history rows.
    fn history_turn_seq(&self, session_id: &str, history_id: &str) -> Option<u32> {
        let Ok(session_id) = session_id.parse() else {
            return None;
        };
        let page = self
            .runtime
            .block_on(self.engine.session_transcript(TranscriptReq {
                session_id,
                after_turn_seq: None,
                offset: None,
                limit: Some(20),
            }))
            .ok()
            .map(|page| page.items)?;
        page.into_iter()
            .find(|entry| entry.entry_id == history_id)
            .map(|entry| entry.turn_seq)
    }

    fn upsert_provider(
        &self,
        name: &str,
        sdk: &str,
        base_url: Option<&str>,
        model_id: Option<String>,
    ) -> Result<(), String> {
        let base = base_url.map(str::to_string).or_else(|| {
            self.catalog()
                .and_then(|providers| providers.into_iter().find(|p| p.provider_id == name))
                .and_then(|p| p.base_url)
                .unwrap_or_default()
                .into()
        });
        let base = base.unwrap_or_default();
        let sdk = parse_sdk(sdk);
        self.runtime
            .block_on(
                self.engine
                    .config_upsert_openai_compatible(OpenAiCompatibleProviderReq {
                        provider_id: name.to_string(),
                        base_url: base,
                        model_id,
                        create_scope: None,
                        expected_revision: None,
                        sdk: Some(sdk),
                        ..empty_upsert()
                    }),
            )
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

fn empty_upsert() -> OpenAiCompatibleProviderReq {
    OpenAiCompatibleProviderReq {
        provider_id: String::new(),
        base_url: String::new(),
        model_id: None,
        rename_from: None,
        wire_model: None,
        display_name: None,
        context_window: None,
        max_output_tokens: None,
        sdk: None,
        generic: None,
        wiring: Default::default(),
        network: None,
        model_network: None,
        provider_default_params: None,
        model_default_params: None,
        no_think_params: None,
        vision: None,
        thinking: None,
        pricing: None,
        tier: None,
        create_scope: None,
        expected_revision: None,
    }
}

pub fn sdk_id(sdk: zlogic_protocol::Sdk) -> &'static str {
    use zlogic_protocol::Sdk;
    match sdk {
        Sdk::OpenAiGeneric => "openai_generic",
        Sdk::OpenAiChat => "openai_chat",
        Sdk::OpenAiResponses => "openai_responses",
        Sdk::DeepSeek => "deepseek",
        Sdk::Glm => "glm",
        Sdk::DashScope => "dashscope",
        Sdk::QwenLocal => "qwen_local",
        Sdk::OpenRouter => "openrouter",
        Sdk::Fireworks => "fireworks",
        Sdk::Anthropic => "anthropic",
        Sdk::Gemini => "gemini",
        Sdk::Bedrock => "bedrock",
    }
}

fn parse_sdk(s: &str) -> zlogic_protocol::Sdk {
    use zlogic_protocol::Sdk;
    let s = s.trim().to_lowercase().replace('-', "_");
    match s.as_str() {
        "openai_generic" | "openai_compatible" => Sdk::OpenAiGeneric,
        "openai_chat" | "openai" => Sdk::OpenAiChat,
        "openai_responses" => Sdk::OpenAiResponses,
        "deepseek" => Sdk::DeepSeek,
        "glm" | "zhipu" => Sdk::Glm,
        "dashscope" | "qwen" => Sdk::DashScope,
        "qwen_local" => Sdk::QwenLocal,
        "openrouter" => Sdk::OpenRouter,
        "fireworks" => Sdk::Fireworks,
        "anthropic" | "claude" => Sdk::Anthropic,
        "gemini" => Sdk::Gemini,
        "bedrock" | "aws" => Sdk::Bedrock,
        _ => Sdk::OpenAiGeneric,
    }
}

fn seed_mailbox(cache: &Mutex<Vec<MailboxEntry>>, opened: &zlogic_protocol::SessionOpened) {
    *cache.lock().unwrap() = opened
        .pending_submissions
        .iter()
        .map(|p| MailboxEntry {
            id: p.submission_id.clone(),
            messages: p
                .parts
                .iter()
                .map(|part| transcript_part_to_message(part.clone()))
                .collect(),
        })
        .collect();
}

/// cli-rs `MessagePart` (composer output) → protocol `MessagePart`.
/// Parts with no protocol equivalent degrade to text.
fn part_to_message_part(part: Part) -> Option<MessagePart> {
    match part {
        Part::Text { text } => Some(MessagePart::Text { text }),
        Part::File { path, .. } | Part::Directory { path, .. } | Part::Image { path, .. } => {
            Some(MessagePart::File { path })
        }
        Part::Url { url, display } => Some(MessagePart::Text {
            text: display.unwrap_or_else(|| url.clone()),
        }),
        Part::Command { name, text, .. } => Some(MessagePart::Text {
            text: format!("/{name} {text}").trim_end().to_string(),
        }),
    }
}

fn message_part_to_message(part: &MessagePart, i18n: &I18n) -> Message {
    let text_part = |text: &str| Message::text(text.to_string());
    match part {
        MessagePart::Text { text } => text_part(text),
        MessagePart::Skill { name, .. } => text_part(&format!("/{name}")),
        MessagePart::SkillLoad { name, .. } => text_part(&format!("/load {name}")),
        MessagePart::SkillInvocation { name, .. } => text_part(&format!("/{name}")),
        MessagePart::SkillUnload { name } => text_part(&format!("/unload {name}")),
        MessagePart::File { path } => Message::from_parts(vec![Part::File {
            path: path.clone(),
            display: path.clone(),
            preview: None,
        }]),
        MessagePart::Attachment { name, .. } => Message::from_parts(vec![Part::File {
            path: String::new(),
            display: name.clone(),
            preview: None,
        }]),
        MessagePart::TaskUpdate { update } => text_part(
            &update
                .summary
                .clone()
                .unwrap_or_else(|| i18n.t("task-update-fallback")),
        ),
    }
}

fn protocol_session_to_summary(session: zlogic_protocol::SessionSummary) -> SessionSummary {
    let anchor = session.last_message_at.unwrap_or(session.created_at);
    SessionSummary {
        id: session.session_id.to_string(),
        title: session.title.unwrap_or_else(|| "New session".into()),
        renamed: matches!(
            session.title_source,
            Some(zlogic_protocol::TitleSource::User)
        ),
        group: relative_group(anchor),
        when: relative_when(anchor),
        cost: 0.0,
        model: session.model_ref.unwrap_or_default(),
        archived: session.archived_at.is_some(),
        ..Default::default()
    }
}

fn usage_groups_to_models(groups: &[zlogic_protocol::UsageGroup]) -> Vec<ModelUsage> {
    let total: f64 = groups
        .iter()
        .map(|g| g.tokens.input + g.tokens.output)
        .sum::<u64>() as f64;
    groups
        .iter()
        .map(|group| {
            let tokens = group.tokens.input + group.tokens.output;
            ModelUsage {
                model: group.key.clone(),
                tokens,
                share: if total > 0.0 {
                    tokens as f64 / total
                } else {
                    0.0
                },
                cost: group.cost.as_ref().map(|c| c.amount).unwrap_or(0.0),
                ttft_p50_ms: group.avg_first_token_ms.map(|ms| ms as u32),
                throughput_tok_s: group.avg_response_ms.map(|ms| {
                    if ms > 0 {
                        tokens as f64 / (ms as f64 / 1000.0)
                    } else {
                        0.0
                    }
                }),
            }
        })
        .collect()
}

fn local_utc_offset_minutes() -> i32 {
    use chrono::Offset;
    chrono::Local::now().offset().fix().local_minus_utc() / 60
}

/// "Today" / "Yesterday" / "This week" — the shared session-grouping vocabulary.
fn relative_group(at: chrono::DateTime<chrono::Utc>) -> String {
    use chrono::Datelike;
    let now = chrono::Utc::now();
    let local_at = at.with_timezone(&chrono::Local);
    let local_now = now.with_timezone(&chrono::Local);
    if local_at.date_naive() == local_now.date_naive() {
        "Today".into()
    } else if local_at.date_naive() == local_now.date_naive() - chrono::Days::new(1) {
        "Yesterday".into()
    } else if local_at.date_naive().iso_week() == local_now.date_naive().iso_week()
        && local_at.date_naive().year() == local_now.date_naive().year()
    {
        "This week".into()
    } else {
        "Earlier".into()
    }
}

/// "14:02" for today, "Mon" / "07-20" otherwise (matches the old line CLI's `ago`).
fn relative_when(at: chrono::DateTime<chrono::Utc>) -> String {
    let now = chrono::Utc::now();
    let local_at = at.with_timezone(&chrono::Local);
    let local_now = now.with_timezone(&chrono::Local);
    if local_at.date_naive() == local_now.date_naive() {
        local_at.format("%H:%M").to_string()
    } else if at.date_naive() >= now.date_naive() - chrono::Days::new(7) {
        local_at.format("%a").to_string()
    } else {
        local_at.format("%m-%d").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_model_ref_splits_on_last_colon() {
        assert_eq!(
            EngineSession::split_model_ref("anthropic:claude-sonnet-4-5"),
            Some(("anthropic", "claude-sonnet-4-5"))
        );
        assert_eq!(EngineSession::split_model_ref("nope"), None);
    }

    #[test]
    fn json_answers_map_to_field_values() {
        assert_eq!(
            json_to_field_value(&serde_json::json!("hi")),
            Some(FieldValue::Text("hi".into()))
        );
        assert_eq!(
            json_to_field_value(&serde_json::json!(true)),
            Some(FieldValue::Bool(true))
        );
        assert_eq!(
            json_to_field_value(&serde_json::json!(["a", "b"])),
            Some(FieldValue::Choices(vec!["a".into(), "b".into()]))
        );
        assert_eq!(json_to_field_value(&serde_json::json!(null)), None);
    }

    #[test]
    fn always_scope_maps_to_the_durable_global_grant() {
        let decision = answer_to_decision(&Answer::Allow {
            scope: GrantScope::Always,
        });
        assert!(matches!(
            decision,
            InteractionDecision::Allow {
                scope: ProtoScope::Global,
                ..
            }
        ));
    }

    #[test]
    fn session_group_uses_the_list_sort_key_not_updated_at() {
        use chrono::{DateTime, Utc};
        use zlogic_protocol::query::SessionSummary as ProtoSummary;

        let now = DateTime::<Utc>::from(std::time::SystemTime::now());
        let base = ProtoSummary {
            session_id: zlogic_protocol::SessionId::new(),
            workspace_id: zlogic_protocol::WorkspaceId::new(),
            agent_paths: vec!["main".into()],
            root_session_id: zlogic_protocol::SessionId::new(),
            title: None,
            title_source: None,
            model_ref: None,
            effort: None,
            created_at: now - chrono::Days::new(30),
            updated_at: now, // opened today — must not affect grouping
            last_message_at: Some(now - chrono::Days::new(4)),
            turn_count: 2,
            live_turn_id: None,
            awaiting_input: false,
            archived_at: None,
        };
        let out = protocol_session_to_summary(base.clone());
        let anchor = base.last_message_at.unwrap();
        assert_eq!(
            out.group,
            relative_group(anchor),
            "grouping must follow the last message time"
        );
        assert_eq!(
            out.when,
            relative_when(anchor),
            "the displayed time must follow the last message time"
        );
        assert_ne!(
            out.group, "Today",
            "updated_at is today but the last message was 4 days ago"
        );

        let silent = ProtoSummary {
            last_message_at: None,
            ..base
        };
        let out = protocol_session_to_summary(silent);
        assert_eq!(out.group, relative_group(base.created_at));
        assert_eq!(
            out.group, "Earlier",
            "a session created 30 days ago that was never spoken in goes to Earlier"
        );
    }
}
