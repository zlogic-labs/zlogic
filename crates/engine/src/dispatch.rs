//! ```text
//! ```

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures_util::FutureExt;
use zlogic_core::{
    CancellationToken, Core, CoreServices, CoreSpawner, EventSink, SharedStore, TurnInput, TurnPlan,
};
use zlogic_hooks::HookRunner;
use zlogic_objects::ObjectId;
use zlogic_protocol::input::{Delivery as InputDelivery, MessagePart};
use zlogic_protocol::query::{ApiResult, PendingInteraction, TurnPhase, TurnState};
use zlogic_protocol::stream::{
    NoticeLevel, StateChange, StateNotice, StreamEvent, TurnStats, TurnStatus,
};
use zlogic_protocol::usage::Purpose;
use zlogic_protocol::{
    Command, Effort, SessionId, Submission, SubmitAck, ThinkingIntent, ThinkingMode, TurnId,
    WorkspaceId,
};
use zlogic_store::Delivery;

use crate::hub::EventHub;
use crate::interaction::InteractionRouter;
use crate::lock::SessionLocks;
use crate::router::ModelRouter;
use crate::service::TurnService;
use crate::{EngineError, Result};

/// Resolves the `model` argument of `create_agent` through the engine's router.
/// The router owns the candidate chain, credentials and the LLM factory; this adapter exposes it
/// through core's [`zlogic_core::ModelResolver`] boundary so the spawner can resolve a custom
/// agent's model without knowing what a router is.
pub struct RouterModelResolver {
    router: Arc<ModelRouter>,
}

impl RouterModelResolver {
    pub fn new(router: Arc<ModelRouter>) -> Self {
        Self { router }
    }
}

#[async_trait]
impl zlogic_core::ModelResolver for RouterModelResolver {
    async fn resolve(
        &self,
        model_ref: &str,
    ) -> std::result::Result<
        (
            zlogic_protocol::config::ResolvedModel,
            Arc<dyn zlogic_llm::LlmClient>,
            zlogic_protocol::llm::ThinkingIntent,
        ),
        String,
    > {
        // A custom agent's model is its own role: a `Purpose::Agent(name)` with the reference
        // pinned as the session model gives the chain a concrete anchor, exactly as a profile
        // name would resolve it.
        let routed = self
            .router
            .resolve(
                &zlogic_protocol::usage::Purpose::Agent("custom".into()),
                Some(model_ref),
            )
            .map_err(|error| error.to_string())?;
        Ok((routed.model, routed.client, routed.thinking))
    }
}

pub struct HubSink(pub Arc<EventHub>);

impl EventSink for HubSink {
    fn emit(&self, event: StreamEvent) {
        self.0.emit(event);
    }
}

fn panic_message(panic: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else if let Some(s) = panic.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else {
        "(non-string panic payload)".to_string()
    }
}

#[derive(Clone)]
pub struct LiveTurn {
    pub turn_id: TurnId,
    pub session_id: SessionId,
    pub cancel: CancellationToken,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub status: Option<TurnStatus>,
    pub stats: TurnStats,
}

impl LiveTurn {
    fn phase(&self, awaiting_input: bool) -> TurnPhase {
        match (self.ended_at.is_some(), awaiting_input) {
            (true, _) => TurnPhase::Ended,
            (false, true) => TurnPhase::AwaitingInput,
            (false, false) => TurnPhase::Running,
        }
    }
}

#[derive(Default)]
pub struct TurnRegistry {
    turns: Mutex<HashMap<TurnId, LiveTurn>>,
}

impl TurnRegistry {
    pub fn get(&self, turn_id: TurnId) -> Option<LiveTurn> {
        self.lock().get(&turn_id).cloned()
    }

    pub fn live_of(&self, session_id: SessionId) -> Option<LiveTurn> {
        self.lock()
            .values()
            .find(|t| t.session_id == session_id && t.ended_at.is_none())
            .cloned()
    }

    fn insert(&self, turn: LiveTurn) {
        let mut turns = self.lock();
        turns.retain(|_, t| t.session_id != turn.session_id || t.ended_at.is_none());
        turns.insert(turn.turn_id, turn);
    }

    fn finish(&self, turn_id: TurnId, status: TurnStatus, stats: TurnStats) {
        if let Some(t) = self.lock().get_mut(&turn_id) {
            t.ended_at = Some(Utc::now());
            t.status = Some(status);
            t.stats = stats;
        }
    }

    pub fn live_count(&self) -> usize {
        self.lock()
            .values()
            .filter(|t| t.ended_at.is_none())
            .count()
    }

