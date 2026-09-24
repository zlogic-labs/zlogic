//! Signing a subscription provider in through the engine, and what the account may then call.
//!
//! The issuer and the model backend here are local stand-ins, because what is being asserted is
//! the engine's half: which providers may sign in, where the fetched model list ends up, and what
//! signing out does and does not touch.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use zlogic_config::{AppConfig, Dirs};
use zlogic_core::SharedStore;
use zlogic_credential::CredentialError;
use zlogic_engine::provider_auth::oauth_entry;
use zlogic_engine::service::{CredentialService, WorkspaceService};
use zlogic_engine::{Config, CredentialStore, Credentials, Workspaces};
use zlogic_llm::transport::{HttpTransport, ReplayTransport};
use zlogic_protocol::query::{
    CredentialDeleteReq, CredentialSetReq, ProviderModelsReq, ProviderSignInBeginReq,
    ProviderSignInMethod, ProviderSignInState, ProviderSignInStatusReq,
};

#[derive(Default)]
struct MemoryKeys(Mutex<HashMap<String, String>>);

impl CredentialStore for MemoryKeys {
    fn resolve(&self, credential_ref: &str) -> Option<String> {
        self.0.lock().unwrap().get(credential_ref).cloned()
    }

    fn set_keyring(&self, entry: &str, secret: &str) -> Result<(), CredentialError> {
        self.0
            .lock()
            .unwrap()
            .insert(format!("keyring:{entry}"), secret.to_string());
        Ok(())
    }

    fn delete_keyring(&self, entry: &str) -> Result<(), CredentialError> {
        self.0.lock().unwrap().remove(&format!("keyring:{entry}"));
        Ok(())
    }
}

struct Backend {
    base: String,
}

impl Backend {
    async fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(serve(stream));
            }
        });
        Self {
            base: format!("http://127.0.0.1:{port}"),
        }
    }
}

async fn serve(mut stream: TcpStream) {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut head_end = None;
    loop {
        let read = stream.read(&mut chunk).await.unwrap_or(0);
        if read == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..read]);
        if head_end.is_none()
            && let Some(end) = find(&buf, b"\r\n\r\n")
        {
            head_end = Some(end);
        }
        if let Some(end) = head_end {
            let head = String::from_utf8_lossy(&buf[..end]).to_string();
            if buf.len() >= end + 4 + content_length(&head) {
                break;
            }
        }
    }
    let request = String::from_utf8_lossy(&buf).to_string();
    let head = request.split("\r\n\r\n").next().unwrap_or_default();
    let target = head.split_whitespace().nth(1).unwrap_or("/").to_string();
    let path = target.split('?').next().unwrap_or("/").to_string();

    let body = match path.as_str() {
        "/oauth/token" => {
            r#"{"access_token":"access-1","refresh_token":"refresh-1","expires_in":3600,"id_token":"header.eyJjaGF0Z3B0X2FjY291bnRfaWQiOiJhY2N0LTEiLCJlbWFpbCI6Im1lQGV4YW1wbGUuY29tIn0.signature"}"#
        }
        "/api/accounts/deviceauth/usercode" => {
            r#"{"device_auth_id":"device-1","user_code":"ABCD-1234","interval":1}"#
        }
        "/api/accounts/deviceauth/token" => {
            r#"{"authorization_code":"code-1","code_verifier":"verifier-1"}"#
        }
        "/backend/models" => {
            r#"{"models":[
                {"slug":"gpt-test-max","display_name":"GPT Test Max","context_window":400000,"priority":1,
                 "supported_reasoning_levels":[{"effort":"low"},{"effort":"high"}],"supports_image_detail_original":true},
                {"slug":"gpt-test-mini","display_name":"GPT Test Mini","context_window":128000,"priority":2},
                {"slug":"gpt-test-hidden","display_name":"Hidden","visibility":"hide","priority":3}
            ]}"#
        }
        other => {
            let body = format!(r#"{{"error":"no route for {other}"}}"#);
            let _ = stream.write_all(response(404, &body).as_bytes()).await;
            let _ = stream.flush().await;
            return;
        }
    };
    let _ = stream.write_all(response(200, body).as_bytes()).await;
    let _ = stream.flush().await;
}

fn response(code: u16, body: &str) -> String {
    format!(
        "HTTP/1.1 {code} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn content_length(head: &str) -> usize {
    head.lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0)
}

struct Rig {
    credentials: Credentials,
    config: Arc<Config>,
    keys: Arc<MemoryKeys>,
    dirs: Dirs,
    _home: tempfile::TempDir,
}

