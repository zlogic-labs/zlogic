use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use zlogic_config::{AppConfig, ConfigFile};
use zlogic_core::{
    ContextPolicy, CoreServices, Limits, PolicyDecision, PolicyGate, PolicyRequest, SharedStore,
};
use zlogic_engine::dispatch::RegisteredRoots;
use zlogic_engine::hub::EventHub;
use zlogic_engine::service::TurnService;
use zlogic_engine::{
    CredentialStore, Dispatcher, EngineInteractions, ModelRouter, SessionLocks, Worktrees,
};
use zlogic_llm::transport::{ByteStream, HttpRequest, HttpTransport};
use zlogic_objects::MemoryObjectStore;
use zlogic_protocol::input::{Delivery, MessagePart};
use zlogic_protocol::{SessionId, Submission, WorkspaceId};
use zlogic_store::{Db, NewSession};
use zlogic_tools::ToolRegistry;

struct ScriptedTransport {
    bodies: Mutex<std::collections::VecDeque<String>>,
    last: Mutex<String>,
}

impl ScriptedTransport {
    fn new(bodies: Vec<String>) -> Arc<Self> {
        let last = bodies.last().cloned().unwrap_or_default();
        Arc::new(Self {
            bodies: Mutex::new(bodies.into()),
            last: Mutex::new(last),
        })
    }
}

#[async_trait]
impl HttpTransport for ScriptedTransport {
    async fn post_stream(
        &self,
        _req: HttpRequest,
    ) -> Result<ByteStream, zlogic_protocol::llm::LlmError> {
        let body = self
            .bodies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| self.last.lock().unwrap().clone());
        Ok(Box::pin(futures_util::stream::once(async move {
            Ok(bytes::Bytes::from(body.into_bytes()))
        })))
    }
}

fn sse_reply(text: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":{}}}}}]}}\n\n\
         data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}],\
         \"usage\":{{\"prompt_tokens\":10,\"completion_tokens\":5}}}}\n\n\
         data: [DONE]\n\n",
        serde_json::to_string(text).unwrap()
    )
}

fn sse_tool_call(call_id: &str, name: &str, args: serde_json::Value) -> String {
    let args = serde_json::to_string(&args.to_string()).unwrap();
    format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\
         \"id\":\"{call_id}\",\"type\":\"function\",\"function\":{{\"name\":\"{name}\",\
         \"arguments\":{args}}}}}]}}}}]}}\n\n\
         data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n\
         data: [DONE]\n\n"
    )
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
worktree:
  dir: "../{workspace}-worktrees"
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
    root: std::path::PathBuf,
    _tmp: tempfile::TempDir,
}

impl Harness {
    fn new(bodies: Vec<String>) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(tmp.path()).unwrap();
        let root = base.join("repo");
        std::fs::create_dir_all(&root).unwrap();
        init_repo(&root);

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
        let worktree_dir = cfg.worktree.dir.clone();
        let router = Arc::new(ModelRouter::new(
            Arc::new(cfg),
            ScriptedTransport::new(bodies),
            Arc::new(NoKeys),
        ));

        let services = Arc::new(CoreServices {
            store: store.clone(),
            objects: Arc::new(MemoryObjectStore::new()),
            attachment_dir: root.join(".test-attachments"),
            tools: ToolRegistry::with_builtins(),
            policy: Arc::new(AllowAll),
            interaction: Some(interactions.clone()),
            tasks: None,
            runtime_paths: None,
            model_resolver: None,
            limits: Limits {
                max_rounds: 6,
                ..Default::default()
            },
            context: ContextPolicy::default(),
        });

        let dispatcher = Arc::new(
            Dispatcher::new(
                store.clone(),
                hub.clone(),
                router,
                Arc::new(SessionLocks::new(store.clone(), "test")),
                services,
                Arc::new(RegisteredRoots::with(workspace, &root)),
                interactions,
            )
            .with_worktrees(Arc::new(Worktrees::new(store.clone(), worktree_dir))),
        );

