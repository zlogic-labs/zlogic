use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use zlogic_config::{AppConfig, ConfigFile, Dirs};
use zlogic_core::{
    ContextPolicy, CoreServices, Limits, PolicyDecision, PolicyGate, PolicyRequest, SharedStore,
};
use zlogic_engine::dispatch::RegisteredRoots;
use zlogic_engine::hub::EventHub;
use zlogic_engine::{
    CredentialStore, Dispatcher, EngineInteractions, Extensions, ModelRouter, SessionLocks,
};
use zlogic_llm::transport::{ByteStream, HttpRequest, HttpTransport};
use zlogic_objects::MemoryObjectStore;
use zlogic_protocol::stream::TurnStatus;
use zlogic_protocol::{SessionId, WorkspaceId};
use zlogic_store::{Db, NewSession};
use zlogic_tools::ToolRegistry;

struct RecordingTransport {
    bodies: Mutex<Vec<String>>,
    reply: String,
}

impl RecordingTransport {
    fn new(reply: &str) -> Arc<Self> {
        Arc::new(Self {
            bodies: Mutex::new(Vec::new()),
            reply: sse_reply(reply),
        })
    }

    fn requests(&self) -> Vec<String> {
        self.bodies.lock().unwrap().clone()
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
        let body = self.reply.clone();
        Ok(Box::pin(futures_util::stream::once(async move {
            Ok(bytes::Bytes::from(body.into_bytes()))
        })))
    }
}

fn sse_reply(text: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":{}}}}}]}}\n\n\
         data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}],\
         \"usage\":{{\"prompt_tokens\":1,\"completion_tokens\":1}}}}\n\n\
         data: [DONE]\n\n",
        serde_json::json!(text)
    )
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

fn enqueue(store: &SharedStore, session: SessionId, text: &str) {
    store
        .with(|db| {
            db.mailbox().submit(
                session,
                &format!("r-{text}"),
                &serde_json::json!([{ "type": "text", "text": text }]),
                zlogic_store::Delivery::Queue,
            )
        })
        .unwrap();
}

struct Rig {
    dispatcher: Arc<Dispatcher>,
    store: SharedStore,
    session: SessionId,
    transport: Arc<RecordingTransport>,
    extensions: Arc<Extensions>,
    workspace: WorkspaceId,
    workspace_root: std::path::PathBuf,
    dirs: Dirs,
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
        let transport = RecordingTransport::new("ok");
        let router = Arc::new(ModelRouter::new(
            Arc::new(cfg),
            transport.clone(),
            Arc::new(NoKeys),
        ));

        let services = Arc::new(CoreServices {
            store: store.clone(),
            objects: Arc::new(MemoryObjectStore::new()),
            attachment_dir: dirs.data.join("attachments"),
            tools: ToolRegistry::with_builtins(),
            policy: Arc::new(AllowAll),
            interaction: Some(interactions.clone()),
            tasks: None,
            runtime_paths: None,
            model_resolver: None,
            limits: Limits::default(),
            context: ContextPolicy::default(),
        });

        let extensions = Extensions::new(&dirs);
        let dispatcher = Arc::new(
            Dispatcher::new(
                store.clone(),
                hub.clone(),
                router,
                Arc::new(SessionLocks::new(store.clone(), "test")),
                services,
                Arc::new(RegisteredRoots::with(workspace, &workspace_root)),
                interactions.clone(),
            )
            .with_extensions(extensions.clone()),
        );