impl Rig {
    /// An engine whose subscription backend is `backend`, so nothing reaches the real issuer.
    fn new(backend: &Backend) -> Self {
        let home = tempfile::tempdir().unwrap();
        let dirs = Dirs {
            config: home.path().join("config"),
            data: home.path().join("data"),
            state: home.path().join("state"),
            cache: home.path().join("cache"),
        };
        std::fs::create_dir_all(&dirs.config).unwrap();
        std::fs::create_dir_all(&dirs.data).unwrap();

        let keys = Arc::new(MemoryKeys::default());
        let config =
            Arc::new(AppConfig::load(&dirs, |reference| keys.is_available(reference)).unwrap());
        let db = SharedStore::new(zlogic_store::Db::open_in_memory().unwrap());
        let workspaces: Arc<dyn WorkspaceService> = Arc::new(Workspaces::new(db.clone()));
        let config = Arc::new(Config::new(
            config,
            dirs.clone(),
            keys.clone(),
            db,
            workspaces,
        ));

        let codex = Arc::new(zlogic_codex::Codex::with_client(
            zlogic_codex::Config {
                issuer: backend.base.clone(),
                backend: format!("{}/backend", backend.base),
                client_id: "client-1".into(),
                originator: "zlogic".into(),
                client_version: "zlogic/0.0.0-test".into(),
            },
            reqwest::Client::builder().no_proxy().build().unwrap(),
        ));
        let transport: Arc<dyn HttpTransport> = Arc::new(ReplayTransport::whole(""));

        Self {
            credentials: Credentials::with_codex(
                Arc::clone(&config),
                keys.clone(),
                transport,
                codex,
            ),
            config,
            keys,
            dirs,
            _home: home,
        }
    }

    /// The model ids the configuration currently offers for a provider.
    async fn models_of(&self, provider_id: &str) -> Vec<String> {
        self.config
            .snapshot()
            .await
            .providers
            .get(provider_id)
            .map(|provider| provider.models.keys().cloned().collect())
            .unwrap_or_default()
    }
}

