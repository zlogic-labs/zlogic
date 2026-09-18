//! The shared harness for core's integration tests.
//! Every test here drives a real `Core` over a real in-memory database with a scripted client, so
//! what is exercised is the actual round loop rather than a stand-in for it.

// Each test binary uses a different subset of this.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use zlogic_core::{
    AuxModel, CancellationToken, ContextPolicy, Core, CoreServices, EventSink, Limits,
    PolicyDecision, PolicyGate, PolicyRequest, RecordingSink, SharedStore, TurnPlan,
};
use zlogic_llm::mock::{MockClient, MockScript};
use zlogic_llm::{EventStream, LlmClient};
use zlogic_objects::{MemoryObjectStore, ObjectStore};
use zlogic_protocol::config::{ClientSpec, Pricing, ResolvedModel, Sdk};
use zlogic_protocol::interaction::{
    GrantScope, InteractionBody, InteractionDecision, InteractionPort, InteractionRequest,
};
use zlogic_protocol::llm::{FinishReason, LlmRequest};
use zlogic_protocol::message::{ContentPart, Source, TextPart};
use zlogic_protocol::stream::StreamPayload;
use zlogic_protocol::{SessionId, WorkspaceId};
use zlogic_store::{Db, EntryKind, EntryRecord, NewSession};
use zlogic_tools::{
    AgentSpawner, TaskHost, Tool, ToolCtx, ToolExecResult, ToolExposure, ToolMeta, ToolRegistry,
    ToolRisk,
};

// ─────────────────────────── clients ───────────────────────────

/// Plays a queue of scripted responses, one per request, and keeps what it was sent.
/// `MockClient` answers every request identically, which cannot express a tool round followed by a
/// final answer — the shape most of these tests are about.
pub struct Scripted {
    scripts: Mutex<std::collections::VecDeque<MockScript>>,
    requests: Mutex<Vec<LlmRequest>>,
}

impl Scripted {
    pub fn new(scripts: Vec<MockScript>) -> Arc<Self> {
        Arc::new(Self {
            scripts: Mutex::new(scripts.into()),
            requests: Mutex::new(Vec::new()),
        })
    }