        Self {
            dispatcher,
            store,
            session,
            transport,
            extensions,
            workspace,
            workspace_root,
            dirs,
            _dir: dir,
        }
    }

    fn define(&self, path: &Path, id: &str, tools: &[&str]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body = serde_json::json!({
            "mcpServers": { id: { "command": "zlogic-mcp-never-started", "cwd": "/fixed" } }
        });
        std::fs::write(path, serde_json::to_string_pretty(&body).unwrap()).unwrap();

        let parsed = zlogic_mcp::def::parse_file(path, zlogic_mcp::Origin::Global).unwrap();
        let def = parsed
            .servers
            .iter()
            .find(|s| s.id == id)
            .expect("the definition just written cannot be read back");

        let specs: Vec<zlogic_mcp::ToolSpec> = tools
            .iter()
            .map(|name| zlogic_mcp::ToolSpec {
                name: (*name).to_string(),
                description: Some(format!("the {name} tool")),
                input_schema: serde_json::json!({ "type": "object", "properties": {} }),
                hints: zlogic_mcp::Hints::default(),
            })
            .collect();
        zlogic_mcp::write_tool_cache(
            &zlogic_mcp::CatalogDirs::under(&self.dirs),
            def,
            &self.workspace_root,
            &specs,
            chrono::Utc::now(),
        )
        .unwrap();
    }

    async fn turn(&self, text: &str) {
        enqueue(&self.store, self.session, text);
        let turn = self
            .dispatcher
            .start_if_idle(self.session)
            .unwrap()
            .expect("the turn should start");
        for _ in 0..600 {
            if let Some(live) = self.dispatcher.registry().get(turn)
                && live.status.is_some()
            {
                assert_ne!(
                    live.status,
                    Some(TurnStatus::Failed),
                    "the turn must not fail"
                );
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("the turn did not finish within 12 seconds");
    }

    fn last_request(&self) -> String {
        self.transport
            .requests()
            .last()
            .cloned()
            .expect("not a single request went out")
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_configured_server_reaches_the_model_as_a_tool() {
    let rig = Rig::new();
    rig.define(
        &rig.dirs.data.join("extensions/mcp/notes.json"),
        "notes",
        &["append", "search"],
    );

    rig.turn("hello").await;

    let body = rig.last_request();
    assert!(
        body.contains("mcp__notes__append"),
        "the MCP tool did not make it into the wire: {body}"
    );
    assert!(body.contains("mcp__notes__search"), "{body}");
    assert!(body.contains("read_file"), "{body}");
    assert!(
        body.contains("the append tool"),
        "the description should travel too: {body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn disabling_it_takes_it_out_of_the_next_request() {
    let rig = Rig::new();
    rig.define(
        &rig.dirs.data.join("extensions/mcp/notes.json"),
        "notes",
        &["append"],
    );

    rig.turn("first round").await;
    assert!(rig.last_request().contains("mcp__notes__append"));

    zlogic_engine::extensions::state::set_personal(
        &rig.dirs,
        rig.workspace,
        zlogic_engine::extensions::Kind::Mcp,
        "notes",
        Some(false),
    )
    .unwrap();
    rig.extensions.after_extension_change("notes");

    rig.turn("second round").await;
    assert!(
        !rig.last_request().contains("mcp__notes__append"),
        "the server that was switched off is still in the request: {}",
        rig.last_request()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repository_definition_is_withheld_until_it_is_confirmed() {
    let rig = Rig::new();
    rig.define(&rig.workspace_root.join(".mcp.json"), "repo", &["danger"]);

    rig.turn("first round").await;
    let body = rig.last_request();
    assert!(
        !body.contains("mcp__repo__danger"),
        "an unconfirmed definition made it into the wire: {body}"
    );

    let def = rig
        .extensions
        .catalog()
        .load(&rig.workspace_root, Vec::new())
        .get("repo")
        .expect("the definition just written cannot be read back")
        .clone();
    zlogic_engine::extensions::state::set_trusted(
        &rig.dirs,
        rig.workspace,
        "repo",
        Some(&def.fingerprint()),
    )
    .unwrap();

    rig.turn("second round").await;
    assert!(
        rig.last_request().contains("mcp__repo__danger"),
        "it was confirmed and it still did not go in: {}",
        rig.last_request()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deployment_without_extensions_still_runs_turns() {
    let rig = Rig::new();
    rig.define(
        &rig.dirs.data.join("extensions/mcp/notes.json"),
        "notes",
        &["append"],
    );
    let dispatcher = Arc::new(Dispatcher::new(
        rig.store.clone(),
        Arc::new(EventHub::new()),
        Arc::new(ModelRouter::new(
            {
                let file: ConfigFile = serde_yaml_ng::from_str(CONFIG).unwrap();
                let mut cfg = AppConfig {
                    revision: 1,
                    ..Default::default()
                };
                cfg.apply(file);
                Arc::new(cfg)
            },
            rig.transport.clone(),
            Arc::new(NoKeys),
        )),
        Arc::new(SessionLocks::new(rig.store.clone(), "test")),
        Arc::new(CoreServices {
            store: rig.store.clone(),
            objects: Arc::new(MemoryObjectStore::new()),
            attachment_dir: rig.workspace_root.join(".test-attachments"),
            tools: ToolRegistry::with_builtins(),
            policy: Arc::new(AllowAll),
            interaction: None,
            tasks: None,
            runtime_paths: None,
            model_resolver: None,
            limits: Limits::default(),
            context: ContextPolicy::default(),
        }),
        Arc::new(RegisteredRoots::with(rig.workspace, &rig.workspace_root)),
        Arc::new(EngineInteractions::new(Arc::new(EventHub::new()))),
    ));

    enqueue(&rig.store, rig.session, "the round with no extensions");
    let turn = dispatcher
        .start_if_idle(rig.session)
        .unwrap()
        .expect("the turn should start");
    for _ in 0..600 {
        if let Some(live) = dispatcher.registry().get(turn)
            && live.status.is_some()
        {
            assert_ne!(live.status, Some(TurnStatus::Failed));
            let body = rig.last_request();
            assert!(
                body.contains("read_file"),
                "the built-in tools are there as usual: {body}"
            );
            assert!(
                !body.contains("mcp__notes__"),
                "with no extensions wired up there must be no MCP tools: {body}"
            );
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("the turn did not finish");
}
