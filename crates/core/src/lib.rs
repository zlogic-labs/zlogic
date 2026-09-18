//! # zlogic-core
//! One agent run: build the context, drive rounds, execute tools, persist everything, stream
//! events. [`Core::run`] is the whole entry point.
//! ```text
//! run ─┬─ next turn_seq, persist the user's parts
//!      ├─ round loop ─┬─ build_context  (pure, over stored entries)
//!      │              ├─ client.stream  ──> PartEnd persists, deltas render
//!      │              ├─ usage: normalise, price, upsert
//!      │              └─ tool batch: policy gate ──> execute ──> persist results
//!      └─ TurnEnd with stats
//! ```
//! # What core does not know
//! - **The workspace.** It is handed a `root` and a session. Which workspace that root belongs to,
//!   and whether it moved, is the engine's business — see `zlogic_store::session`.
//! - **Model routing.** A turn arrives with a [`TurnPlan`] holding an already-resolved model and
//!   the client to reach it with. Core pins that for the whole turn; a user switching models takes
//!   effect on the next turn.
//! - **Where the UI is.** It emits [`zlogic_protocol::stream::StreamEvent`] into an [`EventSink`] and
//!   asks questions through an [`InteractionPort`]. Both are traits.
//! # Why the store is a connection pool
//! `rusqlite::Connection` is `Send` but **not `Sync`**, so a shared `Db` used to hide behind a
//! mutex that serialised every store access. Now the store is a **connection pool**: it is easy
//! to hold a connection across an `await` and deadlock the process, so the only way to reach the
//! database is [`SharedStore::with`], which takes a closure: a connection cannot outlive it, and
//! no `.await` can appear inside one.
//! # Cancellation
//! **One [`CancellationToken`] per conversation**, created outside (the CLI or the UI), passed in
//! through the engine and handed to [`Core::run`]. Core forwards the *same* token to every tool and
//! to every sub-agent — nothing derives a child from it, so there is no layer that can forget to
//! propagate and leave work running after the user pressed Esc.
//! What cancelling does, and does not do:
//! - The round loop stops at its next checkpoint: **during the request handshake** (the
//!   `client.stream()` future is dropped — connect, TLS and any retry backoff all go with it),
//!   mid-stream (it stops polling and drops the stream), between tool calls, and while waiting
//!   on the user.
//! - **A running tool is asked, not killed.** Core never races a tool's future against the token —
//!   see [`zlogic_tools::ToolCtx::cancel`].
//! - Tool calls that never ran still get a **result**, marked cancelled. A `tool_calls` group
//!   missing any of its results is a replay error on every provider, so the group is completed
//!   rather than left dangling for `build_context` to drop.
//! - Everything already persisted stays. A cancelled turn is a real part of the history, and the
//!   next turn continues from it.
//! # Scope of this layer
//! Deliberately not here yet, and each needs its own LLM call path or a design of its own:
//! title refinement, attachment resolution (`file://` → base64), rewind and fork, MCP and skill
//! tool sources.

pub mod budget;
pub mod compact;
pub mod context;
pub mod cost;
pub mod entry_data;
pub mod policy;
pub mod round;
pub mod sink;
pub mod spawn;
pub mod steer;

use std::path::PathBuf;
use std::sync::Arc;

use zlogic_llm::LlmClient;
use zlogic_objects::ObjectStore;
use zlogic_protocol::config::ResolvedModel;
use zlogic_protocol::interaction::{
    Choice, Form, InteractionBody, InteractionDecision, InteractionPort, InteractionRequest,
};
use zlogic_protocol::llm::{FinishReason, ThinkingIntent};
use zlogic_protocol::message::{ContentPart, TextPart};
use zlogic_protocol::stream::{
    AgentRef, CompactionReason, IncompleteReason, ModelRef, NoticeLevel, RoundOutcome,
    StreamPayload, TurnStats, TurnStatus,
};
use zlogic_protocol::usage::Purpose;
use zlogic_protocol::{MessagePart, SessionId, TurnId};
pub use zlogic_store::SharedStore;
use zlogic_store::{Delivery, NewEntry};
use zlogic_task::{ExecutorSpec, TaskRun, TaskTrigger};
pub use zlogic_tools::CancellationToken;
use zlogic_tools::{
    AgentMailboxGate, AgentSpawner, RuntimePathProvider, SkillHost, TaskHost, ToolRegistry,
    WorktreeHost,
};

pub use compact::{ContextPolicy, Summary};
pub use policy::{GrantSet, PolicyDecision, PolicyGate, PolicyRequest};
pub use sink::{EventSink, RecordingSink, TurnEmitter};
pub use spawn::{AgentProfile, AgentSkillFactory, CoreSpawner};
pub use steer::Drained;