    pub fn cancel_live(&self) -> usize {
        let mut cancelled = 0;
        for turn in self.lock().values() {
            if turn.ended_at.is_none() {
                turn.cancel.cancel();
                cancelled += 1;
            }
        }
        cancelled
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<TurnId, LiveTurn>> {
        self.turns.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

pub trait WorkspaceRoots: Send + Sync {
    fn root_of(&self, workspace_id: WorkspaceId) -> Option<PathBuf>;

    fn tools_of(&self, _workspace_id: WorkspaceId) -> Option<Vec<String>> {
        None
    }
}

#[derive(Default)]
pub struct RegisteredRoots {
    roots: Mutex<HashMap<WorkspaceId, PathBuf>>,
}

impl RegisteredRoots {
    pub fn with(workspace_id: WorkspaceId, root: impl Into<PathBuf>) -> Self {
        let me = Self::default();
        me.register(workspace_id, root);
        me
    }

    pub fn register(&self, workspace_id: WorkspaceId, root: impl Into<PathBuf>) {
        self.roots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(workspace_id, root.into());
    }
}

impl WorkspaceRoots for RegisteredRoots {
    fn root_of(&self, workspace_id: WorkspaceId) -> Option<PathBuf> {
        self.roots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&workspace_id)
            .cloned()
    }
}

#[derive(Clone)]
pub struct Dispatcher {
    store: SharedStore,
    hub: Arc<EventHub>,
    router: Arc<ModelRouter>,
    locks: Arc<SessionLocks>,
    services: Arc<CoreServices>,
    roots: Arc<dyn WorkspaceRoots>,
    interactions: Arc<dyn InteractionRouter>,
    registry: Arc<TurnRegistry>,
    delivered: Arc<Mutex<DeliveredKeys>>,
    agents: Arc<Vec<String>>,
    worktrees: Option<Arc<crate::Worktrees>>,
    prompts: Option<Arc<crate::SystemPrompts>>,
    skills: Option<Arc<crate::SkillLibrary>>,
    extensions: Option<Arc<crate::Extensions>>,
    /// Non-conversational calls such as automatic title refinement.
    auxiliary: Option<Arc<crate::Auxiliary>>,
    dirs: Option<zlogic_config::Dirs>,
}

#[derive(Default)]
struct DeliveredKeys {
    seen: HashMap<(SessionId, String), String>,
    order: std::collections::VecDeque<(SessionId, String)>,
}

impl DeliveredKeys {
    const CAP: usize = 256;

    fn remember(&mut self, session: SessionId, request_id: &str, submission_id: String) {
        let key = (session, request_id.to_string());
        if self.seen.insert(key.clone(), submission_id).is_none() {
            self.order.push_back(key);
            if self.order.len() > Self::CAP
                && let Some(old) = self.order.pop_front()
            {
                self.seen.remove(&old);
            }
        }
    }

    fn get(&self, session: SessionId, request_id: &str) -> Option<String> {
        self.seen.get(&(session, request_id.to_string())).cloned()
    }
}

impl Dispatcher {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: SharedStore,
        hub: Arc<EventHub>,
        router: Arc<ModelRouter>,
        locks: Arc<SessionLocks>,
        services: Arc<CoreServices>,
        roots: Arc<dyn WorkspaceRoots>,
        interactions: Arc<dyn InteractionRouter>,
    ) -> Self {
        Self {
            store,
            hub,
            router,
            locks,
            services,
            roots,
            interactions,
            registry: Arc::new(TurnRegistry::default()),
            delivered: Arc::new(Mutex::new(DeliveredKeys::default())),
            agents: Arc::new(Vec::new()),
            worktrees: None,
            prompts: None,
            skills: None,
            extensions: None,
            auxiliary: None,
            dirs: None,
        }
    }

    pub fn with_dirs(mut self, dirs: zlogic_config::Dirs) -> Self {
        self.dirs = Some(dirs);
        self
    }

    pub fn with_agents(mut self, names: Vec<String>) -> Self {
        self.agents = Arc::new(names);
        self
    }

    pub fn with_worktrees(mut self, worktrees: Arc<crate::Worktrees>) -> Self {
        self.worktrees = Some(worktrees);
        self
    }

    pub fn with_skills(mut self, skills: Arc<crate::SkillLibrary>) -> Self {
        self.skills = Some(skills);
        self
    }

    pub fn with_prompts(mut self, prompts: Arc<crate::SystemPrompts>) -> Self {
        self.prompts = Some(prompts);
        self
    }

    pub fn with_extensions(mut self, extensions: Arc<crate::Extensions>) -> Self {
        self.extensions = Some(extensions);
        self
    }

    pub fn with_auxiliary(mut self, auxiliary: Arc<crate::Auxiliary>) -> Self {
        self.auxiliary = Some(auxiliary);
        self
    }

    pub fn registry(&self) -> &Arc<TurnRegistry> {
        &self.registry
    }

    /// Copies an uploaded object into the path-based attachment contract used by models and tools.
    /// The object store's shard path is deliberately not exposed: it is an implementation detail,
    /// may be remote, and does not preserve the original extension that format readers need.
    async fn materialize_attachment(
        &self,
        session_id: SessionId,
        object_id: ObjectId,
        name: &str,
        bytes: u64,
    ) -> std::result::Result<PathBuf, String> {
        let directory = self.services.attachment_dir.join(session_id.to_string());
        let object_key = safe_attachment_component(&object_id.to_string());
        let file_name = safe_attachment_component(name);
        let target = directory.join(format!("{object_key}-{file_name}"));
        if target
            .metadata()
            .map(|metadata| metadata.is_file() && metadata.len() == bytes)
            .unwrap_or(false)
        {
            return Ok(target);
        }

        let objects = self.services.objects.clone();
        tokio::task::spawn_blocking(move || {
            fs::create_dir_all(&directory).map_err(|error| {
                format!(
                    "failed to create attachment directory {}: {error}",
                    directory.display()
                )
            })?;
            restrict_attachment_permissions(&directory, true)?;
            let temporary = directory.join(format!(
                ".{}.{}.tmp",
                target
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or("attachment"),
                TurnId::new()
            ));
            let result = (|| {
                let mut source = objects.open(&object_id).map_err(|error| {
                    format!("attachment object {object_id} could not be read: {error}")
                })?;
                let file = OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&temporary)
                    .map_err(|error| {
                        format!(
                            "failed to create attachment temp file {}: {error}",
                            temporary.display()
                        )
                    })?;
                let mut destination = BufWriter::new(file);
                let copied = std::io::copy(&mut source, &mut destination).map_err(|error| {
                    format!("failed to write attachment {}: {error}", target.display())
                })?;
                destination.flush().map_err(|error| {
                    format!("failed to flush attachment {}: {error}", target.display())
                })?;
                let file = destination.into_inner().map_err(|error| {
                    format!(
                        "failed to finalize attachment {}: {error}",
                        target.display()
                    )
                })?;
                file.sync_all().map_err(|error| {
                    format!("failed to sync attachment {}: {error}", target.display())
                })?;
                if copied != bytes {
                    return Err(format!(
                        "attachment object {object_id} size mismatch: expected {bytes} bytes, \
                         read {copied} bytes"
                    ));
                }
                restrict_attachment_permissions(&temporary, false)?;
                if target
                    .metadata()
                    .map(|metadata| metadata.is_file() && metadata.len() == bytes)
                    .unwrap_or(false)
                {
                    fs::remove_file(&temporary).map_err(|error| {
                        format!(
                            "failed to clean up duplicate attachment temp file {}: {error}",
                            temporary.display()
                        )
                    })?;
                    return Ok(target.clone());
                }
                if target.exists() {
                    fs::remove_file(&target).map_err(|error| {
                        format!("failed to replace attachment {}: {error}", target.display())
                    })?;
                }
                fs::rename(&temporary, &target).map_err(|error| {
                    format!(
                        "failed to commit attachment {} -> {}: {error}",
                        temporary.display(),
                        target.display()
                    )
                })?;
                Ok(target.clone())
            })();
            if result.is_err() {
                let _ = fs::remove_file(&temporary);
            }
            result
        })
        .await
        .map_err(|error| format!("attachment materialization task failed: {error}"))?
    }

    async fn normalize_attachments(
        &self,
        session_id: SessionId,
        parts: Vec<MessagePart>,
    ) -> std::result::Result<Vec<MessagePart>, String> {
        let mut normalized = Vec::with_capacity(parts.len());
        for part in parts {
            if matches!(
                part,
                MessagePart::SkillLoad { .. } | MessagePart::SkillInvocation { .. }
            ) {
                return Err(
                    "skill_load and skill_invocation can only be produced by the engine".into(),
                );
            }
            let MessagePart::Attachment {
                object_id,
                name,
                mime_type,
                ..
            } = part
            else {
                normalized.push(part);
                continue;
            };
            let id: ObjectId = object_id
                .parse()
                .map_err(|_| format!("invalid attachment object id: {object_id}"))?;
            let size = self
                .services
                .objects
                .size(&id)
                .map_err(|_| format!("attachment object missing or unreadable: {object_id}"))?;
            let clean_name = name.trim();
            if clean_name.is_empty() || clean_name.chars().count() > 255 {
                return Err("attachment name must be non-empty and at most 255 characters".into());
            }
            let mime_type = normalize_mime(&mime_type);
            let materialized_name = attachment_name_with_extension(clean_name, &mime_type);
            let path = self
                .materialize_attachment(session_id, id, &materialized_name, size)
                .await?;
            tracing::debug!(
                target: "zlogic::engine",
                attachment = %path.display(),
                mime = %mime_type,
                "uploaded attachment materialized as a user file"
            );
            normalized.push(MessagePart::File {
                path: path.display().to_string(),
            });
        }
        Ok(normalized)
    }

    fn assemble_turn_prompt(
        &self,
        plan: &mut TurnPlan,
        tools: Option<&zlogic_tools::ToolRegistry>,
        workspace_id: WorkspaceId,
        root: &Path,
        exec_cwd: &Path,
        unavailable_mcp: &[String],
    ) {
        let Some(prompts) = &self.prompts else {
            return;
        };
        let effective_registry = tools.unwrap_or(&self.services.tools);
        let mut names = effective_registry.available_names();
        if let Some(allow) = &plan.tools_allow {
            names.retain(|name| allow.contains(name));
        }
        let allows_mcp = plan
            .tools_allow
            .as_ref()
            .is_none_or(|allow| allow.iter().any(|name| name.starts_with("mcp__")));
        // Reading durable context is independent of permission to mutate it. A Chat-only
        // workspace can omit `memory_update` and must still benefit from memories written
        // elsewhere; only the write protocol is gated by tool visibility.
        let global_memory = self
            .store
            .with(|db| {
                db.memories().list(&zlogic_protocol::MemoryListReq {
                    scope: zlogic_protocol::MemoryScope::Global,
                    workspace_id: None,
                    include_removed: false,
                })
            })
            .unwrap_or_else(|error| {
                plan.notices.push(zlogic_core::PlanNotice::warn(
                    "memory.read_global",
                    error.to_string(),
                ));
                Vec::new()
            });
        let workspace_memory = self
            .store
            .with(|db| {
                db.memories().list(&zlogic_protocol::MemoryListReq {
                    scope: zlogic_protocol::MemoryScope::Workspace,
                    workspace_id: Some(workspace_id),
                    include_removed: false,
                })
            })
            .unwrap_or_else(|error| {
                plan.notices.push(zlogic_core::PlanNotice::warn(
                    "memory.read_workspace",
                    error.to_string(),
                ));
                Vec::new()
            });
        let tool_guidance = effective_registry.prompt_guidance(&names);
        let effectful = effective_registry.any_effectful(&names);
        plan.system = prompts.build(crate::PromptRequest {
            workspace_id,
            root,
            exec_cwd,
            tools: &names,
            tool_guidance: &tool_guidance,
            unavailable_mcp,
            allows_mcp,
            effectful,
            global_memory: &global_memory,
            workspace_memory: &workspace_memory,
        });
    }

