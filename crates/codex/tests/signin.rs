//! The two sign-ins and the token custody around them, against a local stand-in for
//! `auth.openai.com`.
//!
//! What is exercised here is the part that must not break: the callback that arrives from a
//! browser, the device code that arrives from a terminal, and the refresh that has to happen once
//! — not once per request — when an access token has expired.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use zlogic_codex::flow::{FlowOutcome, Flows, SignInMethod};
use zlogic_codex::{CALLBACK_PORT, Codex, Config, Subscription, store};
use zlogic_credential::{CredentialError, CredentialStore};
use zlogic_llm::TokenProvider;
use zlogic_protocol::llm::RequestMeta;
use zlogic_protocol::usage::Purpose;

#[derive(Default)]
struct Memory(Mutex<HashMap<String, String>>);

impl CredentialStore for Memory {
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

#[derive(Default)]
struct State {
    grants: Vec<String>,
    device_polls: usize,
    /// Answered on the poll after this many pending answers.
    pending_polls: usize,
}

struct Issuer {
    base: String,
    state: Arc<Mutex<State>>,
}

impl Issuer {
    /// A stand-in for the issuer, answering the endpoints the crate calls and nothing else.
    async fn start(pending_polls: usize) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(State {
            pending_polls,
            ..Default::default()
        }));
        let shared = Arc::clone(&state);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let shared = Arc::clone(&shared);
                tokio::spawn(async move { serve(stream, shared).await });
            }
        });
        Self {
            base: format!("http://127.0.0.1:{port}"),
            state,
        }
    }

    fn config(&self) -> Config {
        Config {
            issuer: self.base.clone(),
            backend: format!("{}/backend", self.base),
            client_id: "client-1".into(),
            originator: "zlogic".into(),
            client_version: "zlogic/0.0.0-test".into(),
        }
    }

    /// A client that talks to this issuer directly: a machine with a system proxy configured would
    /// otherwise send loopback traffic through it.
    fn codex(&self) -> Codex {
        Codex::with_client(
            self.config(),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        )
    }

    fn token_grants(&self) -> Vec<String> {
        self.state.lock().unwrap().grants.clone()
    }
}

async fn serve(mut stream: TcpStream, state: Arc<Mutex<State>>) {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = stream.read(&mut chunk).await.unwrap_or(0);
        if read == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..read]);
        if let Some(end) = find(&buf, b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..end]).to_string();
            if buf.len() >= end + 4 + content_length(&head) {
                break;
            }
        }
    }
    let request = String::from_utf8_lossy(&buf).to_string();
    let split = request.find("\r\n\r\n").unwrap_or(request.len());
    let head = request[..split].to_string();
    let body = request.get(split + 4..).unwrap_or_default().to_string();
    let target = head.split_whitespace().nth(1).unwrap_or("/").to_string();
    let path = target.split('?').next().unwrap_or("/").to_string();

    let response = route(&path, &body, &state);
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

fn route(path: &str, body: &str, state: &Arc<Mutex<State>>) -> String {
    match path {
        "/oauth/token" => {
            let form = form(body);
            let grant = form.get("grant_type").cloned().unwrap_or_default();
            state.lock().unwrap().grants.push(grant.clone());
            let access = if grant == "refresh_token" {
                "access-refreshed"
            } else {
                "access-1"
            };
            ok(&json!({
                "access_token": access,
                "refresh_token": "refresh-1",
                "expires_in": 3600,
                "id_token": jwt(json!({
                    "chatgpt_account_id": "acct-1",
                    "chatgpt_plan_type": "plus",
                    "email": "me@example.com",
                })),
            }))
        }
        "/api/accounts/deviceauth/usercode" => ok(&json!({
            "device_auth_id": "device-1",
            "user_code": "ABCD-1234",
            "interval": 1,
        })),
        "/api/accounts/deviceauth/token" => {
            let mut state = state.lock().unwrap();
            state.device_polls += 1;
            if state.device_polls <= state.pending_polls {
                return status(403, &json!({"error": "authorization_pending"}));
            }
            ok(&json!({
                "authorization_code": "code-1",
                "code_verifier": "verifier-1",
            }))
        }
        other => status(404, &json!({ "error": format!("no route for {other}") })),
    }
}

fn ok(body: &Value) -> String {
    status(200, body)
}

