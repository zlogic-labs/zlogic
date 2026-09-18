use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use zlogic_config::{AppConfig, ConfigFile, Dirs};
use zlogic_core::{
    ContextPolicy, CoreServices, Limits, PolicyDecision, PolicyGate, PolicyRequest, SharedStore,
};
use zlogic_engine::dispatch::RegisteredRoots;
use zlogic_engine::hub::EventHub;
use zlogic_engine::service::TurnService;
use zlogic_engine::{
    CredentialStore, Dispatcher, EngineInteractions, ModelRouter, SessionLocks, SystemPrompts,
};
use zlogic_llm::transport::{ByteStream, HttpRequest, HttpTransport};
use zlogic_objects::MemoryObjectStore;
use zlogic_protocol::input::{Delivery, MessagePart};
use zlogic_protocol::{
    MemoryAddReq, MemoryCategory, MemoryScope, SessionId, Submission, TurnId, WorkspaceId,
};
use zlogic_store::{Db, NewSession};
use zlogic_tools::{ShellDialect, ToolRegistry};

#[derive(Default)]
struct RecordingTransport {
    bodies: Mutex<Vec<String>>,
}

impl RecordingTransport {
    fn first_body(&self) -> String {
        self.bodies
            .lock()
            .unwrap()
            .first()
            .cloned()
            .expect("a request has gone out")
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
            .push(String::from_utf8_lossy(&req.body).into_owned());
        let body = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"}}]}\n\n\
                    data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\
                    \"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2}}\n\n\
                    data: [DONE]\n\n";
        Ok(Box::pin(futures_util::stream::once(async move {
            Ok(bytes::Bytes::from(body.as_bytes().to_vec()))
        })))
    }
}

struct AllowAll;
#[async_trait]
impl PolicyGate for AllowAll {
    async fn evaluate(&self, _r: &PolicyRequest) -> PolicyDecision {
        PolicyDecision::Allow
    }
}

struct NoKeys;
impl CredentialStore for NoKeys {
    fn resolve(&self, _r: &str) -> Option<String> {
        Some("test-key".into())
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

struct Harness {
    dispatcher: Arc<Dispatcher>,
    store: SharedStore,
    session: SessionId,
    transport: Arc<RecordingTransport>,
    _tmp: tempfile::TempDir,
}

impl Harness {
    fn new() -> Self {
        Self::with_prompts(true)
    }

    fn with_prompts(prompts: bool) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(tmp.path()).unwrap();
        let dirs = Dirs::under(&base);
        std::fs::create_dir_all(&dirs.config).unwrap();
        std::fs::write(
            dirs.config.join("AGENTS.md"),
            "USER-RULE: always run cargo fmt\n",
        )
        .unwrap();

        let root = base.join("repo");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("AGENTS.md"),
            "PROJECT-RULE: comments in English\n",
        )
        .unwrap();
        let skill = root.join(".zlogic").join("skills").join("release");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: release\ndescription: cut a release\n---\nSECRET-BODY\n",
        )
        .unwrap();

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
        let config = Arc::new(cfg);

        let transport = Arc::new(RecordingTransport::default());
        let router = Arc::new(ModelRouter::new(
            config.clone(),
            transport.clone(),
            Arc::new(NoKeys),
        ));

        let services = Arc::new(CoreServices {
            store: store.clone(),
            objects: Arc::new(MemoryObjectStore::new()),
            attachment_dir: root.join("attachments"),
            tools: ToolRegistry::with_builtins(),
            policy: Arc::new(AllowAll),
            interaction: Some(interactions.clone()),
            tasks: None,
            runtime_paths: None,
            model_resolver: None,
            limits: Limits {
                max_rounds: 2,
                ..Default::default()
            },
            context: ContextPolicy::default(),
        });

        let mut dispatcher = Dispatcher::new(
            store.clone(),
            hub.clone(),
            router,
            Arc::new(SessionLocks::new(store.clone(), "test")),
            services,
            Arc::new(RegisteredRoots::with(workspace, &root)),
            interactions,
        );
        if prompts {
            dispatcher = dispatcher.with_prompts(Arc::new(SystemPrompts::new(
                dirs,
                config,
                Some(ShellDialect::Posix),
            )));
        }
        let dispatcher = Arc::new(dispatcher);

        Self {
            dispatcher,
            store,
            session,
            transport,
            _tmp: tmp,
        }
    }

    fn submission(&self, text: &str) -> Submission {
        Submission {
            submission_id: String::new(),
            session_id: self.session.to_string(),
            client_request_id: "req-1".into(),
            parts: vec![MessagePart::Text { text: text.into() }],
            delivery: Delivery::Queue,
            model_ref: None,
            thinking: None,
        }
    }

    async fn settle(&self) {
        for _ in 0..2_000 {
            if self
                .store
                .with(|db| db.locks().live_turn(self.session).unwrap())
                .is_none()
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("the turn did not finish in reasonable time");
    }
}