    /// Starts a standalone manual compaction. Unlike a submission, this has no mailbox row and
    /// never asks the main model for a conversational response.
    fn start_manual_compaction(&self, session_id: SessionId) -> Result<TurnId> {
        let session = self.store.with(|db| db.sessions().get(session_id))?;
        let root = self.roots.root_of(session.workspace_id).ok_or_else(|| {
            EngineError::Invalid(format!(
                "workspace {} has no registered root",
                session.workspace_id
            ))
        })?;
        let turn_id = TurnId::new();
        let cancel = CancellationToken::new();
        let guard = self.locks.claim(session_id, turn_id, cancel.clone())?;

        let turn_model_ref = session.model_ref.as_deref();
        let routed = self.router.resolve(&Purpose::Main, turn_model_ref)?;
        let mut plan =
            TurnPlan::new(routed.model.clone(), routed.client.clone()).for_purpose(Purpose::Main);
        if let Ok(aux) = self.router.resolve(&Purpose::Compaction, turn_model_ref) {
            plan = plan.with_compaction_model(aux.aux());
        }
        // The workspace's tool allowlist, exactly as a submission gets it. The summary call itself
        // may not need these tools, but the tool *definitions* are part of the conversation's
        // cached prompt prefix, and the prefix has to be assembled the same way for the provider
        // to recognise it.
        plan = plan.with_tools(self.roots.tools_of(session.workspace_id));
        let hooks: Option<Arc<dyn zlogic_hooks::HookRunner>> =
            self.dirs.as_ref().and_then(|dirs| {
                match zlogic_hooks::CommandHookRunner::load(&dirs.config.join("hooks.yaml"), &root)
                {
                    Ok(runner) if !runner.is_empty() => {
                        Some(runner as Arc<dyn zlogic_hooks::HookRunner>)
                    }
                    Ok(_) => None,
                    Err(error) => {
                        tracing::warn!(
                            target: "zlogic::engine",
                            "failed to load hooks for manual compaction: {error}"
                        );
                        None
                    }
                }
            });

        self.registry.insert(LiveTurn {
            turn_id,
            session_id,
            cancel: cancel.clone(),
            started_at: Utc::now(),
            ended_at: None,
            status: None,
            stats: TurnStats::default(),
        });
        self.hub.notify(StateNotice {
            session_id: session_id.to_string(),
            turn_id: Some(turn_id.to_string()),
            change: StateChange::TurnStateChanged,
        });

        let services = self.services.clone();
        let registry = self.registry.clone();
        let hub = self.hub.clone();
        let next_dispatch = self.clone();
        let extensions = self.extensions.clone();
        let workspace_id = session.workspace_id;
        let exec_dir = session.exec_cwd_or(&root).to_path_buf();
        let context_window = routed.model.context_window;
        tokio::spawn(async move {
            // The same assembly a submitting turn does, and for the same reason it is done *here*
            // rather than before the spawn: an extension's tool set is read from disk and may open
            // a connection, which is async, while `start_manual_compaction` has to return at once.
            // It matters more than symmetry: the summary call rides the conversation's cached
            // prompt prefix, so the tool set and the system prompt have to come out identical to a
            // submission's. Assembled differently, the prefix misses the cache and the whole
            // conversation is re-billed at full price — the exact cost this path avoids.
            let mut plan = plan;
            let mut unavailable_mcp: Vec<String> = Vec::new();
            let tools = match &extensions {
                Some(ext) => {
                    let assembled = ext
                        .tools_for(workspace_id, &root, &services.tools, Some(context_window))
                        .await;
                    plan.notices.extend(assembled.notices);
                    unavailable_mcp = assembled.unavailable;
                    Some(assembled.tools)
                }
                None => None,
            };
            next_dispatch.assemble_turn_prompt(
                &mut plan,
                tools.as_ref(),
                workspace_id,
                &root,
                &exec_dir,
                &unavailable_mcp,
            );
            // A summary is a real request against a real model, so it is accounted for like one —
            // including the limits. `compact` charges the aux call through `plan.budget`.
            let budget = Arc::new(next_dispatch.turn_budget(&root, session_id, &mut plan));
            plan.budget = Some(budget.clone());

            let core = Core::new(
                services,
                session_id,
                root,
                Arc::new(HubSink(hub.clone())) as Arc<dyn EventSink>,
            )
            .with_tools(tools)
            .with_hooks(hooks);
            let outcome =
                std::panic::AssertUnwindSafe(core.compact_context(turn_id, plan, cancel.clone()))
                    .catch_unwind()
                    .await;
            drop(guard);

            match outcome {
                Ok(Ok(out)) => registry.finish(turn_id, out.status, out.stats),
                Ok(Err(error)) => {
                    tracing::error!(
                        target: "zlogic::engine",
                        %session_id,
                        "manual compaction of context failed: {error}"
                    );
                    registry.finish(turn_id, TurnStatus::Failed, TurnStats::default());
                }
                Err(panic) => {
                    cancel.cancel();
                    tracing::error!(
                        target: "zlogic::engine",
                        %session_id,
                        %turn_id,
                        end = "manual compaction panicked; ending the turn as failed",
                        panic = %panic_message(&panic),
                    );
                    registry.finish(turn_id, TurnStatus::Failed, TurnStats::default());
                }
            }
            hub.notify(StateNotice {
                session_id: session_id.to_string(),
                turn_id: Some(turn_id.to_string()),
                change: StateChange::TurnStateChanged,
            });
            if let Err(error) = next_dispatch.start_if_idle(session_id) {
                tracing::warn!(
                    target: "zlogic::engine",
                    %session_id,
                    "failed to start the queued message after manual compaction: {error}"
                );
                write_session_notice(
                    &next_dispatch.services,
                    session_id,
                    None,
                    "turn_start_failed",
                    &zlogic_protocol::LocalizedMessage::new(
                        "notice.turn_start_failed",
                        format!(
                            "A queued message could not start a reply: {error}. Configure a model and an API key, then try again."
                        ),
                    )
                    .arg("error", error.to_string()),
                );
            }
        });
        Ok(turn_id)
    }