    pub fn requests(&self) -> Vec<LlmRequest> {
        self.requests.lock().unwrap().clone()
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

#[async_trait]
impl LlmClient for Scripted {
    async fn stream(&self, req: LlmRequest) -> Result<EventStream, zlogic_protocol::llm::LlmError> {
        self.requests.lock().unwrap().push(req.clone());
        let script = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            // Running past the script means the loop did not stop when it should have; say so
            // loudly rather than quietly repeating the last answer.
            .unwrap_or_else(|| MockScript::text("[script exhausted]"));
        MockClient::new(script).stream(req).await
    }
}

// ─────────────────────────── gates ───────────────────────────

pub struct AllowAll;

#[async_trait]
impl PolicyGate for AllowAll {
    async fn evaluate(&self, _r: &PolicyRequest) -> PolicyDecision {
        PolicyDecision::Allow
    }
}

pub struct DenyAll;

#[async_trait]
impl PolicyGate for DenyAll {
    async fn evaluate(&self, _r: &PolicyRequest) -> PolicyDecision {
        PolicyDecision::Deny {
            reason: "not in this test".into(),
        }
    }
}

/// Always asks, so the interaction path is exercised.
pub struct AlwaysAsk;

#[async_trait]
impl PolicyGate for AlwaysAsk {
    async fn evaluate(&self, r: &PolicyRequest) -> PolicyDecision {
        PolicyDecision::Ask {
            body: InteractionBody::Permission {
                tool: r.tool.name.clone(),
                args_preview: r.args.clone(),
                reason: "test".into(),
                caveats: Vec::new(),
                offered_scopes: vec![GrantScope::Once, GrantScope::Session],
                grant_preview: None,
            },
        }
    }
}

/// Answers every prompt the same way, and counts how often it was asked.
pub struct Answers {
    decision: InteractionDecision,
    asked: Mutex<u32>,
}

impl Answers {
    pub fn new(decision: InteractionDecision) -> Arc<Self> {
        Arc::new(Self {
            decision,
            asked: Mutex::new(0),
        })
    }

    pub fn count(&self) -> u32 {
        *self.asked.lock().unwrap()
    }
}

#[async_trait]
impl InteractionPort for Answers {
    async fn ask(&self, _req: InteractionRequest) -> Result<InteractionDecision, String> {
        *self.asked.lock().unwrap() += 1;
        Ok(self.decision.clone())
    }
}

// ─────────────────────────── models ───────────────────────────

pub fn model() -> ResolvedModel {
    ResolvedModel {
        source: Source::new("mock", "m1"),
        wire_model: "m1".into(),
        display_name: "Mock".into(),
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
        pricing: Some(Pricing {
            input_per_m: 3.0,
            cached_input_per_m: None,
            cache_write_per_m: None,
            output_per_m: 15.0,
            currency: "USD".into(),
        }),
        default_params: Default::default(),
        config_revision: 1,
    }
}

/// A distinct model, for the raw-gate and aux-attribution cases.
pub fn other_model(model_id: &str) -> ResolvedModel {
    ResolvedModel {
        source: Source::new("mock", model_id),
        wire_model: model_id.into(),
        display_name: model_id.into(),
        ..model()
    }
}

pub fn aux(model: ResolvedModel, client: Arc<dyn LlmClient>) -> AuxModel {
    AuxModel { model, client }
}

// ─────────────────────────── scripts ───────────────────────────

pub fn user(text: &str) -> Vec<ContentPart> {
    vec![ContentPart::Text(TextPart {
        text: text.into(),
        raw: None,
        truncated: false,
    })]
}

/// A response that calls one tool.
pub fn call(name: &str, args: &str) -> MockScript {
    MockScript {
        tool_calls: vec![(0, "call_1".into(), name.into(), args.into())],
        finish: Some(FinishReason::ToolCalls),
        ..Default::default()
    }
}

/// A response that calls one tool, with a call id of your choosing (several rounds in one turn
/// must not reuse one, or the ids collide in the history).
pub fn call_id(id: &str, name: &str, args: &str) -> MockScript {
    MockScript {
        tool_calls: vec![(0, id.into(), name.into(), args.into())],
        finish: Some(FinishReason::ToolCalls),
        ..Default::default()
    }
}

pub fn spawn_call(agent: &str, task: &str) -> MockScript {
    call(
        "create_agent",
        &serde_json::json!({ "agent": agent, "task": task }).to_string(),
    )
}

/// A plain answer that also reports usage — the only way to move the compaction trigger.
pub fn answer_using(text: &str, input_tokens: u64) -> MockScript {
    MockScript {
        text: Some(text.into()),
        usage: Some(zlogic_protocol::usage::TokenUsage {
            input: input_tokens,
            output: 10,
            ..Default::default()
        }),
        ..Default::default()
    }
}

// ─────────────────────────── tools ───────────────────────────

/// A deferred tool for the tests that exercise `load_tool`.
/// The heavy tools that make deferral worth having live outside this repository, so the mechanism
/// is covered here with a test-only stand-in rather than a real one.
struct Palette;

#[async_trait]
impl Tool for Palette {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "palette".into(),
            source: "test",
            risk: ToolRisk::Read,
        }
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Deferred
    }

    fn definition(&self) -> zlogic_protocol::llm::ToolDefinition {
        zlogic_protocol::llm::ToolDefinition {
            name: "palette".into(),
            description: "Use palette to look up the colours of a named palette.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "name": { "type": "string" } }
            }),
        }
    }

    async fn execute(&self, _ctx: &ToolCtx, _args: &str) -> zlogic_tools::Result<ToolExecResult> {
        Ok(ToolExecResult::success("palette"))
    }
}

// ─────────────────────────── harness ───────────────────────────

pub struct Harness {
    pub store: SharedStore,
    pub objects: Arc<dyn ObjectStore>,
    pub sink: Arc<RecordingSink>,
    pub session: SessionId,
    pub dir: tempfile::TempDir,
    /// The conversation's cancellation token — one for the whole harness, exactly as the CLI or the
    /// UI would own one per conversation.
    pub cancel: CancellationToken,
    policy: Arc<dyn PolicyGate>,
    interaction: Option<Arc<dyn InteractionPort>>,
    tasks: Option<Arc<dyn TaskHost>>,
    tools: ToolRegistry,
    limits: Limits,
    context: ContextPolicy,
}

impl Harness {
    pub fn new() -> Self {
        let db = Db::open_in_memory().unwrap();
        let session = db
            .sessions()
            .create(NewSession::root(WorkspaceId::new()))
            .unwrap()
            .session_id;
        let mut tools = ToolRegistry::with_builtins();
        tools.add(Arc::new(Palette));
        Self {
            store: SharedStore::new(db),
            objects: Arc::new(MemoryObjectStore::new()),
            sink: Arc::new(RecordingSink::default()),
            session,
            dir: tempfile::tempdir().unwrap(),
            cancel: CancellationToken::new(),
            policy: Arc::new(AllowAll),
            interaction: None,
            tasks: None,
            tools,
            limits: Limits::default(),
            context: ContextPolicy::default(),
        }
    }