#[tokio::test]
async fn one_submit_puts_the_whole_system_prompt_on_the_wire() {
    let h = Harness::new();
    h.dispatcher.submit(h.submission("hi")).await.unwrap();
    h.settle().await;

    let body = h.transport.first_body();
    let sent: serde_json::Value = serde_json::from_str(&body).expect("the request body is JSON");
    let system = sent["messages"][0]["content"]
        .as_str()
        .expect("the first message is the system prompt");
    assert_eq!(sent["messages"][0]["role"], "system");

    assert!(
        system.contains("You are zlogic"),
        "the identity did not make it in: {system}"
    );
    assert!(
        system.contains("USER-RULE"),
        "the user-level AGENTS.md did not make it in"
    );
    assert!(
        system.contains("PROJECT-RULE"),
        "the project-level AGENTS.md did not make it in"
    );
    assert!(
        system.contains("release — cut a release"),
        "the skill catalogue did not make it in"
    );
    assert!(
        !system.contains("SECRET-BODY"),
        "a skill body must not go into the prompt — that is the whole point of progressive disclosure"
    );
    assert!(
        system.contains("<environment>"),
        "the environment block did not make it in"
    );
    assert!(
        !system.contains("\ndate:"),
        "a dynamic date would invalidate the system prompt cache: {system}"
    );
    assert!(
        system.contains("whenever the answer depends on now"),
        "with a time tool present, it should be used to get the current time: {system}"
    );
    assert!(
        !system.contains("get it with shell"),
        "with both time and shell present, the dedicated tool should win: {system}"
    );
    assert!(
        system.contains(std::env::consts::OS),
        "there is no operating system in the environment block"
    );

    let identity = system.find("You are zlogic").unwrap();
    let user = system.find("USER-RULE").unwrap();
    let project = system.find("PROJECT-RULE").unwrap();
    let env = system.find("<environment>").unwrap();
    assert!(
        identity < user && user < project && project < env,
        "the order is wrong: {system}"
    );

    assert!(
        system.contains("enter_worktree"),
        "when the tool exists it has to be described"
    );
    let names = zlogic_tools::ToolRegistry::with_builtins().names();
    if names.iter().any(|n| n == "create_agent" || n == "shell") {
        assert!(
            system.contains("background: true"),
            "with a tool that can start background work, how to start it has to be spelled out: {system}"
        );
    }
    if names.iter().any(|n| n.starts_with("task_")) {
        assert!(
            system.contains("notification"),
            "completion notifications have to be written into the system prompt: {system}"
        );
    }
    assert!(
        system.contains("does not typeset mathematics"),
        "a CLI host needs this line: {system}"
    );
}

#[tokio::test]
async fn memory_priority_reaches_the_real_model_request() {
    let h = Harness::new();
    let workspace = h
        .store
        .with(|db| db.sessions().get(h.session).unwrap().workspace_id);
    h.store.with(|db| {
        db.memories()
            .add(MemoryAddReq {
                scope: MemoryScope::Global,
                workspace_id: None,
                category: MemoryCategory::Preference,
                fact: "always answer in Chinese".into(),
                source_quote: "always answer in Chinese".into(),
                source_session_id: None,
                source_turn_id: None,
            })
            .unwrap();
        db.memories()
            .add(MemoryAddReq {
                scope: MemoryScope::Workspace,
                workspace_id: Some(workspace),
                category: MemoryCategory::Reference,
                fact: "the user may like Rust".into(),
                source_quote: "I like Rust".into(),
                source_session_id: Some(h.session),
                source_turn_id: Some(TurnId::new()),
            })
            .unwrap();
    });

    h.dispatcher.submit(h.submission("hi")).await.unwrap();
    h.settle().await;

    let sent: serde_json::Value =
        serde_json::from_str(&h.transport.first_body()).expect("the request body is JSON");
    let system = sent["messages"][0]["content"]
        .as_str()
        .expect("the first message is the system prompt");
    assert!(system.contains("MUST follow"), "{system}");
    assert!(system.contains("<high_priority_memory>"), "{system}");
    assert!(system.contains("always answer in Chinese"), "{system}");
    assert!(system.contains("<reference_memory>"), "{system}");
    assert!(system.contains("the user may like Rust"), "{system}");
}

#[tokio::test]
async fn without_the_assembler_no_zlogic_system_prompt_is_sent() {
    let h = Harness::with_prompts(false);
    h.dispatcher.submit(h.submission("hi")).await.unwrap();
    h.settle().await;

    let sent: serde_json::Value = serde_json::from_str(&h.transport.first_body()).unwrap();
    let body = h.transport.first_body();
    assert!(!body.contains("You are zlogic"));
    assert!(!body.contains("<environment>"));
    assert!(
        sent["messages"]
            .as_array()
            .is_some_and(|messages| messages.iter().any(|message| message["role"] == "user")),
        "the user message still has to be sent: {body}"
    );
}