/// Resolves a model reference (as typed into `create_agent`'s `model` argument) into a runnable
/// model and client.
/// The same dependency boundary as [`AgentSpawner`]: the trait lives in core (which owns the
/// resolved shape) and the implementation lives in the engine, which owns routing, credentials and
/// the LLM factory. It is injected through [`CoreServices`], so a host without a resolver (tests,
/// headless runs) fails a custom `model` closed with a clear message rather than pretending.
#[async_trait::async_trait]
pub trait ModelResolver: Send + Sync {
    /// Resolves a `provider:model` (or tier / `session` sentinel) reference to a concrete model,
    /// client and thinking intent. The error must say what was tried, because "no such model" is
    /// unanswerable without it — same contract as the engine's `NoModel`.
    async fn resolve(
        &self,
        model_ref: &str,
    ) -> std::result::Result<(ResolvedModel, Arc<dyn LlmClient>, ThinkingIntent), String>;
}

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error(transparent)]
    Store(#[from] zlogic_store::StoreError),
    #[error(transparent)]
    Object(#[from] zlogic_objects::ObjectError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Llm(#[from] zlogic_protocol::llm::LlmError),
    #[error(transparent)]
    Tool(#[from] zlogic_tools::ToolError),
    /// Stored data that cannot be interpreted — a missing source stamp, an unreadable payload.
    /// Its own variant because the response is different: not a retry and not a user error, but a
    /// history that has to be repaired or skipped.
    #[error("corrupt history: {0}")]
    Corrupt(String),
    /// Compaction ran but could not shrink the request below the model window; retrying would
    /// resend the same oversized tail.
    #[error("{0}")]
    ContextUncompressible(String),
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, CoreError>;

/// Everything a turn needs that outlives it.
pub struct CoreServices {
    pub store: SharedStore,
    pub objects: Arc<dyn ObjectStore>,
    /// Stable, application-managed files materialized from remote attachment objects.
    /// Models and file tools speak paths. Keeping this directory at the service boundary lets the
    /// engine normalize an uploaded object into that one path-based contract before it reaches a
    /// turn, without exposing the object store's private shard layout.
    pub attachment_dir: PathBuf,
    pub tools: ToolRegistry,
    pub policy: Arc<dyn PolicyGate>,
    /// Absent in a headless run. Anything needing a decision then fails closed.
    pub interaction: Option<Arc<dyn InteractionPort>>,
    /// Process-wide durable task runtime. Core only forwards it to tools.
    pub tasks: Option<Arc<dyn TaskHost>>,
    pub runtime_paths: Option<Arc<dyn RuntimePathProvider>>,
    /// Resolves a model reference (`create_agent`'s `model` argument) into a runnable model and
    /// client. `None` = a custom `model` argument fails closed with "no model resolver", exactly
    /// like `create_agent` without a spawner.
    pub model_resolver: Option<Arc<dyn ModelResolver>>,
    pub limits: Limits,
    pub context: ContextPolicy,
}

#[derive(Debug, Clone)]
pub struct Limits {
    /// Rounds one turn may take before it is stopped.
    /// A backstop against a model that calls tools forever, not a budget: reaching it ends the turn
    /// as [`TurnStatus::LimitReached`] rather than as an error.
    pub max_rounds: u32,
    /// How deep sub-agents may nest.
    pub max_depth: u32,
    /// Tool output longer than this is offloaded to the object store.
    pub max_result_chars: usize,
    /// 0 = no limit.
    pub tool_timeout_secs: u64,
    pub max_parallel_tools: usize,
    /// Turn-end wait budget for shell-created background tasks, in seconds. 0 = don't wait.
    pub task_wait_secs: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_rounds: 40,
            max_depth: 2,
            max_result_chars: 30_000,
            tool_timeout_secs: 0,
            max_parallel_tools: 16,
            task_wait_secs: 60,
        }
    }
}

/// A model for an auxiliary call — summarising, titling, approving.
/// Resolved by the engine, which owns routing. Core never learns what a role is; it is handed a
/// model and something to reach it with.
#[derive(Clone)]
pub struct AuxModel {
    pub model: ResolvedModel,
    pub client: Arc<dyn LlmClient>,
}

impl AuxModel {
    pub(crate) fn model_key(&self) -> String {
        format!(
            "{}:{}",
            self.model.source.provider_id, self.model.source.model_id
        )
    }
}

/// What one turn is asked to do, pinned at its start.
enum BudgetAnswer {
    Continue,
    Stop,
}

struct TurnTaskWait {
    drained: Drained,
    still_running: Vec<TaskRun>,
}

fn running_turn_tasks<'a>(tasks: &'a [TaskRun], turn_id: TurnId) -> Vec<&'a TaskRun> {
    tasks
        .iter()
        .filter(|task| {
            task.turn_scoped
                && !task.state.is_terminal()
                && matches!(task.executor, ExecutorSpec::Process(_))
                && matches!(
                    &task.trigger,
                    TaskTrigger::Tool {
                        turn_id: created, ..
                    } if *created == turn_id
                )
        })
        .collect()
}

fn task_title(task: &TaskRun) -> String {
    match &task.executor {
        ExecutorSpec::Process(spec) => {
            let mut title = spec.program.clone();
            if !spec.args.is_empty() {
                title.push(' ');
                title.push_str(&spec.args.join(" "));
            }
            title
        }
        ExecutorSpec::Agent(spec) => spec.agent.clone(),
    }
}

pub struct TurnPlan {
    pub model: ResolvedModel,
    pub client: Arc<dyn LlmClient>,
    /// The model that writes summaries. `None` falls back to the conversation's own model.
    /// Falling back rather than refusing is the router's rule made concrete: every auxiliary chain
    /// ends at `session`, so an unconfigured aux role can never land on the mock client and silently
    /// produce a summary that is not a summary.
    pub compaction: Option<AuxModel>,
    /// System-prompt sections, assembled by [`context::build_system`].
    pub system: Vec<String>,
    pub thinking: ThinkingIntent,
    /// `None` = every registered tool. Applied at materialisation, so an excluded tool is never
    /// offered to the model rather than being refused once called.
    pub tools_allow: Option<Vec<String>>,
    /// Usage attribution. `Main` for the user's own conversation, `Agent(name)` for a sub-agent.
    pub purpose: Purpose,
    /// Things the caller worked out **before** the turn existed and the user has to be told about:
    /// an untrusted extension definition, a model that had to be swapped, or an active budget limit.
    /// They arrive on the plan rather than being logged by the assembler because a notice has to be
    /// *persisted*, and only a turn has an emitter that persists — see [`TurnEmitter::notice`]. They
    /// are emitted first, so the explanation is in the transcript ahead of the reply it explains.
    pub notices: Vec<PlanNotice>,
    pub budget: Option<Arc<budget::TurnBudget>>,
}

/// A notice the caller of [`Core::run`] wants emitted at the start of the turn.
#[derive(Debug, Clone)]
pub struct PlanNotice {
    pub level: NoticeLevel,
    /// A stable, machine-readable reason. Grouped on by clients, so it is not free-form prose.
    pub code: String,
    /// The default rendering of the notice (fallback text for hosts without a
    /// catalogue entry for `code`).
    pub message: String,
    /// Scalar interpolation values for the localized rendering of the notice.
    pub args: std::collections::BTreeMap<String, serde_json::Value>,
}