        Self {
            dispatcher,
            store,
            session,
            root,
            _tmp: tmp,
        }
    }

    fn submit(&self, text: &str) -> Submission {
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

    fn exec_cwd(&self) -> Option<String> {
        self.store
            .with(|db| db.sessions().get(self.session))
            .unwrap()
            .exec_cwd
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

fn init_repo(dir: &Path) {
    let repo = git2::Repository::init(dir).unwrap();
    std::fs::write(dir.join(".gitignore"), ".env\n").unwrap();
    std::fs::write(dir.join(".worktreeinclude"), ".env\n").unwrap();
    std::fs::write(dir.join(".env"), "TOKEN=1\n").unwrap();
    std::fs::write(dir.join("a.txt"), "x").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("a.txt")).unwrap();
    index.write().unwrap();
    let tree = index.write_tree().unwrap();
    let tree = repo.find_tree(tree).unwrap();
    let sig = git2::Signature::now("t", "t@test").unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
        .unwrap();
}

#[tokio::test]
async fn entering_moves_the_next_tool_call_and_exiting_moves_it_back() {
    let h = Harness::new(vec![
        sse_tool_call("c1", "enter_worktree", serde_json::json!({ "name": "e2e" })),
        sse_tool_call(
            "c2",
            "shell",
            serde_json::json!({ "command": "pwd > inside.txt" }),
        ),
        sse_tool_call(
            "c3",
            "exit_worktree",
            serde_json::json!({ "action": "keep" }),
        ),
        sse_tool_call(
            "c4",
            "shell",
            serde_json::json!({ "command": "pwd > outside.txt" }),
        ),
        sse_reply("done"),
    ]);

    h.dispatcher
        .submit(h.submit("do it inside the worktree"))
        .await
        .unwrap();
    h.settle().await;

    assert_eq!(
        h.exec_cwd(),
        None,
        "there should be no diversion left after exit"
    );

    let checkout = h.root.parent().unwrap().join("repo-worktrees").join("e2e");
    assert!(
        checkout.is_dir(),
        "the checkout was not created in the sibling directory: {}",
        checkout.display()
    );

    assert_eq!(
        std::fs::read_to_string(checkout.join(".env")).unwrap(),
        "TOKEN=1\n",
        "a file that is gitignored but selected by .worktreeinclude should come along"
    );

    let inside = checkout.join("inside.txt");
    assert!(
        inside.exists(),
        "a tool call after enter did not run inside the worktree: {}",
        inside.display()
    );
    assert!(
        std::fs::read_to_string(&inside)
            .unwrap()
            .trim()
            .ends_with("e2e"),
        "the directory pwd reports is not that checkout"
    );
    assert!(
        !h.root.join("inside.txt").exists(),
        "and it must certainly not land in the user's working directory"
    );

    assert!(
        h.root.join("outside.txt").exists(),
        "a tool call after exit should run back in the root"
    );
    assert!(!checkout.join("outside.txt").exists());

    let repo = git2::Repository::open(&h.root).unwrap();
    assert!(
        repo.find_branch("e2e", git2::BranchType::Local).is_ok(),
        "keep must not delete the branch"
    );
}

#[tokio::test]
async fn removing_unsaved_work_is_refused_and_the_session_stays_put() {
    let h = Harness::new(vec![
        sse_tool_call("c1", "enter_worktree", serde_json::json!({ "name": "wip" })),
        sse_tool_call(
            "c2",
            "shell",
            serde_json::json!({ "command": "zlogic dirty >> a.txt" }),
        ),
        sse_tool_call(
            "c3",
            "exit_worktree",
            serde_json::json!({ "action": "remove" }),
        ),
        sse_reply("let me ask the user first"),
    ]);

    h.dispatcher
        .submit(h.submit("change something in the worktree, then remove it"))
        .await
        .unwrap();
    h.settle().await;

    let cwd = h
        .exec_cwd()
        .expect("after being refused it should still be in the worktree");
    assert!(cwd.ends_with("wip"), "{cwd}");
    assert!(
        Path::new(&cwd).exists(),
        "not a single byte of the checkout should have been touched"
    );
    let repo = git2::Repository::open(&h.root).unwrap();
    assert!(
        repo.find_branch("wip", git2::BranchType::Local).is_ok(),
        "the branch is still there too"
    );
}