    pub fn start_if_idle(&self, session_id: SessionId) -> Result<Option<TurnId>> {
        let pending = self.store.with(|db| db.mailbox().pending(session_id))?;
        if pending.is_empty() {
            return Ok(None);
        }

        let session = self.store.with(|db| db.sessions().get(session_id))?;
        let root = self.roots.root_of(session.workspace_id).ok_or_else(|| {
            EngineError::Invalid(format!(
                "workspace {} has no registered root",
                session.workspace_id
            ))
        })?;

        let turn_id = TurnId::new();
        let cancel = CancellationToken::new();

        let guard = match self.locks.claim(session_id, turn_id, cancel.clone()) {
            Ok(g) => g,
            Err(EngineError::Busy { .. }) => return Ok(None),
            Err(e) => return Err(e),
        };

        // The first explicitly pinned queued submission owns this turn's model snapshot. Rows
        // generated by the runtime carry no preference and must not hide a later user selection.
        // Session selection is only the fallback when this batch has no pin at all.
        let turn_model_ref = pending
            .iter()
            .find_map(|record| record.model_ref.as_deref())
            .or(session.model_ref.as_deref());
        let routed = self.router.resolve(&Purpose::Main, turn_model_ref)?;
        let mut plan =
            TurnPlan::new(routed.model.clone(), routed.client.clone()).for_purpose(Purpose::Main);
        let pinned_thinking = pending.iter().find_map(|record| {
            record
                .thinking
                .as_ref()
                .and_then(|value| serde_json::from_value::<ThinkingIntent>(value.clone()).ok())
                .filter(|intent| intent.mode != ThinkingMode::Default)
        });
        plan.thinking = match pinned_thinking {
            Some(intent) => intent,
            None => match session.effort.as_deref().and_then(Effort::from_wire) {
                Some(effort) => ThinkingIntent {
                    mode: ThinkingMode::On,
                    effort: Some(effort),
                    budget_tokens: None,
                },
                None => routed.thinking,
            },
        };
        plan = plan.with_tools(self.roots.tools_of(session.workspace_id));
        if let Ok(aux) = self.router.resolve(&Purpose::Compaction, turn_model_ref) {
            plan = plan.with_compaction_model(aux.aux());
        }
        let hooks: Option<Arc<dyn zlogic_hooks::HookRunner>> =
            self.dirs.as_ref().and_then(|dirs| {
                match zlogic_hooks::CommandHookRunner::load(&dirs.config.join("hooks.yaml"), &root)
                {
                    Ok(runner) if !runner.is_empty() => {
                        Some(runner as Arc<dyn zlogic_hooks::HookRunner>)
                    }
                    Ok(_) => None,
                    Err(error) => {
                        plan.notices.push(zlogic_core::PlanNotice::warn(
                            "hook_config_invalid",
                            error.to_string(),
                        ));
                        None
                    }
                }
            });

        let mut input = Vec::new();
        let mut consumed: Vec<(zlogic_protocol::SubmissionId, String)> = Vec::new();
        for record in &pending {
            let parts = zlogic_core::steer::decode_parts(&record.parts);
            if parts.is_empty() {
                self.store
                    .with(|db| db.mailbox().cancel(record.submission_id))?;
                continue;
            }
            input.extend(parts.into_iter().map(|part| match part {
                zlogic_core::steer::DecodedPart::User(part) => TurnInput::Submitted(part),
                zlogic_core::steer::DecodedPart::TaskUpdate(update) => {
                    TurnInput::TaskUpdate(update)
                }
            }));
            consumed.push((record.submission_id, record.client_request_id.clone()));
        }
        if input.is_empty() {
            return Ok(None);
        }

        let target_dir = session.exec_cwd_or(&root).to_path_buf();

        let worktree = self
            .worktrees
            .as_ref()
            .map(|w| w.host(session_id, root.clone()) as Arc<dyn zlogic_tools::WorktreeHost>);

        self.registry.insert(LiveTurn {
            turn_id,
            session_id,
            cancel: cancel.clone(),
            started_at: Utc::now(),
            ended_at: None,
            status: None,
            stats: TurnStats::default(),
        });

        for (submission_id, request_id) in &consumed {
            self.store.with(|db| db.mailbox().cancel(*submission_id))?;
            self.delivered
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remember(session_id, request_id, submission_id.to_string());
        }
        self.hub.notify(StateNotice {
            session_id: session_id.to_string(),
            turn_id: Some(turn_id.to_string()),
            change: StateChange::TurnStateChanged,
        });

        let registry = self.registry.clone();
        let hub = self.hub.clone();
        let session_ref = session_id;
        let services = self.services.clone();
        let extensions = self.extensions.clone();
        let workspace_id = session.workspace_id;
        let agent_names = self.agents.clone();
        let context_window = routed.model.context_window;
        let router = self.router.clone();
        let session_model = turn_model_ref.map(str::to_owned);
        let skills = self
            .skills
            .as_ref()
            .map(|lib| lib.host(workspace_id, root.clone()) as Arc<dyn zlogic_tools::SkillHost>);
        let skill_library = self.skills.clone();
        let exec_dir = target_dir.clone();
        let next_dispatch = self.clone();
        tokio::spawn(async move {
            tracing::info!(
                target: "zlogic::engine",
                %session_ref,
                %turn_id,
                "turn task started"
            );
            let mut plan = plan;
            let mut unavailable_mcp: Vec<String> = Vec::new();
            let tools = match &extensions {
                Some(ext) => {
                    let assembled = ext
                        .tools_for(workspace_id, &root, &services.tools, Some(context_window))
                        .await;
                    plan.notices.extend(assembled.notices);
                    unavailable_mcp = assembled.unavailable;
                    if assembled.extension_tools > 0 {
                        tracing::debug!(
                            target: "zlogic::engine",
                            "extensions contributed {} tools (~{} tokens / turn)",
                            assembled.extension_tools,
                            assembled.extension_tokens
                        );
                    }
                    Some(assembled.tools)
                }
                None => None,
            };

            next_dispatch.assemble_turn_prompt(
                &mut plan,
                tools.as_ref(),
                workspace_id,
                &root,
                &exec_dir,
                &unavailable_mcp,
            );

            let budget = Arc::new(next_dispatch.turn_budget(&root, session_id, &mut plan));
            plan.budget = Some(budget.clone());

            let core = Core::new(
                services.clone(),
                session_id,
                root.clone(),
                Arc::new(HubSink(hub.clone())) as Arc<dyn EventSink>,
            )
            .with_worktree(worktree)
            .with_skills(skills)
            .with_tools(tools.clone())
            .with_hooks(hooks);
            // A child gets the same workspace capability boundary and durable prompt context as
            // its parent. `tools: None` here used to mean "all registered tools", which let a
            // custom workspace grant only `create_agent` and accidentally give the child shell /
            // write access as well.
            let agent_tools = plan.tools_allow.clone();
            let agent_system = plan.system.clone();
            let profiles = agent_names
                .iter()
                .filter_map(|name| {
                    match router.resolve(&Purpose::Agent(name.clone()), session_model.as_deref()) {
                        Ok(routed) => {
                            let mut system = agent_system.clone();
                            system.push(background_agent_system(name));
                            Some(zlogic_core::AgentProfile {
                                name: name.clone(),
                                system,
                                tools: agent_tools.clone(),
                                model: routed.model,
                                client: routed.client,
                                thinking: routed.thinking,
                            })
                        }
                        Err(error) => {
                            tracing::warn!(
                                target: "zlogic::engine",
                                agent = %name,
                                "sub-agent profile could not resolve a model: {error}"
                            );
                            plan.notices.push(zlogic_core::PlanNotice {
                                level: NoticeLevel::Warn,
                                code: "agent_model_unavailable".into(),
                                message: format!(
                                    "sub-agent `{name}` could not resolve a model ({error}); it will not run in this turn"
                                ),
                                args: std::collections::BTreeMap::from([
                                    ("name".into(), serde_json::json!(name)),
                                    ("error".into(), serde_json::json!(error.to_string())),
                                ]),
                            });
                            None
                        }
                    }
                })
                .collect::<Vec<_>>();
            let core = if profiles.is_empty() {
                core
            } else {
                let skill_factory: Option<zlogic_core::AgentSkillFactory> =
                    skill_library.map(|library| {
                        let skill_root = root.clone();
                        Arc::new(move |_session_id: SessionId| {
                            library.host(workspace_id, skill_root.clone())
                                as Arc<dyn zlogic_tools::SkillHost>
                        }) as zlogic_core::AgentSkillFactory
                    });
                let spawner = CoreSpawner::with_base(
                    services.clone(),
                    profiles,
                    agent_names
                        .iter()
                        .find(|name| name.as_str() == "general")
                        .and_then(|name| {
                            router
                                .resolve(&Purpose::Agent(name.clone()), session_model.as_deref())
                                .ok()
                                .map(|routed| {
                                    let mut system = agent_system.clone();
                                    system.push(background_agent_system(name));
                                    zlogic_core::AgentProfile {
                                        name: name.clone(),
                                        system,
                                        tools: agent_tools.clone(),
                                        model: routed.model,
                                        client: routed.client,
                                        thinking: routed.thinking,
                                    }
                                })
                        }),
                    Arc::new(HubSink(hub.clone())) as Arc<dyn EventSink>,
                    root,
                    0,
                    tools,
                    skill_factory,
                );
                spawner.set_budget(Some(budget.clone()));
                core.with_spawner(Some(spawner))
            };

            let outcome = std::panic::AssertUnwindSafe(core.run_with_input(
                turn_id,
                plan,
                input,
                cancel.clone(),
            ))
            .catch_unwind()
            .await;
            drop(guard);

            match outcome {
                Ok(Ok(out)) => {
                    tracing::info!(
                        target: "zlogic::engine",
                        %session_ref,
                        %turn_id,
                        status = ?out.status,
                        rounds = out.stats.rounds,
                        duration_ms = out.stats.duration_ms,
                        interaction_wait_ms = out.stats.interaction_wait_ms,
                        "turn task finished"
                    );
                    registry.finish(turn_id, out.status, out.stats);
                }
                Ok(Err(e)) => {
                    tracing::error!(target: "zlogic::engine", %session_id, %turn_id, "turn failed: {e}");
                    registry.finish(turn_id, TurnStatus::Failed, TurnStats::default());
                    tracing::info!(
                        target: "zlogic::engine",
                        %session_ref,
                        %turn_id,
                        status = ?TurnStatus::Failed,
                        "turn task finished"
                    );
                }
                Err(panic) => {
                    cancel.cancel();
                    tracing::error!(
                        target: "zlogic::engine",
                        %session_id,
                        %turn_id,
                        end = "turn panicked; ending the conversation's turn as failed",
                        panic = %panic_message(&panic),
                    );
                    registry.finish(turn_id, TurnStatus::Failed, TurnStats::default());
                    tracing::info!(
                        target: "zlogic::engine",
                        %session_ref,
                        %turn_id,
                        status = ?TurnStatus::Failed,
                        "turn task finished"
                    );
                }
            }
            hub.notify(StateNotice {
                session_id: session_ref.to_string(),
                turn_id: Some(turn_id.to_string()),
                change: StateChange::TurnStateChanged,
            });
            // Exactly the same entry point as submit/task completion. This closes the race where a
            // mailbox row arrives after the last core checkpoint but before the lock is released.
            if let Err(error) = next_dispatch.start_if_idle(session_ref) {
                tracing::warn!(
                    target: "zlogic::engine",
                    %session_ref,
                    "turn ended with pending mailbox input, but the next turn could not start: {error}"
                );
                write_session_notice(
                    &next_dispatch.services,
                    session_ref,
                    None,
                    "turn_start_failed",
                    &zlogic_protocol::LocalizedMessage::new(
                        "notice.turn_start_failed",
                        format!(
                            "A queued message could not start a reply: {error}. Configure a model and an API key, then try again."
                        ),
                    )
                    .arg("error", error.to_string()),
                );
            }
        });

        Ok(Some(turn_id))
    }