    pub fn policy(mut self, policy: Arc<dyn PolicyGate>) -> Self {
        self.policy = policy;
        self
    }

    pub fn interaction(mut self, port: Arc<dyn InteractionPort>) -> Self {
        self.interaction = Some(port);
        self
    }

    pub fn tasks(mut self, tasks: Arc<dyn TaskHost>) -> Self {
        self.tasks = Some(tasks);
        self
    }

    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    pub fn context(mut self, context: ContextPolicy) -> Self {
        self.context = context;
        self
    }

    pub fn tool(mut self, tool: Arc<dyn zlogic_tools::Tool>) -> Self {
        self.tools.add(tool);
        self
    }

    pub fn services(&self) -> Arc<CoreServices> {
        Arc::new(CoreServices {
            store: self.store.clone(),
            objects: self.objects.clone(),
            attachment_dir: self.dir.path().join("attachments"),
            tools: self.tools.clone(),
            policy: self.policy.clone(),
            interaction: self.interaction.clone(),
            tasks: self.tasks.clone(),
            runtime_paths: None,
            model_resolver: None,
            limits: self.limits.clone(),
            context: self.context.clone(),
        })
    }

    pub fn core(&self) -> Core {
        Core::new(
            self.services(),
            self.session,
            self.dir.path(),
            self.sink.clone() as Arc<dyn EventSink>,
        )
    }

    pub fn core_with_spawner(&self, spawner: Arc<dyn AgentSpawner>) -> Core {
        self.core().with_spawner(Some(spawner))
    }

    /// The token to hand `Core::run`.
    pub fn token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Stops the conversation, as pressing Esc would.
    pub fn stop(&self) {
        self.cancel.cancel();
    }

    pub fn plan(&self, client: Arc<dyn LlmClient>) -> TurnPlan {
        TurnPlan::new(model(), client).with_system(vec!["you are a test".into()])
    }

    // ── inspection ──

    /// Payload type names in order — the cheapest way to assert an event sequence.
    pub fn event_names(&self) -> Vec<String> {
        self.sink
            .payloads()
            .iter()
            .map(|p| {
                serde_json::to_value(p).unwrap()["type"]
                    .as_str()
                    .unwrap_or("?")
                    .to_string()
            })
            .collect()
    }

    pub fn saw(&self, event: &str) -> bool {
        self.event_names().iter().any(|n| n == event)
    }

    pub fn entries(&self) -> Vec<EntryRecord> {
        self.store
            .with(|db| db.entries().list(self.session).unwrap())
    }

    pub fn entries_of(&self, session: SessionId) -> Vec<EntryRecord> {
        self.store.with(|db| db.entries().list(session).unwrap())
    }

    pub fn kinds(&self) -> Vec<EntryKind> {
        self.entries().iter().map(|e| e.kind).collect()
    }

    /// `(is_error, content)` per tool result, in order.
    pub fn tool_results(&self) -> Vec<(bool, String)> {
        self.entries()
            .iter()
            .filter(|e| e.kind == EntryKind::ToolResult)
            .map(
                |e| match serde_json::from_value::<ContentPart>(e.data.clone()).unwrap() {
                    ContentPart::ToolResult(r) => (r.is_error, r.content),
                    other => panic!("{other:?}"),
                },
            )
            .collect()
    }

    /// Text of the payloads matching a predicate — used to read notices.
    pub fn notices(&self) -> Vec<(String, String)> {
        self.sink
            .payloads()
            .into_iter()
            .filter_map(|p| match p {
                StreamPayload::Notice { code, message, .. } => Some((code, message.fallback)),
                _ => None,
            })
            .collect()
    }
}

/// Asserts the one shape several providers reject outright.
/// Worth checking on every request a test inspects: summaries, steering injections and ordinary
/// input all produce user messages, and it is the *combination* that could put two next to each
/// other.
pub fn assert_no_consecutive_user(messages: &[zlogic_protocol::message::Message]) {
    use zlogic_protocol::message::Role;
    for pair in messages.windows(2) {
        assert!(
            !(pair[0].role == Role::User && pair[1].role == Role::User),
            "two consecutive user messages: {:#?}",
            pair
        );
    }
}

impl Default for Harness {
    fn default() -> Self {
        Self::new()
    }
}