fn status(code: u16, body: &Value) -> String {
    let body = body.to_string();
    format!(
        "HTTP/1.1 {code} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn jwt(payload: Value) -> String {
    let encode = |text: &str| URL_SAFE_NO_PAD.encode(text.as_bytes());
    format!(
        "{}.{}.signature",
        encode(r#"{"alg":"none"}"#),
        encode(&payload.to_string())
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

fn form(body: &str) -> HashMap<String, String> {
    body.split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

/// A browser (or `curl`) hitting the loopback callback — by the hostname the redirect URI names,
/// so the address family the browser picks is exercised too.
async fn hit_callback(query: &str) -> String {
    let mut stream = TcpStream::connect(("localhost", CALLBACK_PORT))
        .await
        .expect("the callback listener must be reachable at localhost");
    let request = format!(
        "GET /auth/callback?{query} HTTP/1.1\r\nhost: localhost:{CALLBACK_PORT}\r\nconnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response).await;
    response
}

async fn wait_for(flows: &Flows, flow_id: &str) -> FlowOutcome {
    for _ in 0..400 {
        if let Some(outcome) = flows.status(flow_id)
            && outcome.is_final()
        {
            return outcome;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the sign-in never reached a final state");
}

fn query_param(url: &str, name: &str) -> String {
    let query = url.split_once('?').map(|(_, q)| q).unwrap_or_default();
    let value = form(query)
        .remove(name)
        .unwrap_or_else(|| panic!("{url} carries no {name}"));
    percent_decode(&value)
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&value[i + 1..i + 3], 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[tokio::test]
async fn the_browser_callback_lands_a_usable_subscription() {
    let issuer = Issuer::start(0).await;
    let store = Arc::new(Memory::default());
    let flows = Flows::new(Arc::new(issuer.codex()), store.clone());

    let begin = match flows.begin("codex", SignInMethod::Browser).await {
        Ok(begin) => begin,
        // The registered redirect URI names a fixed port, so a machine that has 1455 busy cannot
        // run this half of the test. It is an environment limit, not a behaviour to assert on.
        Err(zlogic_codex::Error::CallbackPortInUse(port)) => {
            eprintln!("skipping: port {port} is in use");
            return;
        }
        Err(error) => panic!("{error}"),
    };
    assert_eq!(begin.method, SignInMethod::Browser);
    assert_eq!(begin.user_code, None);

    let state = query_param(&begin.authorization_url, "state");
    let page = hit_callback(&format!("code=code-1&state={state}")).await;
    assert!(page.contains("200 OK"), "{page}");
    assert!(page.contains("Signed in"), "{page}");

    let outcome = wait_for(&flows, &begin.flow_id).await;
    assert!(
        matches!(outcome, FlowOutcome::Succeeded { .. }),
        "{outcome:?}"
    );

    let tokens = store::read(&*store, "codex").expect("the tokens have to be stored");
    assert_eq!(tokens.access_token, "access-1");
    assert_eq!(tokens.refresh_token, "refresh-1");
    assert_eq!(
        tokens.account_id.as_deref(),
        Some("acct-1"),
        "the account comes out of the id token"
    );
    assert_eq!(tokens.label().as_deref(), Some("me@example.com"));
    assert!(!tokens.needs_refresh());
    assert!(
        store::load(&*store, "other").is_none(),
        "one entry per provider"
    );
}

#[tokio::test]
async fn a_callback_for_another_sign_in_is_refused() {
    let issuer = Issuer::start(0).await;
    let store = Arc::new(Memory::default());
    let flows = Flows::new(Arc::new(issuer.codex()), store.clone());

    let begin = match flows.begin("codex", SignInMethod::Browser).await {
        Ok(begin) => begin,
        Err(zlogic_codex::Error::CallbackPortInUse(port)) => {
            eprintln!("skipping: port {port} is in use");
            return;
        }
        Err(error) => panic!("{error}"),
    };

    let page = hit_callback("code=code-1&state=not-ours").await;
    assert!(page.contains("did not match"), "{page}");

    let outcome = wait_for(&flows, &begin.flow_id).await;
    assert!(matches!(outcome, FlowOutcome::Failed { .. }), "{outcome:?}");
    assert!(
        store::load(&*store, "codex").is_none(),
        "a refused callback must not leave a credential behind"
    );
}

#[tokio::test]
async fn a_pending_device_poll_is_not_a_failure() {
    let issuer = Issuer::start(1).await;
    let codex = issuer.codex();

    let device = codex.device_start().await.unwrap();
    assert_eq!(device.user_code, "ABCD-1234");
    assert!(codex.config().device_url().ends_with("/codex/device"));

    assert!(matches!(
        codex.device_poll(&device).await.unwrap(),
        zlogic_codex::DevicePoll::Pending
    ));
    let zlogic_codex::DevicePoll::Done(tokens) = codex.device_poll(&device).await.unwrap() else {
        panic!("the second poll carries the code");
    };
    assert_eq!(tokens.access_token, "access-1");
    assert_eq!(tokens.refresh_token, "refresh-1");
}

#[tokio::test]
async fn the_device_flow_finishes_without_a_callback() {
    let issuer = Issuer::start(0).await;
    let store = Arc::new(Memory::default());
    let flows = Flows::new(Arc::new(issuer.codex()), store.clone());

    let begin = flows.begin("codex", SignInMethod::Device).await.unwrap();
    assert_eq!(begin.method, SignInMethod::Device);
    assert_eq!(begin.user_code.as_deref(), Some("ABCD-1234"));
    assert!(begin.authorization_url.ends_with("/codex/device"));

    let outcome = wait_for(&flows, &begin.flow_id).await;
    assert!(
        matches!(outcome, FlowOutcome::Succeeded { .. }),
        "{outcome:?}"
    );
    assert_eq!(
        store::read(&*store, "codex").unwrap().access_token,
        "access-1"
    );
}

#[tokio::test]
async fn a_cancelled_flow_stops_without_storing_anything() {
    let issuer = Issuer::start(0).await;
    let store = Arc::new(Memory::default());
    let flows = Flows::new(Arc::new(issuer.codex()), store.clone());

    let begin = flows.begin("codex", SignInMethod::Device).await.unwrap();
    assert_eq!(flows.provider_of(&begin.flow_id).as_deref(), Some("codex"));
    flows.cancel(&begin.flow_id).unwrap();

    assert!(matches!(
        flows.status(&begin.flow_id),
        Some(FlowOutcome::Cancelled)
    ));
    assert!(store::load(&*store, "codex").is_none());
    assert!(
        matches!(
            flows.cancel("not-a-flow"),
            Err(zlogic_codex::Error::UnknownFlow)
        ),
        "cancelling something that never started is a lookup failure, not a silent success"
    );
}

#[tokio::test]
async fn an_expired_token_is_refreshed_once_even_under_concurrent_requests() {
    let issuer = Issuer::start(0).await;
    let store = Arc::new(Memory::default());
    let expired = zlogic_codex::Tokens {
        access_token: "access-stale".into(),
        refresh_token: "refresh-1".into(),
        expires_at: 0,
        account_id: Some("acct-1".into()),
        ..Default::default()
    };
    store::save(&*store, "codex", &expired).unwrap();

    let subscription = Subscription::new("codex", Arc::new(issuer.codex()), store.clone());

    let meta = RequestMeta {
        session_id: "session-1".into(),
        turn_id: "turn-1".into(),
        round_id: "round-1".into(),
        purpose: Purpose::Main,
    };
    let (first, second) = tokio::join!(subscription.token(&meta), subscription.token(&meta));
    let (first, second) = (first.unwrap(), second.unwrap());

    assert_eq!(first.access, "access-refreshed");
    assert_eq!(second.access, "access-refreshed");
    assert_eq!(
        issuer.token_grants(),
        ["refresh_token"],
        "two requests in flight must share one refresh"
    );
    assert_eq!(
        store::read(&*store, "codex").unwrap().access_token,
        "access-refreshed",
        "the refreshed token is written back, so a restart does not refresh again"
    );

    let account = first
        .headers
        .iter()
        .find(|(name, _)| name == "chatgpt-account-id")
        .map(|(_, value)| value.clone());
    assert_eq!(account.as_deref(), Some("acct-1"));
    assert!(
        first
            .headers
            .iter()
            .any(|(name, value)| name == "session_id" && value == "session-1"),
        "the session travels with the request: {:?}",
        first.headers
    );
    assert!(
        !first
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("authorization")),
        "the bearer token belongs in `access` and must not also be a header"
    );
}

#[tokio::test]
async fn a_subscription_that_was_never_signed_in_says_so() {
    let issuer = Issuer::start(0).await;
    let store = Arc::new(Memory::default());
    let subscription = Subscription::new("codex", Arc::new(issuer.codex()), store.clone());

    let meta = RequestMeta {
        session_id: "s".into(),
        turn_id: "t".into(),
        round_id: "r".into(),
        purpose: Purpose::Main,
    };
    let error = subscription.token(&meta).await.unwrap_err();
    assert_eq!(error.kind, zlogic_protocol::llm::LlmErrorKind::Auth);
    assert!(!error.retryable);
    assert!(error.message.contains("codex"), "{}", error.message);
}
