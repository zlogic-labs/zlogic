//! The shell tool's time limits are configurable per workspace, and the only way that fails
//! quietly is for the override to never reach the tool the turn actually runs with. These run a
//! real turn — with no extensions, which is the case the base-registry fallback used to skip — and
//! read the budgets back out of the request the model was sent.

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use zlogic_config::{AppConfig, ConfigFile, Dirs};
use zlogic_core::{
    ContextPolicy, CoreServices, Limits, PolicyDecision, PolicyGate, PolicyRequest, SharedStore,
};
use zlogic_engine::dispatch::RegisteredRoots;
use zlogic_engine::hub::EventHub;
use zlogic_engine::{CredentialStore, Dispatcher, EngineInteractions, ModelRouter, SessionLocks};
use zlogic_llm::transport::{ByteStream, HttpRequest, HttpTransport};
use zlogic_objects::MemoryObjectStore;
use zlogic_protocol::stream::TurnStatus;
use zlogic_protocol::{SessionId, WorkspaceId};
use zlogic_store::{Db, Delivery, NewSession};
use zlogic_tools::{Shell, ToolRegistry};

struct RecordingTransport {
    bodies: Mutex<Vec<String>>,
}

impl RecordingTransport {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            bodies: Mutex::new(Vec::new()),
        })
    }

    fn last_request(&self) -> String {
        self.bodies
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("not a single request went out")
    }
}

#[async_trait]
impl HttpTransport for RecordingTransport {
    async fn post_stream(
        &self,
        req: HttpRequest,
    ) -> Result<ByteStream, zlogic_protocol::llm::LlmError> {
        self.bodies
            .lock()
            .unwrap()
            .push(String::from_utf8_lossy(&req.body).to_string());
        let body = sse_reply();
        Ok(Box::pin(futures_util::stream::once(async move {
            Ok(bytes::Bytes::from(body.into_bytes()))
        })))
    }
}

fn sse_reply() -> String {
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\
     \"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\
     \"completion_tokens\":1}}\n\ndata: [DONE]\n\n"
        .into()
}

struct NoKeys;
impl CredentialStore for NoKeys {
    fn resolve(&self, _credential_ref: &str) -> Option<String> {
        Some("test-key".into())
    }
}

struct AllowAll;
#[async_trait]
impl PolicyGate for AllowAll {
    async fn evaluate(&self, _r: &PolicyRequest) -> PolicyDecision {
        PolicyDecision::Allow
    }
}

const CONFIG: &str = r#"
default_model: p:m
providers:
  p:
    sdk: deepseek
    base_url: https://scripted.test
    models:
      m: { context_window: 64000 }
"#;

struct Rig {
    dispatcher: Arc<Dispatcher>,
    store: SharedStore,
    session: SessionId,
    transport: Arc<RecordingTransport>,
    workspace_root: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

impl Rig {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(dir.path().join("home"));
        let workspace_root = dir.path().join("work");
        std::fs::create_dir_all(&workspace_root).unwrap();

        let db = Db::open_in_memory().unwrap();
        let workspace = WorkspaceId::new();
        let session = db
            .sessions()
            .create(NewSession {
                model_ref: Some("p:m".into()),
                ..NewSession::root(workspace)
            })
            .unwrap()
            .session_id;
        let store = SharedStore::new(db);

        let hub = Arc::new(EventHub::new());
        let interactions = Arc::new(EngineInteractions::new(hub.clone()));

        let file: ConfigFile = serde_yaml_ng::from_str(CONFIG).unwrap();
        let mut cfg = AppConfig {
            revision: 1,
            ..Default::default()
        };
        cfg.apply(file);
        let transport = RecordingTransport::new();
        let router = Arc::new(ModelRouter::new(
            Arc::new(cfg),
            transport.clone(),
            Arc::new(NoKeys),
        ));

        // The two halves bootstrap hands the dispatcher: a registry holding the shell, and the
        // shell itself so a workspace can rebudget it without a second backend probe.
        let shell = Shell::default();
        let mut tools = ToolRegistry::with_builtins();
        tools.add(Arc::new(shell.clone()));

        let services = Arc::new(CoreServices {
            store: store.clone(),
            objects: Arc::new(MemoryObjectStore::new()),
            attachment_dir: dirs.data.join("attachments"),
            tools,
            policy: Arc::new(AllowAll),
            interaction: Some(interactions.clone()),
            tasks: None,
            runtime_paths: None,
            env: None,
            computer: None,
            model_resolver: None,
            limits: Limits::default(),
            context: ContextPolicy::default(),
        });

        let dispatcher = Arc::new(
            Dispatcher::new(
                store.clone(),
                hub,
                router,
                Arc::new(SessionLocks::new(store.clone(), "test")),
                services,
                Arc::new(RegisteredRoots::with(workspace, &workspace_root)),
                interactions,
            )
            .with_shell(shell),
        );

        Self {
            dispatcher,
            store,
            session,
            transport,
            workspace_root,
            _dir: dir,
        }
    }

    fn write_workspace_settings(&self, path: &Path, body: &str) {
        let path = self.workspace_root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    /// Runs one turn and returns the request the model was sent, tool definitions and all.
    async fn request(&self) -> String {
        self.store
            .with(|db| {
                db.mailbox().submit(
                    self.session,
                    "r-1",
                    &serde_json::json!([{ "type": "text", "text": "look around" }]),
                    Delivery::Queue,
                )
            })
            .unwrap();
        let turn = self
            .dispatcher
            .start_if_idle(self.session)
            .unwrap()
            .expect("the turn should start");
        for _ in 0..600 {
            if let Some(live) = self.dispatcher.registry().get(turn)
                && live.status.is_some()
            {
                assert_ne!(live.status, Some(TurnStatus::Failed), "the turn failed");
                return self.transport.last_request();
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("the turn did not finish within 12 seconds");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_workspace_can_raise_the_test_budget_the_model_is_told_about() {
    let rig = Rig::new();
    rig.write_workspace_settings(
        Path::new(".zlogic/settings.yaml"),
        "tools:\n  shell:\n    test_secs: 1800\n",
    );
    let body = rig.request().await;

    assert!(
        body.contains("30m 0s for a test run"),
        "the workspace's test budget is not in the definition the model was sent: {body}"
    );
    assert!(
        !body.contains("10m 0s for a test run"),
        "the global default survived the override: {body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_workspace_can_lower_a_budget_too() {
    let rig = Rig::new();
    rig.write_workspace_settings(
        Path::new(".zlogic/settings.yaml"),
        "tools:\n  shell:\n    quick_secs: 5\n",
    );
    let body = rig.request().await;
    assert!(
        body.contains("5s for a quick check"),
        "a lowered budget did not reach the definition: {body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_workspace_that_says_nothing_keeps_the_global_budgets() {
    let rig = Rig::new();
    let body = rig.request().await;
    assert!(
        body.contains("10m 0s for a test run"),
        "the shipped default is not what the model was told: {body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_settings_file_with_no_shell_keys_changes_nothing() {
    let rig = Rig::new();
    rig.write_workspace_settings(Path::new(".zlogic/settings.yaml"), "locale: zh-CN\n");
    let body = rig.request().await;
    assert!(
        body.contains("10m 0s for a test run"),
        "an unrelated settings file changed the shell budgets: {body}"
    );
}