impl PlanNotice {
    pub fn warn(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            level: NoticeLevel::Warn,
            code: code.into(),
            message: message.into(),
            args: std::collections::BTreeMap::new(),
        }
    }

    pub fn info(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            level: NoticeLevel::Info,
            code: code.into(),
            message: message.into(),
            args: std::collections::BTreeMap::new(),
        }
    }

    /// [`Self::info`] plus interpolation values for the localized notice.
    pub fn info_with_args(
        code: impl Into<String>,
        message: impl Into<String>,
        args: std::collections::BTreeMap<String, serde_json::Value>,
    ) -> Self {
        Self {
            level: NoticeLevel::Info,
            code: code.into(),
            message: message.into(),
            args,
        }
    }

    /// [`Self::warn`] plus interpolation values for the localized notice.
    pub fn warn_with_args(
        code: impl Into<String>,
        message: impl Into<String>,
        args: std::collections::BTreeMap<String, serde_json::Value>,
    ) -> Self {
        Self {
            level: NoticeLevel::Warn,
            code: code.into(),
            message: message.into(),
            args,
        }
    }
}

impl TurnPlan {
    pub fn new(model: ResolvedModel, client: Arc<dyn LlmClient>) -> Self {
        Self {
            model,
            client,
            compaction: None,
            system: Vec::new(),
            thinking: ThinkingIntent::default(),
            tools_allow: None,
            purpose: Purpose::Main,
            notices: Vec::new(),
            budget: None,
        }
    }

    pub fn with_compaction_model(mut self, aux: AuxModel) -> Self {
        self.compaction = Some(aux);
        self
    }

    /// The model summaries are written with.
    pub(crate) fn compaction_model(&self) -> AuxModel {
        self.compaction.clone().unwrap_or_else(|| AuxModel {
            model: self.model.clone(),
            client: self.client.clone(),
        })
    }

    pub fn with_system(mut self, sections: Vec<String>) -> Self {
        self.system = sections;
        self
    }

    pub fn with_tools(mut self, allow: Option<Vec<String>>) -> Self {
        self.tools_allow = allow;
        self
    }

    pub fn for_purpose(mut self, purpose: Purpose) -> Self {
        self.purpose = purpose;
        self
    }

    pub(crate) fn model_ref(&self) -> ModelRef {
        ModelRef {
            provider_id: self.model.source.provider_id.clone(),
            model_id: self.model.source.model_id.clone(),
            display_name: self.model.display_name.clone(),
        }
    }

    /// `provider:model`, the form usage rows store.
    pub(crate) fn model_key(&self) -> String {
        format!(
            "{}:{}",
            self.model.source.provider_id, self.model.source.model_id
        )
    }
}

/// How a turn ended.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnOutcome {
    pub turn_id: TurnId,
    pub turn_seq: i64,
    pub status: TurnStatus,
    pub stats: TurnStats,
    /// The assistant text of the final round — a sub-agent's conclusion, and what a title is
    /// drafted from. Everything else stays in the entries.
    pub answer: String,
}

/// One item entering a new turn.
/// Most callers use [`Core::run`] and therefore produce `User`. Dispatcher uses `Submitted` for
/// API input and `TaskUpdate` for engine-generated mailbox records, so persistence retains both
/// structures until the model-context boundary.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnInput {
    User(ContentPart),
    /// A UI/API submission part. Persisted verbatim; files become model text in `build_context`.
    Submitted(zlogic_protocol::MessagePart),
    TaskUpdate(zlogic_protocol::TaskUpdatePart),
}

pub struct Core {
    services: Arc<CoreServices>,
    session_id: SessionId,
    /// The workspace root, injected per turn. Never stored on the session.
    root: PathBuf,
    agent: AgentRef,
    depth: u32,
    sink: Arc<dyn EventSink>,
    /// Wired in when sub-agents are available. `create_agent` fails closed without it.
    spawner: Option<Arc<dyn AgentSpawner>>,
    agent_mailbox: Option<Arc<AgentMailboxGate>>,
    /// Wired in when a skill library is available. `skill` fails closed without it.
    /// Per-run like [`Core::worktree`]: the library is read relative to one workspace root.
    skills: Option<Arc<dyn SkillHost>>,
    /// Wired in when worktrees are available. `enter_worktree` / `exit_worktree` fail closed
    /// without it.
    /// Per-run rather than in [`CoreServices`], because it is bound to one session: what it
    /// changes is that session's `exec_cwd`.
    worktree: Option<Arc<dyn WorktreeHost>>,
    /// The tool set for this run. `None` = [`CoreServices::tools`], the built-ins.
    /// Per-run rather than in [`CoreServices`], because the extension part of the tool set is
    /// **per workspace**: MCP servers are configured globally *and* inside a repository, so two
    /// sessions in the same process legitimately see different tools. The registry is a map of
    /// handles, so carrying one per run is cheap.
    tools: Option<ToolRegistry>,
    /// Lifecycle hooks loaded for this workspace and turn.
    hooks: Option<Arc<dyn zlogic_hooks::HookRunner>>,
}

impl Core {
    pub fn new(
        services: Arc<CoreServices>,
        session_id: SessionId,
        root: impl Into<PathBuf>,
        sink: Arc<dyn EventSink>,
    ) -> Self {
        Self {
            services,
            session_id,
            root: root.into(),
            agent: AgentRef::root(),
            depth: 0,
            sink,
            spawner: None,
            agent_mailbox: None,
            skills: None,
            worktree: None,
            tools: None,
            hooks: None,
        }
    }

    /// Replaces the tool set for this run — built-ins plus whatever the workspace's extensions
    /// contribute. Without it a run sees [`CoreServices::tools`] only.
    pub fn with_tools(mut self, tools: Option<ToolRegistry>) -> Self {
        self.tools = tools;
        self
    }

    pub fn with_hooks(mut self, hooks: Option<Arc<dyn zlogic_hooks::HookRunner>>) -> Self {
        self.hooks = hooks;
        self
    }

    pub(crate) fn hooks(&self) -> Option<&Arc<dyn zlogic_hooks::HookRunner>> {
        self.hooks.as_ref()
    }

    pub fn with_spawner(mut self, spawner: Option<Arc<dyn AgentSpawner>>) -> Self {
        self.spawner = spawner;
        self
    }