    fn turn_budget(
        &self,
        root: &std::path::Path,
        session_id: SessionId,
        plan: &mut TurnPlan,
    ) -> zlogic_core::budget::TurnBudget {
        use zlogic_protocol::usage::CostTally;

        let cost = self.router.config().cost.clone();
        let (config, warnings) = match &self.dirs {
            Some(dirs) => crate::budget::load(&[
                &dirs.config.join("policy.yaml"),
                &root.join(".zlogic/policy.yaml"),
            ]),
            None => Default::default(),
        };
        for warning in warnings {
            plan.notices
                .push(zlogic_core::PlanNotice::warn("budget.config", warning));
        }

        let mut budget = zlogic_core::budget::TurnBudget::new(config.clone(), cost);
        if !config.is_set() {
            return budget;
        }

        if config.per_session.is_some() {
            let spent = self.store.with(|db| {
                let root_session = db.sessions().get(session_id)?.root_session_id;
                db.usage().cost_by_currency_for_tree(root_session)
            });
            match spent {
                Ok(rows) => {
                    let mut tally = CostTally::default();
                    for (currency, amount) in rows {
                        tally.add(amount, &currency, None);
                    }
                    budget = budget.with_session_spent(tally);
                }
                Err(e) => plan.notices.push(zlogic_core::PlanNotice::warn(
                    "budget.baseline",
                    format!("could not look up how much this session has spent ({e}) — the per_session budget will only apply to this turn for now"),
                )),
            }
        }
        if let Some(window) = &config.window {
            let since = Utc::now() - chrono::Duration::hours(i64::from(window.hours));
            match self
                .store
                .with(|db| db.usage().cost_by_currency_since(since))
            {
                Ok(rows) => {
                    let mut tally = CostTally::default();
                    for (currency, amount) in rows {
                        tally.add(amount, &currency, None);
                    }
                    budget = budget.with_window_spent(tally);
                }
                Err(e) => plan.notices.push(zlogic_core::PlanNotice::warn(
                    "budget.baseline",
                    format!("could not look up how much was spent in the time window ({e}) — the window budget will only apply to this turn for now"),
                )),
            }
        }
        budget
    }