async fn sign_in(credentials: &Credentials) -> ProviderSignInState {
    let begin = credentials
        .sign_in_begin(ProviderSignInBeginReq {
            provider_id: "codex".into(),
            method: ProviderSignInMethod::Device,
        })
        .await
        .unwrap();
    assert_eq!(begin.user_code.as_deref(), Some("ABCD-1234"));

    for _ in 0..200 {
        let status = credentials
            .sign_in_status(ProviderSignInStatusReq {
                flow_id: begin.flow_id.clone(),
            })
            .await
            .unwrap();
        if status.state != ProviderSignInState::Pending {
            return status.state;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the sign-in never finished");
}

#[tokio::test]
async fn a_subscription_provider_is_signed_in_rather_than_keyed() {
    let backend = Backend::start().await;
    let rig = Rig::new(&backend);

    let state = rig
        .credentials
        .list()
        .await
        .unwrap()
        .into_iter()
        .find(|state| state.provider_id == "codex")
        .expect("the subscription provider is always listed, signed in or not");
    assert!(!state.present);
    assert_eq!(
        state.candidates,
        vec![format!("keyring:{}", oauth_entry("codex"))],
        "a subscription has exactly one home"
    );

    let refusal = rig
        .credentials
        .set(CredentialSetReq {
            provider_id: "codex".into(),
            value: "sk-not-a-subscription".into(),
        })
        .await
        .unwrap_err();
    assert!(
        refusal.message.fallback.contains("zlogic auth login codex"),
        "{}",
        refusal.message.fallback
    );
}

#[tokio::test]
async fn signing_in_stores_one_credential_and_reports_the_account() {
    let backend = Backend::start().await;
    let rig = Rig::new(&backend);

    let state = sign_in(&rig.credentials).await;
    let ProviderSignInState::Succeeded { label, .. } = state else {
        panic!("{state:?}");
    };
    assert_eq!(
        label.as_deref(),
        Some("me@example.com"),
        "the label comes out of the id token, not out of the access token"
    );

    let listed = rig
        .credentials
        .list()
        .await
        .unwrap()
        .into_iter()
        .find(|state| state.provider_id == "codex")
        .unwrap();
    assert!(listed.present);
    assert_eq!(listed.hint.as_deref(), Some("me@example.com"));
    assert!(
        !listed
            .hint
            .as_deref()
            .unwrap_or_default()
            .contains("access-1"),
        "the hint must not leak the token"
    );
    assert_eq!(
        rig.keys.0.lock().unwrap().keys().collect::<Vec<_>>(),
        ["keyring:codex_oauth"],
        "signing in writes the token and nothing else"
    );
}

#[tokio::test]
async fn the_backend_decides_which_models_the_account_may_call() {
    let backend = Backend::start().await;
    let rig = Rig::new(&backend);
    sign_in(&rig.credentials).await;

    let floor = rig.models_of("codex").await;
    assert!(
        floor.contains(&"gpt-5.5".to_string()),
        "before any fetch the built-in floor applies: {floor:?}"
    );

    let listed = rig
        .credentials
        .models(ProviderModelsReq {
            provider_id: "codex".into(),
        })
        .await
        .unwrap();
    let ids: Vec<&str> = listed
        .models
        .iter()
        .map(|model| model.model_id.as_str())
        .collect();
    assert_eq!(
        ids,
        ["gpt-test-max", "gpt-test-mini"],
        "hidden models are not offered, and the backend's order is kept"
    );
    let max = &listed.models[0];
    assert_eq!(max.display_name.as_deref(), Some("GPT Test Max"));
    assert_eq!(max.context_window, 400_000);
    assert!(max.capabilities.thinking.supported);

    assert_eq!(
        rig.models_of("codex").await,
        ids,
        "the answer is applied to the provider, so routing and the pickers see it"
    );
    let snapshot = zlogic_config::ProviderModelsFile::read(&rig.dirs)
        .expect("the answer is kept in the data directory");
    assert_eq!(snapshot.provider_id, "codex");
    assert_eq!(snapshot.models.keys().cloned().collect::<Vec<_>>(), ids);

    rig.credentials
        .forget_models(ProviderModelsReq {
            provider_id: "codex".into(),
        })
        .await
        .unwrap();
    assert!(
        zlogic_config::ProviderModelsFile::read(&rig.dirs).is_none(),
        "forgetting removes the snapshot rather than blanking it"
    );
    assert!(
        rig.models_of("codex")
            .await
            .contains(&"gpt-5.5".to_string()),
        "and the built-in floor applies again"
    );
}

#[tokio::test]
async fn signing_out_drops_the_token_and_what_it_taught_us() {
    let backend = Backend::start().await;
    let rig = Rig::new(&backend);
    sign_in(&rig.credentials).await;
    rig.credentials
        .models(ProviderModelsReq {
            provider_id: "codex".into(),
        })
        .await
        .unwrap();

    let after = rig
        .credentials
        .delete(CredentialDeleteReq {
            provider_id: "codex".into(),
        })
        .await
        .unwrap();
    assert!(!after.present);
    assert!(rig.keys.0.lock().unwrap().is_empty());
    assert!(
        zlogic_config::ProviderModelsFile::read(&rig.dirs).is_none(),
        "a model list fetched with a token that is gone must not survive it"
    );
    assert!(
        rig.models_of("codex").await.is_empty(),
        "with no credential the subscription provider is not part of the configuration at all"
    );
}

/// A token can disappear from the keychain without the configuration hearing about it — removed in
/// another window, or by the OS. The client builder is the last gate before a request, and it has
/// to refuse rather than send an unauthenticated turn.
#[tokio::test]
async fn a_subscription_that_vanishes_after_signing_in_is_refused_at_the_request() {
    let backend = Backend::start().await;
    let rig = Rig::new(&backend);
    sign_in(&rig.credentials).await;
    assert!(!rig.models_of("codex").await.is_empty());

    rig.keys.0.lock().unwrap().clear();

    let error = rig
        .credentials
        .verify(zlogic_protocol::query::CredentialVerifyReq {
            provider_id: "codex".into(),
            model_id: "gpt-5.5".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(error.code, "provider_not_signed_in");
    assert_eq!(
        error.details.get("provider").and_then(|v| v.as_str()),
        Some("codex")
    );
}

#[tokio::test]
async fn a_provider_that_does_not_sign_in_is_refused_outright() {
    let backend = Backend::start().await;
    let rig = Rig::new(&backend);

    for provider_id in ["openai", "not-a-provider"] {
        let error = rig
            .credentials
            .sign_in_begin(ProviderSignInBeginReq {
                provider_id: provider_id.into(),
                method: ProviderSignInMethod::Device,
            })
            .await
            .unwrap_err();
        assert!(
            matches!(
                error.code.as_str(),
                "provider_not_subscription" | "provider_sign_in_failed"
            ),
            "{provider_id}: {error:?}"
        );
    }
}
