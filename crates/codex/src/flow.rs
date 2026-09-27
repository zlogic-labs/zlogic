//! The two interactive sign-ins, and the state a host polls while they run.
//!
//! A host starts a flow, shows the URL (or the code) to the user, and polls. Nothing here opens a
//! browser or draws anything: that belongs to the front end, which may be a terminal, a desktop
//! window or a phone.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;
use zlogic_credential::CredentialStore;

use crate::oauth::{Codex, DevicePoll, authorize_url};
use crate::pkce::{Pkce, random_state};
use crate::{CALLBACK_PORT, Error, store};

const BROWSER_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const DEVICE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// The device endpoint rejects a poll that comes too soon after the last one.
const POLL_SAFETY_MARGIN: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignInMethod {
    /// A loopback callback on [`CALLBACK_PORT`], with the browser as the front end.
    Browser,
    /// A code the user types at the issuer's device page: the only flow that works headless.
    Device,
}

#[derive(Debug, Clone)]
pub struct Begin {
    pub flow_id: String,
    pub method: SignInMethod,
    pub authorization_url: String,
    pub user_code: Option<String>,
    pub instructions: Option<String>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlowOutcome {
    Pending,
    Succeeded {
        account: Option<String>,
        label: Option<String>,
        plan: Option<String>,
    },
    Failed {
        message: String,
    },
    Expired,
    Cancelled,
}

impl FlowOutcome {
    pub fn is_final(&self) -> bool {
        !matches!(self, FlowOutcome::Pending)
    }
}

struct Entry {
    provider_id: String,
    outcome: Arc<Mutex<FlowOutcome>>,
    cancel: watch::Sender<bool>,
    expires_at: Instant,
    expires_at_wall: DateTime<Utc>,
}

/// A registered flow: its id, the outcome the task writes, and its cancel channel.
type Registered = (String, Arc<Mutex<FlowOutcome>>, watch::Receiver<bool>);

pub struct Flows {
    codex: Arc<Codex>,
    store: Arc<dyn CredentialStore>,
    entries: Mutex<HashMap<String, Entry>>,
}

/// What a flow looks like from the outside.
#[derive(Debug, Clone)]
pub struct FlowSnapshot {
    pub provider_id: String,
    pub outcome: FlowOutcome,
    pub expires_at: DateTime<Utc>,
}

impl Flows {
    pub fn new(codex: Arc<Codex>, store: Arc<dyn CredentialStore>) -> Self {
        Self {
            codex,
            store,
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub async fn begin(&self, provider_id: &str, method: SignInMethod) -> Result<Begin, Error> {
        match method {
            SignInMethod::Browser => self.begin_browser(provider_id).await,
            SignInMethod::Device => self.begin_device(provider_id).await,
        }
    }

    pub fn provider_of(&self, flow_id: &str) -> Option<String> {
        let entries = self.entries.lock().ok()?;
        entries.get(flow_id).map(|e| e.provider_id.clone())
    }

    pub fn view(&self, flow_id: &str) -> Option<FlowSnapshot> {
        let entries = self.entries.lock().ok()?;
        let entry = entries.get(flow_id)?;
        let outcome = entry.outcome.lock().ok()?.clone();
        let outcome = match outcome {
            FlowOutcome::Pending if Instant::now() >= entry.expires_at => FlowOutcome::Expired,
            other => other,
        };
        Some(FlowSnapshot {
            provider_id: entry.provider_id.clone(),
            outcome,
            expires_at: entry.expires_at_wall,
        })
    }

    pub fn status(&self, flow_id: &str) -> Option<FlowOutcome> {
        self.view(flow_id).map(|snapshot| snapshot.outcome)
    }

    pub fn cancel(&self, flow_id: &str) -> Result<(), Error> {
        let entries = self.entries.lock().map_err(|_| self.poisoned())?;
        let entry = entries.get(flow_id).ok_or(Error::UnknownFlow)?;
        let _ = entry.cancel.send(true);
        if let Ok(mut outcome) = entry.outcome.lock() {
            *outcome = FlowOutcome::Cancelled;
        }
        Ok(())
    }

    fn register(&self, provider_id: &str, timeout: Duration) -> Result<Registered, Error> {
        let flow_id = random_state()?;
        let outcome = Arc::new(Mutex::new(FlowOutcome::Pending));
        let (cancel, receiver) = watch::channel(false);
        let entry = Entry {
            provider_id: provider_id.to_string(),
            outcome: Arc::clone(&outcome),
            cancel,
            expires_at: Instant::now() + timeout,
            expires_at_wall: Utc::now() + chrono::Duration::from_std(timeout).unwrap_or_default(),
        };
        self.entries
            .lock()
            .map_err(|_| self.poisoned())?
            .insert(flow_id.clone(), entry);
        Ok((flow_id, outcome, receiver))
    }

    async fn begin_browser(&self, provider_id: &str) -> Result<Begin, Error> {
        let listener = TcpListener::bind(("127.0.0.1", CALLBACK_PORT))
            .await
            .map_err(|_| Error::CallbackPortInUse(CALLBACK_PORT))?;
        /* The registered redirect URI says `localhost`, which a browser may resolve to the other
         * loopback family first — so the other one is listened on too. Best effort: a machine
         * without IPv6 (or with it disabled) simply has one listener. */
        let secondary = TcpListener::bind(("::1", CALLBACK_PORT)).await.ok();
        let pkce = Pkce::generate()?;
        let state = random_state()?;
        let url = authorize_url(self.codex.config(), &pkce, &state);
        let (flow_id, outcome, cancel) = self.register(provider_id, BROWSER_TIMEOUT)?;

        let codex = Arc::clone(&self.codex);
        let store = Arc::clone(&self.store);
        let provider = provider_id.to_string();
        let expected_state = state.clone();
        tokio::spawn(async move {
            let result = tokio::select! {
                _ = cancelled(cancel) => TaskEnd::Cancelled,
                accepted = tokio::time::timeout(
                    BROWSER_TIMEOUT,
                    serve_callback(listener, secondary, &expected_state),
                ) => {
                    match accepted {
                        Ok(Ok(code)) => match codex.exchange_code(&code, &pkce.verifier).await {
                            Ok(tokens) => match store::save(&*store, &provider, &tokens) {
                                Ok(()) => TaskEnd::Done(Box::new(tokens)),
                                Err(e) => TaskEnd::Failed(e.to_string()),
                            },
                            Err(e) => TaskEnd::Failed(e.to_string()),
                        },
                        Ok(Err(e)) => TaskEnd::Failed(e.to_string()),
                        Err(_) => TaskEnd::Expired,
                    }
                }
            };
            *outcome.lock().expect("flow outcome") = result.into_outcome();
        });

        Ok(Begin {
            flow_id,
            method: SignInMethod::Browser,
            authorization_url: url,
            user_code: None,
            instructions: Some(
                "Finish signing in from the browser window that just opened.".into(),
            ),
            expires_at: Utc::now()
                + chrono::Duration::from_std(BROWSER_TIMEOUT).unwrap_or_default(),
        })
    }

    async fn begin_device(&self, provider_id: &str) -> Result<Begin, Error> {
        let device = self.codex.device_start().await?;
        let (flow_id, outcome, cancel) = self.register(provider_id, DEVICE_TIMEOUT)?;

        let codex = Arc::clone(&self.codex);
        let store = Arc::clone(&self.store);
        let provider = provider_id.to_string();
        let interval = Duration::from_secs(device.interval_secs) + POLL_SAFETY_MARGIN;
        let user_code = device.user_code.clone();
        tokio::spawn(async move {
            let deadline = tokio::time::Instant::now() + DEVICE_TIMEOUT;
            let result = loop {
                tokio::select! {
                    _ = cancelled(cancel.clone()) => break TaskEnd::Cancelled,
                    _ = tokio::time::sleep_until(deadline) => break TaskEnd::Expired,
                    _ = tokio::time::sleep(interval) => {}
                }
                match codex.device_poll(&device).await {
                    Ok(DevicePoll::Pending) => continue,
                    Ok(DevicePoll::Done(tokens)) => {
                        break match store::save(&*store, &provider, &tokens) {
                            Ok(()) => TaskEnd::Done(tokens),
                            Err(e) => TaskEnd::Failed(e.to_string()),
                        };
                    }
                    Err(e) => break TaskEnd::Failed(e.to_string()),
                }
            };
            *outcome.lock().expect("flow outcome") = result.into_outcome();
        });

        Ok(Begin {
            flow_id,
            method: SignInMethod::Device,
            authorization_url: self.codex.config().device_url(),
            user_code: Some(user_code),
            instructions: Some("Open the URL and enter the code shown above.".into()),
            expires_at: Utc::now() + chrono::Duration::from_std(DEVICE_TIMEOUT).unwrap_or_default(),
        })
    }

    fn poisoned(&self) -> Error {
        Error::Protocol("the sign-in registry is unusable".into())
    }
}

enum TaskEnd {
    Cancelled,
    Expired,
    Failed(String),
    Done(Box<crate::Tokens>),
}

impl TaskEnd {
    fn into_outcome(self) -> FlowOutcome {
        match self {
            TaskEnd::Cancelled => FlowOutcome::Cancelled,
            TaskEnd::Expired => FlowOutcome::Expired,
            TaskEnd::Failed(message) => FlowOutcome::Failed { message },
            TaskEnd::Done(tokens) => FlowOutcome::Succeeded {
                account: tokens.account_id.clone(),
                label: tokens.label(),
                plan: tokens.plan.clone(),
            },
        }
    }
}

async fn cancelled(mut cancel: watch::Receiver<bool>) {
    let _ = cancel.changed().await;
}

/// Accept on a listener that may not exist: without one this simply never resolves, so the
/// `select!` above falls back to the primary.
async fn accept_on(
    listener: Option<&TcpListener>,
) -> std::io::Result<(tokio::net::TcpStream, std::net::SocketAddr)> {
    match listener {
        Some(listener) => listener.accept().await,
        None => std::future::pending().await,
    }
}

async fn serve_callback(
    primary: TcpListener,
    secondary: Option<TcpListener>,
    expected_state: &str,
) -> Result<String, Error> {
    // A browser asks for `/favicon.ico` (and may retry) before or instead of the callback, so a
    // single accept is not enough — but an unbounded loop is a way to be held open forever.
    for _ in 0..16 {
        let accepted = tokio::select! {
            accepted = primary.accept() => accepted,
            accepted = accept_on(secondary.as_ref()) => accepted,
        };
        let Ok((mut socket, _)) = accepted else {
            return Err(Error::Protocol("the callback listener stopped".into()));
        };
        let mut buf = vec![0u8; 8192];
        let read = socket.read(&mut buf).await.unwrap_or(0);
        let request = String::from_utf8_lossy(&buf[..read]).to_string();
        let target = request.split_whitespace().nth(1).unwrap_or("/").to_string();
        let (path, query) = match target.split_once('?') {
            Some((path, query)) => (path, query),
            None => (target.as_str(), ""),
        };

        if path != "/auth/callback" {
            reply(&mut socket, 404, "Not found").await;
            continue;
        }

        let params = parse_query(query);
        if let Some(error) = params.get("error") {
            let message = params
                .get("error_description")
                .cloned()
                .unwrap_or_else(|| error.clone());
            reply(&mut socket, 200, &page("Sign-in failed", &message)).await;
            return Err(Error::Protocol(message));
        }

        let Some(code) = params.get("code") else {
            reply(
                &mut socket,
                400,
                &page("Sign-in failed", "The callback carried no code."),
            )
            .await;
            return Err(Error::Protocol("the callback carried no code".into()));
        };
        if params.get("state").map(String::as_str) != Some(expected_state) {
            let message = "The sign-in callback did not match this request.";
            reply(&mut socket, 400, &page("Sign-in failed", message)).await;
            return Err(Error::Protocol(message.into()));
        }

        reply(
            &mut socket,
            200,
            &page(
                "Signed in",
                "You can close this window and return to zlogic.",
            ),
        )
        .await;
        return Ok(code.clone());
    }
    Err(Error::Protocol(
        "something else kept the callback port busy".into(),
    ))
}

async fn reply(socket: &mut tokio::net::TcpStream, status: u16, body: &str) {
    let response = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        if status == 200 { "OK" } else { "Error" },
        body.len()
    );
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.flush().await;
}

fn page(title: &str, message: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{}</title></head>\
         <body style=\"font-family:system-ui;margin:4rem auto;max-width:32rem\">\
         <h1>{}</h1><p>{}</p></body></html>",
        escape(title),
        escape(title),
        escape(message)
    )
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            Some((decode(key), decode(value)))
        })
        .collect()
}

fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok());
                match hex {
                    Some(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    None => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_query_is_decoded_the_way_a_browser_escaped_it() {
        let params = parse_query("code=abc%2Fdef&state=x+y&error_description=a%26b");
        assert_eq!(params["code"], "abc/def");
        assert_eq!(params["state"], "x y");
        assert_eq!(params["error_description"], "a&b");
    }

    #[test]
    fn a_callback_page_cannot_inject_markup() {
        let rendered = page("<script>", "& <b>");
        assert!(!rendered.contains("<script>"));
        assert!(rendered.contains("&amp;"));
    }

    #[test]
    fn a_broken_query_never_panics() {
        assert!(parse_query("").is_empty());
        assert!(parse_query("flag").is_empty());
        assert_eq!(parse_query("a=%").len(), 1);
    }
}