    pub fn with_agent_mailbox(mut self, mailbox: Option<Arc<AgentMailboxGate>>) -> Self {
        self.agent_mailbox = mailbox;
        self
    }

    /// Wires in the skill library. Without it `skill` reports itself as unavailable.
    pub fn with_skills(mut self, skills: Option<Arc<dyn SkillHost>>) -> Self {
        self.skills = skills;
        self
    }

    /// Wires in worktrees. Without it `enter_worktree` / `exit_worktree` report themselves as
    /// unavailable rather than failing halfway through creating one.
    pub fn with_worktree(mut self, worktree: Option<Arc<dyn WorktreeHost>>) -> Self {
        self.worktree = worktree;
        self
    }

    /// Marks this run as a sub-agent's, `depth` levels below the root.
    pub fn as_sub_agent(mut self, agent: AgentRef, depth: u32) -> Self {
        self.agent = agent;
        self.depth = depth;
        self
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn depth(&self) -> u32 {
        self.depth
    }

    pub(crate) fn services(&self) -> &Arc<CoreServices> {
        &self.services
    }

    pub(crate) fn spawner(&self) -> Option<Arc<dyn AgentSpawner>> {
        self.spawner.clone()
    }

    pub(crate) fn skills(&self) -> Option<Arc<dyn SkillHost>> {
        self.skills.clone()
    }

    pub(crate) fn worktree(&self) -> Option<Arc<dyn WorktreeHost>> {
        self.worktree.clone()
    }

    async fn ask_budget(
        &self,
        emitter: &Arc<TurnEmitter>,
        turn_id: TurnId,
        turn_seq: i64,
        cancel: &CancellationToken,
        message: &str,
        stats: &mut TurnStats,
    ) -> BudgetAnswer {
        let Some(delegate) = self.services.interaction.clone() else {
            return BudgetAnswer::Stop;
        };
        if self.depth > 0 {
            return BudgetAnswer::Stop;
        }

        let asker = round::RoundInteractions {
            delegate,
            store: self.services.store.clone(),
            emitter: emitter.clone(),
            session_id: self.session_id,
            turn_id,
            turn_seq,
            cancel: cancel.clone(),
        };
        let form = Form::single_choice(
            "Budget limit reached",
            "action",
            vec![
                Choice::new(
                    "continue",
                    "Continue (do not ask again about this scope this turn)",
                ),
                Choice::new("stop", "Stop here"),
            ],
        )
        .with_message(message.to_string());
        let request = InteractionRequest {
            interaction_id: zlogic_protocol::EntryId::new().to_string(),
            session_id: self.session_id,
            turn_id,
            call_id: None,
            body: InteractionBody::Form(form),
        };

        stats.interactions += 1;
        let started = std::time::Instant::now();
        let decision = asker.ask(request).await;
        stats.interaction_wait_ms += started.elapsed().as_millis() as u64;

        match decision {
            Ok(InteractionDecision::Submitted(answer))
                if answer.text("action") == Some("continue") =>
            {
                BudgetAnswer::Continue
            }
            _ => BudgetAnswer::Stop,
        }
    }

    async fn wait_for_turn_tasks(
        &self,
        emitter: &Arc<TurnEmitter>,
        turn_id: TurnId,
        turn_seq: i64,
        cancel: &CancellationToken,
    ) -> Result<TurnTaskWait> {
        let none = TurnTaskWait {
            drained: Drained::default(),
            still_running: Vec::new(),
        };
        let Some(tasks) = &self.services.tasks else {
            return Ok(none);
        };
        let budget = self.services.limits.task_wait_secs;
        if budget == 0 || self.depth > 0 {
            return Ok(none);
        }
        let session = self.session_id;
        let listed = tasks.list(session).await.map_err(CoreError::Invalid)?;
        let mut running = running_turn_tasks(&listed, turn_id);
        if running.is_empty() {
            return Ok(none);
        }

        const POLL: std::time::Duration = std::time::Duration::from_millis(250);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(budget);
        loop {
            tokio::time::sleep(POLL).await;
            if cancel.is_cancelled() {
                break;
            }
            if self.has_pending_steer() {
                break;
            }
            let listed = tasks.list(session).await.map_err(CoreError::Invalid)?;
            running = running_turn_tasks(&listed, turn_id);
            if running.is_empty() {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
        }

        let drained = self.drain_steering(emitter, turn_id, turn_seq).await?;
        let listed = tasks.list(session).await.map_err(CoreError::Invalid)?;
        Ok(TurnTaskWait {
            drained,
            still_running: running_turn_tasks(&listed, turn_id)
                .into_iter()
                .cloned()
                .collect(),
        })
    }

    fn has_pending_steer(&self) -> bool {
        self.services
            .store
            .with(|db| db.mailbox().pending(self.session_id))
            .map(|pending| {
                pending
                    .iter()
                    .any(|record| record.delivery == Delivery::Steer)
            })
            .unwrap_or(false)
    }

    /// Runs one turn to completion.
    /// `input` is the user's content parts. Empty is allowed: that is how a turn continues from
    /// history the model already has.
    /// `cancel` is the conversation's token — the same one for every turn of this conversation, and
    /// the same one handed to tools and sub-agents. See the module docs for what cancelling does.
    /// # The caller allocates `turn_id`
    /// Not core. Whoever dispatches the turn has to be able to associate the id with its cancel
    /// token, its lock and its registry entry **before** anything starts; if core minted the id
    /// there would be a window where the turn is running under an id nobody upstream has yet, and a
    /// `CancelTurn { turn_id }` arriving in that window would silently find nothing. Worse, the id
    /// in the event stream and the id the lock was taken under would be different values.
    pub async fn run(
        &self,
        turn_id: TurnId,
        plan: TurnPlan,
        input: Vec<ContentPart>,
        cancel: CancellationToken,
    ) -> Result<TurnOutcome> {
        self.run_with_input(
            turn_id,
            plan,
            input.into_iter().map(TurnInput::User).collect(),
            cancel,
        )
        .await
    }

    /// Runs a manual context compaction as its own non-conversational turn.
    /// This deliberately does not persist a fake user message and never enters the main round
    /// loop. It still uses the ordinary turn stream so every client observes one consistent
    /// lifecycle and can cancel the summary call by turn id.
    pub async fn compact_context(
        &self,
        turn_id: TurnId,
        mut plan: TurnPlan,
        cancel: CancellationToken,
    ) -> Result<TurnOutcome> {
        // The same tool set a turn materialises, and the same deferred-tool catalogue appended to
        // the system prompt. `run_with_input` does this for its own reason; the extra reason here
        // is that the summary call rides the conversation's prompt prefix, and a provider's cache
        // matches that prefix byte for byte — assemble it differently and the whole window is
        // re-billed at full price, which is the cost this path exists to avoid.
        let registry = self.tools.as_ref().unwrap_or(&self.services.tools);
        let materialized = registry
            .materialize_for(plan.tools_allow.as_deref(), self.depth == 0)
            .with_loaded(self.loaded_tool_names()?);
        if let Some(catalogue) = materialized.deferred_prompt() {
            plan.system.push(catalogue);
        }
        let turn_seq = self
            .services
            .store
            .with(|db| db.entries().next_turn_seq(self.session_id))?;
        let emitter = TurnEmitter::new(
            self.sink.clone(),
            self.session_id,
            turn_id,
            self.agent.clone(),
            self.services.store.clone(),
            self.services.objects.clone(),
            turn_seq,
        );
        emitter.send(StreamPayload::TurnStart {
            model: plan.model_ref(),
            resumed: false,
            proactive: true,
        });

        // Anything the caller worked out before the turn existed, ahead of the result it explains —
        // the same rule `run_with_input` follows. Assembling the system prompt and the tool sets
        // fills this in (an unreadable memory, an extension whose tools could not be loaded), and a
        // notice nobody emits is a notice nobody sees.
        for notice in &plan.notices {
            emitter.notice(
                notice.level,
                &notice.code,
                notice.message.clone(),
                notice.args.clone(),
            );
        }

        let mut stats = TurnStats::default();
        let mut blocked_reason = None;
        if let Some(runner) = self.hooks() {
            let outcome = runner
                .run(
                    &zlogic_hooks::HookRequest {
                        event: zlogic_hooks::HookEvent::ContextCompactBefore,
                        session_id: self.session_id.to_string(),
                        cwd: self.exec_cwd().unwrap_or_else(|_| self.root.clone()),
                        payload: serde_json::json!({"reason": CompactionReason::Manual}),
                        matchers: Default::default(),
                    },
                    &cancel,
                )
                .await;
            for warning in outcome.warnings {
                emitter.notice(
                    NoticeLevel::Warn,
                    "hook_failed",
                    warning,
                    std::collections::BTreeMap::new(),
                );
            }
            blocked_reason = outcome.blocked;
        }

        let blocked = blocked_reason.is_some();
        let result = if let Some(reason) = blocked_reason {
            emitter.notice(
                NoticeLevel::Warn,
                "compaction_blocked",
                format!("manual context compaction was blocked by a hook: {reason}"),
                std::collections::BTreeMap::new(),
            );
            Ok(false)
        } else {
            tokio::select! {
                biased;
                () = cancel.cancelled() => Ok(false),
                written = self.compact(
                    &plan,
                    &materialized,
                    &emitter,
                    turn_id,
                    turn_seq,
                    CompactionReason::Manual,
                ) => written,
            }
        };

        match result {
            Ok(written) => {
                stats.compactions = u32::from(written);
                if cancel.is_cancelled() && !written {
                    emitter.notice(
                        NoticeLevel::Info,
                        "compaction_cancelled",
                        "manual context compaction was cancelled",
                        std::collections::BTreeMap::new(),
                    );
                }
                if !blocked && let Some(runner) = self.hooks() {
                    let outcome = runner
                        .run(
                            &zlogic_hooks::HookRequest {
                                event: zlogic_hooks::HookEvent::ContextCompactAfter,
                                session_id: self.session_id.to_string(),
                                cwd: self.exec_cwd().unwrap_or_else(|_| self.root.clone()),
                                payload: serde_json::json!({
                                    "reason": CompactionReason::Manual,
                                    "written": written,
                                }),
                                matchers: Default::default(),
                            },
                            &cancel,
                        )
                        .await;
                    for warning in outcome.warnings {
                        emitter.notice(
                            NoticeLevel::Warn,
                            "hook_failed",
                            warning,
                            std::collections::BTreeMap::new(),
                        );
                    }
                }
                let status = if cancel.is_cancelled() {
                    TurnStatus::Cancelled
                } else {
                    TurnStatus::Completed
                };
                emitter.turn_end(&status, None, &stats);
                Ok(TurnOutcome {
                    turn_id,
                    turn_seq,
                    status,
                    stats,
                    answer: String::new(),
                })
            }
            Err(error) => {
                emitter.notice(
                    NoticeLevel::Warn,
                    "compaction_failed",
                    format!("manual context compaction failed: {error}"),
                    std::collections::BTreeMap::new(),
                );
                emitter.error(&error);
                emitter.turn_end(&TurnStatus::Failed, Some(&error.to_string()), &stats);
                Ok(TurnOutcome {
                    turn_id,
                    turn_seq,
                    status: TurnStatus::Failed,
                    stats,
                    answer: String::new(),
                })
            }
        }
    }

    /// Runs a turn whose initial input may include engine-generated task updates.
    pub async fn run_with_input(
        &self,
        turn_id: TurnId,
        mut plan: TurnPlan,
        input: Vec<TurnInput>,
        cancel: CancellationToken,
    ) -> Result<TurnOutcome> {
        let turn_seq = self
            .services
            .store
            .with(|db| db.entries().next_turn_seq(self.session_id))?;

        let emitter = Arc::new(TurnEmitter::new(
            self.sink.clone(),
            self.session_id,
            turn_id,
            self.agent.clone(),
            self.services.store.clone(),
            self.services.objects.clone(),
            turn_seq,
        ));
        // The clone is what keeps the failure path below able to persist the message — resolution
        // consumes its input, and by this point the mailbox row is already gone. A few strings per
        // turn; do not "optimise" it away.
        let input = match self.resolve_turn_skills(input.clone()).await {
            Ok(input) => input,
            Err(error) => {
                let stats = TurnStats::default();
                emitter.send(StreamPayload::TurnStart {
                    model: plan.model_ref(),
                    resumed: false,
                    proactive: false,
                });
                // **The message is persisted even though the turn is already over.** Its mailbox row
                // was removed when this turn was dispatched, so what is in hand here is the only
                // remaining copy — returning without writing it makes a submission the user watched
                // leave the composer vanish from the history, with nothing left to redeliver.
                self.persist_unresolved_input(turn_id, turn_seq, input);
                emitter.notice(
                    NoticeLevel::Warn,
                    "skill_load_failed",
                    format!("could not load the selected skill: {error}"),
                    std::collections::BTreeMap::new(),
                );
                emitter.error(&error);
                emitter.turn_end(&TurnStatus::Failed, Some(&error.to_string()), &stats);
                return Ok(TurnOutcome {
                    turn_id,
                    turn_seq,
                    status: TurnStatus::Failed,
                    stats,
                    answer: String::new(),
                });
            }
        };

        // Before anything else: actionable state the caller already knew about has to precede the
        // reply whose behavior it may explain.
        for notice in &plan.notices {
            emitter.notice(
                notice.level,
                &notice.code,
                notice.message.clone(),
                notice.args.clone(),
            );
        }

        // Input is persisted before the first request, so a crash mid-round leaves the user's
        // message in the history rather than losing it.
        for input in input {
            let entry = match input {
                TurnInput::User(part) => entry_data::to_entry(
                    self.session_id,
                    turn_id,
                    turn_seq,
                    None,
                    &entry_data::Author::User,
                    part,
                )?,
                TurnInput::Submitted(MessagePart::SkillLoad {
                    name,
                    revision,
                    body_object,
                    path,
                    loaded_by,
                    unsupported,
                }) => entry_data::skill_load_entry(
                    self.session_id,
                    turn_id,
                    turn_seq,
                    name,
                    revision,
                    body_object,
                    path,
                    loaded_by,
                    unsupported,
                )?,
                TurnInput::Submitted(part) => entry_data::input_entry(
                    self.session_id,
                    turn_id,
                    turn_seq,
                    &entry_data::Author::User,
                    part,
                )?,
                TurnInput::TaskUpdate(update) => {
                    entry_data::task_update_entry(self.session_id, turn_id, turn_seq, &update)?
                }
            };
            self.append(entry)?;
        }

        emitter.send(StreamPayload::TurnStart {
            model: plan.model_ref(),
            resumed: false,
            proactive: false,
        });

        let registry = self.tools.as_ref().unwrap_or(&self.services.tools);
        // Tools loaded via `load_tool` are session state (recorded in `kind: tool_load` entries),
        // so the next turn seeds its materialised set from them instead of asking the model to
        // load everything again.
        let materialized = registry
            .materialize_for(plan.tools_allow.as_deref(), self.depth == 0)
            .with_loaded(self.loaded_tool_names()?);
        if let Some(catalogue) = materialized.deferred_prompt() {
            plan.system.push(catalogue);
        }

        let grants = GrantSet::default();
        let mut stats = TurnStats::default();
        let mut answer = String::new();
        let mut status = TurnStatus::Completed;
        let mut produced_content = false;
        let mut limit_reason: Option<String> = None;
        let mut task_wait_used = false;

        for round_seq in 1..=self.services.limits.max_rounds {
            let ctx = round::RoundCtx {
                core: self,
                plan: &plan,
                emitter: &emitter,
                tools: &materialized,
                grants: &grants,
                turn_id,
                turn_seq,
                round_seq,
                turn_usage: stats.usage,
                cancel: &cancel,
                asked: Default::default(),
                wait_ms: Default::default(),
                approval: Arc::new(tokio::sync::Mutex::new(())),
            };

            let round = match ctx.run().await {
                Ok(r) => r,
                Err(e) => {
                    if let Some(hooks) = self.hooks() {
                        let outcome = hooks
                            .run(
                                &zlogic_hooks::HookRequest {
                                    event: zlogic_hooks::HookEvent::TurnFailed,
                                    session_id: self.session_id.to_string(),
                                    cwd: self.exec_cwd().unwrap_or_else(|_| self.root.clone()),
                                    payload: serde_json::json!({"error": e.to_string()}),
                                    matchers: Default::default(),
                                },
                                &cancel,
                            )
                            .await;
                        for warning in outcome.warnings {
                            emitter.notice(
                                NoticeLevel::Warn,
                                "hook_failed",
                                warning,
                                std::collections::BTreeMap::new(),
                            );
                        }
                    }
                    // `Error` is not terminal on this stream: the reason goes out first, the turn's
                    // own end event after it.
                    emitter.error(&e);
                    emitter.turn_end(&TurnStatus::Failed, Some(&e.to_string()), &stats);
                    tracing::info!(
                        target: "zlogic::core",
                        session_id = %self.session_id,
                        %turn_id,
                        turn_seq,
                        status = ?TurnStatus::Failed,
                        rounds = stats.rounds,
                        duration_ms = stats.duration_ms,
                        "turn run finished (round error)"
                    );
                    return Ok(TurnOutcome {
                        turn_id,
                        turn_seq,
                        status: TurnStatus::Failed,
                        stats,
                        answer,
                    });
                }
            };

            stats.add_round(&round.stats);
            stats.interactions += round.interactions;
            stats.interaction_wait_ms += round.interaction_wait_ms;
            stats.compactions += round.compactions;
            if let Some(budget) = &plan.budget {
                if let Some(cost) = &round.stats.cost {
                    budget.add_round(cost);
                }
                stats.cost = budget.turn_total();
            }
            if !round.text.is_empty() {
                answer = round.text.clone();
                produced_content = true;
            }
            if round.stats.tools.total > 0 {
                produced_content = true;
            }

            // Cancellation wins over whatever the round would otherwise have reported: the round
            // stopped early, so its outcome describes where it stopped, not how the turn went.
            if round.cancelled {
                status = TurnStatus::Cancelled;
                break;
            }

            let mut done = true;
            match round.outcome {
                RoundOutcome::ToolCalls => {
                    const MAX_PRECHECK_RETRIES: u32 = 3;
                    if stats.tools.precheck_failed > MAX_PRECHECK_RETRIES {
                        emitter.notice(
                            NoticeLevel::Warn,
                            "tool_precheck_retries_exhausted",
                            format!(
                                "The model kept sending tool calls with invalid arguments \
                                 ({} attempt(s)); stopped retrying.",
                                stats.tools.precheck_failed
                            ),
                            std::collections::BTreeMap::new(),
                        );
                        status = TurnStatus::Incomplete(IncompleteReason::InvalidToolCalls);
                        done = true;
                    } else {
                        done = false;
                    }
                }
                // A message arrived while the model was answering. It continues this turn rather
                // than starting a new one, which is what makes "wait, also do X" feel like part of
                // the same exchange.
                RoundOutcome::FinalAnswerWithMailbox => done = false,
                RoundOutcome::FinalAnswer => {
                    // Turn-end wait: shell background tasks created by this turn (compile, test)
                    // get a bounded wait so their result can land in the same reply. Completion
                    // notifications land in the mailbox; a non-empty drain continues the turn —
                    // the same mechanism as FinalAnswerWithMailbox. Tasks still running after the
                    // budget are reported to the model so the reply can say so.
                    if !task_wait_used {
                        task_wait_used = true;
                        let wait = self
                            .wait_for_turn_tasks(&emitter, turn_id, turn_seq, &cancel)
                            .await?;
                        if !wait.drained.is_empty() {
                            done = false;
                        } else if !wait.still_running.is_empty() {
                            let list = wait
                                .still_running
                                .iter()
                                .map(|task| task_title(task))
                                .collect::<Vec<_>>()
                                .join("\n");
                            emitter.notice_transient(
                                NoticeLevel::Info,
                                "tasks_still_running",
                                format!(
                                    "Background tasks started by this turn are still running; you will be notified when they finish:\n{list}"
                                ),
                                {
                                    let mut args = std::collections::BTreeMap::new();
                                    args.insert("tasks".into(), serde_json::Value::String(list.clone()));
                                    args
                                },
                            );
                        }
                    }
                    if done && let Some(hooks) = self.hooks() {
                        let outcome = hooks
                            .run(
                                &zlogic_hooks::HookRequest {
                                    event: zlogic_hooks::HookEvent::TurnFinishBefore,
                                    session_id: self.session_id.to_string(),
                                    cwd: self.exec_cwd().unwrap_or_else(|_| self.root.clone()),
                                    payload: serde_json::json!({
                                        "answer": answer,
                                        "round": round_seq,
                                    }),
                                    matchers: Default::default(),
                                },
                                &cancel,
                            )
                            .await;
                        for warning in outcome.warnings {
                            emitter.notice(
                                NoticeLevel::Warn,
                                "hook_failed",
                                warning,
                                std::collections::BTreeMap::new(),
                            );
                        }
                        let mut feedback = outcome.contexts;
                        if let Some(reason) = outcome.blocked {
                            feedback.push(reason);
                        }
                        if !feedback.is_empty() {
                            let part = ContentPart::Text(TextPart {
                                text: format!(
                                    "Hook feedback before finishing:\n{}",
                                    feedback.join("\n")
                                ),
                                raw: None,
                                truncated: false,
                            });
                            let entry = entry_data::to_entry(
                                self.session_id,
                                turn_id,
                                turn_seq,
                                None,
                                &entry_data::Author::Steering,
                                part,
                            )?;
                            self.append(entry)?;
                            done = false;
                        }
                    }
                    if done {
                        if round.finish == FinishReason::Other {
                            status = TurnStatus::Incomplete(IncompleteReason::UnknownStopReason);
                        } else if !produced_content {
                            status = TurnStatus::Incomplete(IncompleteReason::EmptyReply);
                        }
                    }
                }
                RoundOutcome::Truncated => {
                    status = TurnStatus::Incomplete(round.incomplete_reason())
                }
                RoundOutcome::Paused => {
                    status = TurnStatus::Incomplete(IncompleteReason::Interrupted)
                }
                RoundOutcome::Failed => status = TurnStatus::Failed,
            }
            if cancel.is_cancelled() {
                status = TurnStatus::Cancelled;
                break;
            }
            if done {
                break;
            }
            if let Some(budget) = &plan.budget {
                match budget.checkpoint() {
                    budget::Enforcement::Continue => {}
                    budget::Enforcement::Warn { message } => {
                        emitter.notice(
                            NoticeLevel::Warn,
                            "budget",
                            message,
                            std::collections::BTreeMap::new(),
                        );
                    }
                    budget::Enforcement::Stop { message } => {
                        status = TurnStatus::LimitReached;
                        limit_reason = Some(message.clone());
                        emitter.notice(
                            NoticeLevel::Warn,
                            "limit_reached",
                            message,
                            std::collections::BTreeMap::new(),
                        );
                        break;
                    }
                    budget::Enforcement::Ask { scope, message } => {
                        match self
                            .ask_budget(&emitter, turn_id, turn_seq, &cancel, &message, &mut stats)
                            .await
                        {
                            BudgetAnswer::Continue => budget.waive(scope),
                            BudgetAnswer::Stop => {
                                status = TurnStatus::LimitReached;
                                limit_reason = Some(message.clone());
                                emitter.notice(
                                    NoticeLevel::Warn,
                                    "limit_reached",
                                    message,
                                    std::collections::BTreeMap::new(),
                                );
                                break;
                            }
                        }
                    }
                }
            }
            // The loop bound is a backstop, not a budget: reaching it is reported, not an error.
            if round_seq == self.services.limits.max_rounds {
                status = TurnStatus::LimitReached;
                let message = format!(
                    "The round limit for a single submission was reached ({} rounds). The task may not be finished — raise `limits.max_rounds` in config.yaml, or ask me to continue with what is left.",
                    self.services.limits.max_rounds
                );
                limit_reason = Some(message.clone());
                emitter.notice(
                    NoticeLevel::Warn,
                    "limit_reached",
                    message,
                    std::collections::BTreeMap::new(),
                );
            }
        }

        emitter.turn_end(&status, limit_reason.as_deref(), &stats);

        tracing::info!(
            target: "zlogic::core",
            session_id = %self.session_id,
            %turn_id,
            turn_seq,
            status = ?status,
            rounds = stats.rounds,
            duration_ms = stats.duration_ms,
            "turn run finished"
        );

        Ok(TurnOutcome {
            turn_id,
            turn_seq,
            status,
            stats,
            answer,
        })
    }

    /// Persists input whose skill references could not be resolved.
    /// Best-effort and infallible to the caller: it runs on the way out of an already-failing turn,
    /// and turning "could not save the message" into a second error would mask the first.
    fn persist_unresolved_input(&self, turn_id: TurnId, turn_seq: i64, input: Vec<TurnInput>) {
        for item in input {
            let entry = match item {
                TurnInput::User(part) => entry_data::to_entry(
                    self.session_id,
                    turn_id,
                    turn_seq,
                    None,
                    &entry_data::Author::User,
                    part,
                ),
                TurnInput::Submitted(part) => entry_data::input_entry(
                    self.session_id,
                    turn_id,
                    turn_seq,
                    &entry_data::Author::User,
                    unresolved_skill_as_text(part),
                ),
                TurnInput::TaskUpdate(update) => {
                    entry_data::task_update_entry(self.session_id, turn_id, turn_seq, &update)
                }
            };
            if let Err(error) = entry.and_then(|entry| self.append(entry)) {
                tracing::warn!(
                    target: "zlogic::core",
                    "input could not be written to timeline: {error}"
                );
            }
        }
    }

    /// Appends an entry, offloading a large payload to the object store.
    pub(crate) fn append(&self, entry: NewEntry) -> Result<zlogic_store::EntryRecord> {
        let objects = self.services.objects.clone();
        self.services
            .store
            .with(|db| db.entries().append_with_offload(entry, objects.as_ref()))
            .map_err(CoreError::from)
    }

    /// Every deferred tool `load_tool` has made available so far, from the session's
    /// `kind: tool_load` entries.
    /// Append-only and compaction-independent (`list_kind` reads all rows), so the set is the union
    /// of every recorded load. Order does not matter: `Materialized::with_loaded` deduplicates and
    /// drops names the current registry no longer knows.
    pub(crate) fn loaded_tool_names(&self) -> Result<Vec<String>> {
        let session = self.session_id;
        let objects = self.services.objects.clone();
        self.services.store.with(|db| -> Result<Vec<String>> {
            let store = db.entries();
            let loader = context::store_loader(&store, objects.as_ref());
            let mut names = Vec::new();
            for entry in store.list_kind(session, zlogic_store::EntryKind::ToolLoad)? {
                let data = loader(&entry)?;
                names.extend(entry_data::tool_load_names(&entry, &data)?);
            }
            Ok(names)
        })
    }

    /// Where tools run: the session's own deviation if it has one, otherwise the injected root.
    /// Read per call rather than snapshotted, because it can migrate mid-session (into a worktree
    /// and back out).
    pub(crate) fn exec_cwd(&self) -> Result<PathBuf> {
        let session = self
            .services
            .store
            .with(|db| db.sessions().get(self.session_id))?;
        Ok(session.exec_cwd_or(&self.root).to_path_buf())
    }

    pub(crate) fn root(&self) -> &std::path::Path {
        &self.root
    }
}

/// An unresolved `/skill` reference, rendered back to the text it was typed as.
/// [`entry_data::input_entry`] refuses a `Skill` part on purpose: an unresolved reference is not
/// persistable session state, because nothing has established which revision it names. But
/// `/review the diff` is still the user's own sentence, so it is kept as text rather than dropped —
/// the alternative is a history that silently omits what was asked for. Every other part persists
/// unchanged.
fn unresolved_skill_as_text(part: MessagePart) -> MessagePart {
    match part {
        MessagePart::Skill { name, args } => {
            let args = args
                .map(|args| args.trim().to_string())
                .filter(|args| !args.is_empty());
            MessagePart::Text {
                text: match args {
                    Some(args) => format!("/{name} {args}"),
                    None => format!("/{name}"),
                },
            }
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A turn that fails before it starts must not take the user's message with it.
    #[test]
    fn an_unresolved_skill_reference_is_kept_as_the_text_it_was_typed_as() {
        assert_eq!(
            unresolved_skill_as_text(MessagePart::Skill {
                name: "review".into(),
                args: Some("  the diff ".into()),
            }),
            MessagePart::Text {
                text: "/review the diff".into()
            }
        );
        assert_eq!(
            unresolved_skill_as_text(MessagePart::Skill {
                name: "review".into(),
                args: None,
            }),
            MessagePart::Text {
                text: "/review".into()
            }
        );
        // Everything else is the user's message already and is persisted as it stands.
        let text = MessagePart::Text {
            text: "hello".into(),
        };
        assert_eq!(unresolved_skill_as_text(text.clone()), text);
    }

    #[test]
    fn a_poisoned_store_lock_does_not_kill_the_session() {
        let store = SharedStore::new(zlogic_store::Db::open_in_memory().unwrap());
        let clone = store.clone();
        // Poison it: the panic happens inside the closure, with the lock held.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            clone.with(|_| panic!("boom"));
        }));

        // SQLite rolled its own transaction back and the connection is still usable; one bad tool
        // call must not make every later query fail.
        let n = store.with(|db| {
            db.sessions()
                .list(zlogic_protocol::WorkspaceId::new())
                .unwrap()
                .len()
        });
        assert_eq!(n, 0);
    }

    #[test]
    fn limits_default_to_a_backstop_not_a_budget() {
        let l = Limits::default();
        assert!(
            l.max_rounds >= 20,
            "a normal tool chain must not reach this"
        );
        assert_eq!(l.tool_timeout_secs, 0, "no timeout unless configured");
    }
}