    fn turn_state(&self, turn_id: TurnId) -> Result<TurnState> {
        turn_state_of(&self.registry, &self.store, turn_id)
            .ok_or_else(|| EngineError::NotFound(format!("turn {turn_id}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zlogic_core::CoreServices;
    use zlogic_objects::MemoryObjectStore;
    use zlogic_store::{Db, EntryKind, NewSession};

    fn services(db: Db) -> (Arc<CoreServices>, SharedStore, Arc<MemoryObjectStore>) {
        let store = SharedStore::new(db);
        let objects = Arc::new(MemoryObjectStore::new());
        let services = Arc::new(CoreServices {
            store: store.clone(),
            objects: objects.clone(),
            attachment_dir: std::env::temp_dir(),
            tools: zlogic_tools::ToolRegistry::with_builtins(),
            policy: Arc::new(zlogic_core::policy::AllowAll),
            interaction: None,
            tasks: None,
            runtime_paths: None,
            model_resolver: None,
            limits: zlogic_core::Limits::default(),
            context: zlogic_core::ContextPolicy::default(),
        });
        (services, store, objects)
    }

    fn session_with_db() -> (SessionId, Db) {
        let db = Db::open_in_memory().unwrap();
        let session_id = db
            .sessions()
            .create(NewSession::root(WorkspaceId::new()))
            .unwrap()
            .session_id;
        (session_id, db)
    }

    #[test]
    fn write_session_notice_persists_a_warn_notice() {
        let (session_id, db) = session_with_db();
        let (services, store, _objects) = services(db);

        write_session_notice(
            &services,
            session_id,
            None,
            "turn_start_failed",
            &zlogic_protocol::LocalizedMessage::new(
                "notice.turn_start_failed",
                "A queued message could not start a reply: could not get credential env:K for p:m",
            )
            .arg("error", "could not get credential env:K for p:m"),
        );

        let rows = store.with(|db| db.entries().list(session_id)).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, EntryKind::Event);
        let data = &rows[0].data;
        assert_eq!(data["level"], "warn");
        assert_eq!(data["code"], "turn_start_failed");
        assert_eq!(
            data["message"]["key"], "notice.turn_start_failed",
            "the key follows the notice.<code> convention so the host can localize it"
        );
        assert_eq!(
            data["message"]["args"]["error"].as_str().unwrap(),
            "could not get credential env:K for p:m",
            "the error needed for bundle interpolation must travel upstream with the message"
        );
        assert!(
            data["message"]["fallback"]
                .as_str()
                .unwrap()
                .contains("could not get credential"),
            "the fallback carries the full error text: {}",
            data["message"]["fallback"]
        );
    }

    #[test]
    fn write_session_notice_attaches_to_the_given_turn() {
        let (session_id, db) = session_with_db();
        let (services, store, _objects) = services(db);
        let turn_id = TurnId::new();

        write_session_notice(
            &services,
            session_id,
            Some((turn_id, 7)),
            "session_title_failed",
            &zlogic_protocol::LocalizedMessage::new(
                "notice.session_title_failed",
                "the session title could not be refined: no credential",
            )
            .arg("error", "no credential"),
        );

        let rows = store.with(|db| db.entries().list(session_id)).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].turn_id, turn_id);
        assert_eq!(rows[0].turn_seq, 7);
        assert_eq!(rows[0].data["code"], "session_title_failed");
    }
}

fn builtin_agent_system(name: &str) -> String {
    crate::agent_profile::builtin_system_prompt(name)
}

pub fn background_agent_system(name: &str) -> String {
    builtin_agent_system(name)
}

fn write_session_notice(
    services: &CoreServices,
    session_id: SessionId,
    turn: Option<(TurnId, i64)>,
    code: &str,
    message: &zlogic_protocol::LocalizedMessage,
) {
    let (turn_id, turn_seq) = match turn {
        Some((turn_id, turn_seq)) => (turn_id, turn_seq),
        None => {
            let Ok(turn_seq) = services
                .store
                .with(|db| db.entries().next_turn_seq(session_id))
            else {
                return;
            };
            (TurnId::new(), turn_seq)
        }
    };
    let Ok(entry) = zlogic_core::entry_data::notice_entry(
        session_id,
        turn_id,
        turn_seq,
        NoticeLevel::Warn,
        code,
        message,
    ) else {
        return;
    };
    if let Err(e) = services.store.with(|db| {
        db.entries()
            .append_with_offload(entry, services.objects.as_ref())
    }) {
        tracing::warn!(
            target: "zlogic::engine",
            %session_id,
            code,
            "could not persist the failure notice: {e}"
        );
    }
}

impl crate::task::TaskWake for Dispatcher {
    fn mailbox_ready(&self, session_id: SessionId) -> std::result::Result<(), String> {
        self.hub.notify(StateNotice {
            session_id: session_id.to_string(),
            turn_id: None,
            change: StateChange::MailboxChanged,
        });
        match self.start_if_idle(session_id) {
            Ok(_) => Ok(()),
            Err(error) => {
                write_session_notice(
                    &self.services,
                    session_id,
                    None,
                    "turn_start_failed",
                    &zlogic_protocol::LocalizedMessage::new(
                        "notice.turn_start_failed",
                        format!(
                            "A queued message could not start a reply: {error}. Configure a model and an API key, then try again."
                        ),
                    )
                    .arg("error", error.to_string()),
                );
                Err(error.to_string())
            }
        }
    }
}

#[async_trait]
impl crate::task::ScheduledAgentFactory for Dispatcher {
    async fn spawner_for(
        &self,
        workspace_id: WorkspaceId,
        parent_session_id: SessionId,
        profile: &str,
        model_ref: Option<&str>,
        cwd: Option<&str>,
    ) -> std::result::Result<Arc<dyn zlogic_tools::AgentSpawner>, String> {
        let custom = self
            .store
            .with(|db| db.agent_profiles().get(profile))
            .map_err(|error| error.to_string())?;
        let known = self.agents.iter().any(|name| name == profile) || custom.is_some();
        if !known {
            return Err(format!(
                "unknown agent profile `{profile}`; available: {}, {}",
                self.agents.join(", "),
                "(or create a custom agent profile)"
            ));
        }
        let session = self
            .store
            .with(|db| db.sessions().get(parent_session_id))
            .map_err(|error| error.to_string())?;
        if session.workspace_id != workspace_id {
            return Err("agent job session does not belong to its workspace".into());
        }
        let root = self
            .roots
            .root_of(workspace_id)
            .ok_or_else(|| format!("workspace {workspace_id} has no registered root"))?;
        let workspace_tools = self.roots.tools_of(workspace_id);
        let mut resolved_profiles = Vec::new();
        let mut context_window = None;
        let mut agent_names = self.agents.to_vec();
        if custom.is_some() && !agent_names.iter().any(|name| name == profile) {
            agent_names.push(profile.to_owned());
        }
        for name in agent_names.iter() {
            let is_selected = name == profile;
            let custom_this = if is_selected { custom.clone() } else { None };
            let routed = self
                .router
                .resolve(
                    &Purpose::Agent(name.clone()),
                    custom_this
                        .as_ref()
                        .and_then(|c| c.model_ref.as_deref())
                        .or(model_ref),
                )
                .map_err(|error| error.to_string())?;
            if is_selected {
                context_window = Some(routed.model.context_window);
            }
            resolved_profiles.push((name.clone(), routed, custom_this));
        }
        let (tools, unavailable_mcp) = match &self.extensions {
            Some(extensions) => {
                let assembled = extensions
                    .tools_for(workspace_id, &root, &self.services.tools, context_window)
                    .await;
                (Some(assembled.tools), assembled.unavailable)
            }
            None => (None, Vec::new()),
        };
        let mut system = Vec::new();
        if let Some(prompts) = &self.prompts {
            let effective_registry = tools.as_ref().unwrap_or(&self.services.tools);
            let mut names = effective_registry.available_names();
            if let Some(allow) = &workspace_tools {
                names.retain(|name| allow.contains(name));
            }
            let allows_mcp = workspace_tools
                .as_ref()
                .is_none_or(|allow| allow.iter().any(|name| name.starts_with("mcp__")));
            let global_memory = self
                .store
                .with(|db| {
                    db.memories().list(&zlogic_protocol::MemoryListReq {
                        scope: zlogic_protocol::MemoryScope::Global,
                        workspace_id: None,
                        include_removed: false,
                    })
                })
                .map_err(|error| error.to_string())?;
            let workspace_memory = self
                .store
                .with(|db| {
                    db.memories().list(&zlogic_protocol::MemoryListReq {
                        scope: zlogic_protocol::MemoryScope::Workspace,
                        workspace_id: Some(workspace_id),
                        include_removed: false,
                    })
                })
                .map_err(|error| error.to_string())?;
            let tool_guidance = effective_registry.prompt_guidance(&names);
            let effectful = effective_registry.any_effectful(&names);
            system = prompts.build(crate::PromptRequest {
                workspace_id,
                root: &root,
                exec_cwd: cwd.map_or(root.as_path(), std::path::Path::new),
                tools: &names,
                tool_guidance: &tool_guidance,
                unavailable_mcp: &unavailable_mcp,
                allows_mcp,
                effectful,
                global_memory: &global_memory,
                workspace_memory: &workspace_memory,
            });
        }
        let profiles = resolved_profiles
            .into_iter()
            .map(|(name, routed, custom_this)| {
                let mut profile_system = system.clone();
                if let Some(record) = &custom_this {
                    if !record.system_prompt.trim().is_empty() {
                        profile_system.push(record.system_prompt.clone());
                    }
                } else {
                    profile_system.push(background_agent_system(&name));
                }
                let profile_tools = match &custom_this {
                    Some(record) if !record.tools.is_empty() => {
                        let mut tools: Vec<String> = workspace_tools
                            .clone()
                            .unwrap_or_default()
                            .into_iter()
                            .filter(|t| record.tools.contains(t))
                            .collect();
                        if workspace_tools.is_none() {
                            tools = record.tools.clone();
                        }
                        Some(tools)
                    }
                    _ => workspace_tools.clone(),
                };
                zlogic_core::AgentProfile {
                    name,
                    system: profile_system,
                    tools: profile_tools,
                    model: routed.model,
                    client: routed.client,
                    thinking: routed.thinking,
                }
            })
            .collect();
        let skill_factory: Option<zlogic_core::AgentSkillFactory> =
            self.skills.clone().map(|library| {
                let skill_root = root.clone();
                Arc::new(move |_session_id: SessionId| {
                    library.host(workspace_id, skill_root.clone())
                        as Arc<dyn zlogic_tools::SkillHost>
                }) as zlogic_core::AgentSkillFactory
            });
        let base = self
            .agents
            .iter()
            .find(|name| name.as_str() == "general")
            .and_then(|name| {
                self.router
                    .resolve(&Purpose::Agent(name.clone()), model_ref)
                    .ok()
                    .map(|routed| {
                        let mut profile_system = system.clone();
                        profile_system.push(background_agent_system(name));
                        zlogic_core::AgentProfile {
                            name: name.clone(),
                            system: profile_system,
                            tools: workspace_tools.clone(),
                            model: routed.model,
                            client: routed.client,
                            thinking: routed.thinking,
                        }
                    })
            });
        Ok(CoreSpawner::with_base(
            self.services.clone(),
            profiles,
            base,
            Arc::new(HubSink(self.hub.clone())) as Arc<dyn EventSink>,
            root,
            0,
            tools,
            skill_factory,
        ))
    }
}

pub fn turn_state_of(
    registry: &TurnRegistry,
    store: &SharedStore,
    turn_id: TurnId,
) -> Option<TurnState> {
    store.with(|db| turn_state_of_db(registry, db, turn_id))
}

pub fn turn_state_of_db(
    registry: &TurnRegistry,
    db: &zlogic_store::Db,
    turn_id: TurnId,
) -> Option<TurnState> {
    let live = registry.get(turn_id)?;

    let pending = db
        .entries()
        .pending_interactions(live.session_id, turn_id)
        .ok()?
        .into_iter()
        .next()
        .and_then(|p| {
            Some(PendingInteraction {
                interaction_id: p.interaction_id,
                body: serde_json::from_value(p.request.data.get("body")?.clone()).ok()?,
                asked_at: p.request.created_at,
            })
        });

    Some(TurnState {
        turn_id,
        session_id: live.session_id,
        phase: live.phase(pending.is_some()),
        started_at: live.started_at,
        ended_at: live.ended_at,
        status: live.status.clone(),
        stats: live.stats.clone(),
        pending_interaction: pending,
    })
}

#[async_trait]
impl TurnService for Dispatcher {
    async fn submit(&self, submission: Submission) -> ApiResult<SubmitAck> {
        let session_id: SessionId = submission.session_id.parse().map_err(|_| {
            EngineError::Invalid(format!("invalid session id: {}", submission.session_id))
        })?;

        if submission.parts.iter().any(|part| {
            matches!(
                part,
                MessagePart::TaskUpdate { .. }
                    | MessagePart::SkillLoad { .. }
                    | MessagePart::SkillInvocation { .. }
            )
        }) {
            return Ok(SubmitAck::Rejected {
                reason: zlogic_protocol::LocalizedMessage::new(
                    "error.submit_internal_part",
                    "task_update, skill_load and skill_invocation can only be produced by the runtime",
                ),
            });
        }

        let parts: Vec<MessagePart> = submission
            .parts
            .into_iter()
            .filter(|p| !is_blank(p))
            .collect();
        if parts.is_empty() {
            return Ok(SubmitAck::Rejected {
                reason: zlogic_protocol::LocalizedMessage::new(
                    "error.submit_empty",
                    "the submission is empty",
                ),
            });
        }
        let mut parts = parts;

        let model_ref = submission
            .model_ref
            .as_deref()
            .map(str::trim)
            .filter(|model_ref| !model_ref.is_empty())
            .map(str::to_owned);
        if let Some(model_ref) = &model_ref
            && self.router.config().resolve(model_ref).is_err()
        {
            return Ok(SubmitAck::Rejected {
                reason: zlogic_protocol::LocalizedMessage::new(
                    "error.submit_unknown_model",
                    format!("unknown model: {model_ref}"),
                )
                .arg("model", model_ref.clone()),
            });
        }

        let delivery = match submission.delivery {
            InputDelivery::Steer => Delivery::Steer,
            InputDelivery::Queue => Delivery::Queue,
        };
        if let Some(submission_id) = self
            .delivered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(session_id, &submission.client_request_id)
        {
            return Ok(SubmitAck::Duplicate { submission_id });
        }
        let existing = self
            .store
            .with(|db| db.mailbox().pending(session_id))
            .map_err(EngineError::from)?;
        if let Some(known) = existing
            .iter()
            .find(|m| m.client_request_id == submission.client_request_id)
        {
            return Ok(SubmitAck::Duplicate {
                submission_id: known.submission_id.to_string(),
            });
        }

        if let Some(dirs) = &self.dirs {
            let session = self
                .store
                .with(|db| db.sessions().get(session_id))
                .map_err(EngineError::from)?;
            let root = self.roots.root_of(session.workspace_id).ok_or_else(|| {
                EngineError::Invalid(format!(
                    "workspace {} has no registered root",
                    session.workspace_id
                ))
            })?;
            match zlogic_hooks::CommandHookRunner::load(&dirs.config.join("hooks.yaml"), &root) {
                Ok(runner) if !runner.is_empty() => {
                    let mut matchers = std::collections::BTreeMap::new();
                    matchers.insert(
                        "delivery".into(),
                        match delivery {
                            Delivery::Steer => "steer",
                            Delivery::Queue => "queue",
                        }
                        .into(),
                    );
                    let outcome = runner
                        .run(
                            &zlogic_hooks::HookRequest {
                                event: zlogic_hooks::HookEvent::InputSubmitBefore,
                                session_id: session_id.to_string(),
                                cwd: root,
                                payload: serde_json::to_value(&parts).map_err(|error| {
                                    EngineError::Invalid(format!(
                                        "failed to serialize submission: {error}"
                                    ))
                                })?,
                                matchers,
                            },
                            &CancellationToken::new(),
                        )
                        .await;
                    if let Some(reason) = outcome.blocked {
                        let reason = reason.to_string();
                        return Ok(SubmitAck::Rejected {
                            reason: zlogic_protocol::LocalizedMessage::new(
                                "error.submit_rejected",
                                reason.clone(),
                            )
                            .arg("reason", reason),
                        });
                    }
                    if let Some(input) = outcome.input {
                        parts = serde_json::from_value(input).map_err(|error| {
                            EngineError::Invalid(format!(
                                "input.submit.before returned an invalid input: {error}"
                            ))
                        })?;
                    }
                    parts.extend(
                        outcome
                            .contexts
                            .into_iter()
                            .map(|text| MessagePart::Text { text }),
                    );
                    for warning in outcome.warnings {
                        tracing::warn!(target: "zlogic::hooks", "{warning}");
                    }
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(target: "zlogic::hooks", "{error}"),
            }
        }
        parts = match self.normalize_attachments(session_id, parts).await {
            Ok(parts) => parts,
            Err(reason) => {
                let reason = reason.to_string();
                return Ok(SubmitAck::Rejected {
                    reason: zlogic_protocol::LocalizedMessage::new(
                        "error.submit_rejected",
                        reason.clone(),
                    )
                    .arg("reason", reason),
                });
            }
        };

        if let Ok(session) = self.store.with(|db| db.sessions().get(session_id))
            && let Err(error) = self.router.resolve(
                &Purpose::Main,
                existing
                    .iter()
                    .find_map(|record| record.model_ref.as_deref())
                    .or(model_ref.as_deref())
                    .or(session.model_ref.as_deref()),
            )
        {
            tracing::warn!(
                target: "zlogic::engine",
                %session_id,
                "submission rejected before reaching the mailbox: no usable model ({error})"
            );
            return Ok(SubmitAck::Rejected {
                reason: zlogic_protocol::LocalizedMessage::new(
                    "error.submit_no_usable_model",
                    "This message was not sent: no usable model ({error}). Configure a model and an API key, then try again.",
                )
                .arg("error", error.to_string()),
            });
        }

        let payload = serde_json::to_value(&parts)
            .map_err(|e| EngineError::Invalid(format!("failed to serialize submission: {e}")))?;
        let thinking = submission
            .thinking
            .map(|intent| {
                serde_json::to_value(intent).map_err(|e| {
                    EngineError::Invalid(format!("failed to serialize submission thinking: {e}"))
                })
            })
            .transpose()?;
        let record = self
            .store
            .with(|db| {
                db.mailbox().submit_with_model(
                    session_id,
                    &submission.client_request_id,
                    &payload,
                    model_ref.as_deref(),
                    thinking.as_ref(),
                    delivery,
                )
            })
            .map_err(EngineError::from)?;

        self.hub.notify(StateNotice {
            session_id: session_id.to_string(),
            turn_id: None,
            change: StateChange::MailboxChanged,
        });

        let (started_turn_id, notice) = match self.start_if_idle(session_id) {
            Ok(turn_id) => (turn_id.map(|turn_id| turn_id.to_string()), None),
            Err(e) => {
                tracing::warn!(target: "zlogic::engine", "submission accepted, but could not open a turn: {e}");
                let notice = match &e {
                    EngineError::NoModel { .. } => zlogic_protocol::LocalizedMessage::new(
                        "error.submit_queued_no_model",
                        "No model is currently available, so this message is queued. Configure a model and an API key in settings ({error}).",
                    )
                    .arg("error", e.to_string()),
                    _ => zlogic_protocol::LocalizedMessage::new(
                        "error.submit_queued_turn_failed",
                        "This message was queued, but a reply could not be started yet: {error}",
                    )
                    .arg("error", e.to_string()),
                };
                (None, Some(notice))
            }
        };

        if let Some(auxiliary) = &self.auxiliary
            && let Some(text) = parts.iter().find_map(|part| match part {
                MessagePart::Text { text } => Some(text.as_str()),
                _ => None,
            })
        {
            let text = text.to_string();
            let turn_ctx = started_turn_id
                .as_deref()
                .and_then(|id| id.parse::<TurnId>().ok())
                .map(|turn_id| {
                    let turn_seq = self
                        .store
                        .with(|db| db.entries().next_turn_seq(session_id))
                        .unwrap_or(0);
                    (turn_id, turn_seq)
                });
            match auxiliary.draft_session_title(session_id, &text) {
                Ok(()) => {
                    let auxiliary = auxiliary.clone();
                    let services = self.services.clone();
                    tokio::spawn(async move {
                        if let Err(error) = auxiliary.generate_session_title(session_id, text).await
                        {
                            tracing::warn!(
                                target: "zlogic::engine",
                                %session_id,
                                "failed to auto-generate the session title: {error}"
                            );
                            write_session_notice(
                                &services,
                                session_id,
                                turn_ctx,
                                "session_title_failed",
                                &zlogic_protocol::LocalizedMessage::new(
                                    "notice.session_title_failed",
                                    format!("the session title could not be refined: {error}"),
                                )
                                .arg("error", error.to_string()),
                            );
                        }
                    });
                }
                Err(error) => tracing::warn!(
                    target: "zlogic::engine",
                    %session_id,
                    "failed to generate the session title draft: {error}"
                ),
            }
        }

        Ok(SubmitAck::Accepted {
            submission_id: record.submission_id.to_string(),
            started_turn_id,
            notice,
        })
    }

    async fn control(&self, command: Command) -> ApiResult<()> {
        match command {
            Command::CancelTurn { turn_id } => {
                let turn_id: TurnId = turn_id
                    .parse()
                    .map_err(|_| EngineError::Invalid(format!("invalid turn id: {turn_id}")))?;
                match self.registry.get(turn_id) {
                    Some(t) => {
                        tracing::info!(
                            target: "zlogic::engine",
                            %turn_id,
                            session = %t.session_id,
                            "cancel_turn: firing the cancellation token"
                        );
                        t.cancel.cancel();
                    }
                    None => tracing::debug!(
                        target: "zlogic::engine",
                        %turn_id, "cancelling a turn that no longer exists, ignoring"
                    ),
                }
                Ok(())
            }

            Command::CompactContext { session_id } => {
                let session_id: SessionId = session_id.parse().map_err(|_| {
                    EngineError::Invalid(format!("invalid session id: {session_id}"))
                })?;
                self.start_manual_compaction(session_id)?;
                Ok(())
            }

            Command::AnswerInteraction {
                interaction_id,
                decision,
            } => {
                self.interactions.answer(&interaction_id, decision)?;
                Ok(())
            }

            Command::RetargetSubmission {
                submission_id,
                delivery,
            } => {
                let id = submission_id.parse().map_err(|_| {
                    EngineError::Invalid(format!("invalid submission id: {submission_id}"))
                })?;
                let delivery = match delivery {
                    InputDelivery::Steer => Delivery::Steer,
                    InputDelivery::Queue => Delivery::Queue,
                };
                let session_id = self
                    .store
                    .with(|db| db.mailbox().get(id))
                    .map_err(EngineError::from)?
                    .session_id;
                let changed = self
                    .store
                    .with(|db| db.mailbox().retarget(id, delivery))
                    .map_err(EngineError::from)?;
                if !changed {
                    return Err(EngineError::NotFound(format!("submission {submission_id}")).into());
                }
                self.hub.notify(StateNotice {
                    session_id: session_id.to_string(),
                    turn_id: None,
                    change: StateChange::MailboxChanged,
                });
                Ok(())
            }

            Command::CancelSubmission { submission_id } => {
                let id = submission_id.parse().map_err(|_| {
                    EngineError::Invalid(format!("invalid submission id: {submission_id}"))
                })?;
                let session_id = self
                    .store
                    .with(|db| db.mailbox().get(id))
                    .map_err(EngineError::from)?
                    .session_id;
                let removed = self
                    .store
                    .with(|db| db.mailbox().cancel(id))
                    .map_err(EngineError::from)?;
                if !removed {
                    return Err(EngineError::NotFound(format!("submission {submission_id}")).into());
                }
                self.hub.notify(StateNotice {
                    session_id: session_id.to_string(),
                    turn_id: None,
                    change: StateChange::MailboxChanged,
                });
                Ok(())
            }

            Command::Rewind {
                session_id,
                keep_through_turn,
            } => {
                let session: SessionId = session_id.parse().map_err(|_| {
                    EngineError::Invalid(format!("invalid session id: {session_id}"))
                })?;
                let live = self
                    .store
                    .with(|db| db.locks().live_turn(session))
                    .map_err(EngineError::from)?;
                if live.is_some() {
                    return Err(EngineError::Busy {
                        session_id: session.to_string(),
                    }
                    .into());
                }
                self.store
                    .with(|db| {
                        for record in db.mailbox().pending(session)? {
                            db.mailbox().cancel(record.submission_id)?;
                        }
                        db.entries().rewind(session, keep_through_turn as i64)
                    })
                    .map_err(EngineError::from)?;
                self.hub.notify(StateNotice {
                    session_id: session.to_string(),
                    turn_id: None,
                    change: StateChange::TurnStateChanged,
                });
                Ok(())
            }

            Command::Fork {
                session_id,
                keep_through_turn,
            } => {
                let session: SessionId = session_id.parse().map_err(|_| {
                    EngineError::Invalid(format!("invalid session id: {session_id}"))
                })?;
                let forked = self
                    .store
                    .with(|db| {
                        let src = db.sessions().get(session)?;
                        let mut new = zlogic_store::NewSession::root(src.workspace_id);
                        new.exec_cwd = src.exec_cwd.clone();
                        new.model_ref = src.model_ref.clone();
                        new.effort = src.effort.clone();
                        let created = db.sessions().create(new)?;
                        db.entries().copy_through(
                            session,
                            created.session_id,
                            keep_through_turn as i64,
                        )?;
                        if let Some(title) = &src.title {
                            db.sessions().set_title(
                                created.session_id,
                                &format!("{title} (fork)"),
                                zlogic_store::TitleSource::User,
                            )?;
                        }
                        Ok::<_, zlogic_store::StoreError>(created)
                    })
                    .map_err(EngineError::from)?;
                self.hub.notify(StateNotice {
                    session_id: forked.session_id.to_string(),
                    turn_id: None,
                    change: StateChange::TurnStateChanged,
                });
                Ok(())
            }
        }
    }

    async fn state(&self, turn_id: TurnId) -> ApiResult<TurnState> {
        Ok(self.turn_state(turn_id)?)
    }

    async fn cancel_all_turns(&self) -> ApiResult<usize> {
        let cancelled = self.registry.cancel_live();
        if cancelled > 0 {
            tracing::info!(
                target: "zlogic::engine",
                cancelled,
                "host is exiting; cancelling all live turns in this process"
            );
        }
        Ok(cancelled)
    }

    async fn live_turn_count(&self) -> ApiResult<usize> {
        Ok(self.registry.live_count())
    }
}

fn is_blank(part: &MessagePart) -> bool {
    match part {
        MessagePart::Text { text } => text.trim().is_empty(),
        MessagePart::Skill { name, .. } => name.trim().is_empty(),
        MessagePart::SkillLoad {
            name,
            revision,
            body_object,
            ..
        } => name.trim().is_empty() || revision.trim().is_empty() || body_object.trim().is_empty(),
        MessagePart::SkillInvocation { name, .. } => name.trim().is_empty(),
        MessagePart::SkillUnload { name } => name.trim().is_empty(),
        MessagePart::File { path } => path.trim().is_empty(),
        MessagePart::Attachment {
            object_id, name, ..
        } => object_id.trim().is_empty() || name.trim().is_empty(),
        MessagePart::TaskUpdate { .. } => false,
    }
}

fn normalize_mime(value: &str) -> String {
    zlogic_tools::normalize_mime(value)
}

fn attachment_name_with_extension(name: &str, mime_type: &str) -> String {
    if std::path::Path::new(name).extension().is_some() {
        return name.to_string();
    }
    let extension = match mime_type {
        "text/csv" => Some("csv"),
        "text/tab-separated-values" => Some("tsv"),
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => Some("xlsx"),
        "application/vnd.ms-excel" => Some("xls"),
        "application/vnd.oasis.opendocument.spreadsheet" => Some("ods"),
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        _ => None,
    };
    extension.map_or_else(
        || name.to_string(),
        |extension| format!("{name}.{extension}"),
    )
}

fn safe_attachment_component(value: &str) -> String {
    let cleaned = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    let cleaned = cleaned.trim_matches('.').trim_matches('_');
    if cleaned.is_empty() {
        "attachment".into()
    } else {
        cleaned.chars().take(180).collect()
    }
}

#[cfg(unix)]
fn restrict_attachment_permissions(
    path: &std::path::Path,
    directory: bool,
) -> std::result::Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if directory { 0o700 } else { 0o600 };
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|error| {
        format!(
            "failed to restrict attachment permissions on {}: {error}",
            path.display()
        )
    })
}

#[cfg(not(unix))]
fn restrict_attachment_permissions(
    _path: &std::path::Path,
    _directory: bool,
) -> std::result::Result<(), String> {
    Ok(())
}
