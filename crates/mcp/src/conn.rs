//! One live connection to one server.
//! # What the SDK gives us, and what it does not
//! `rmcp` provides a transport and a single session on top of it: the `initialize` handshake,
//! request-id multiplexing (so concurrent `tools/call`s share one connection), and notification
//! delivery. That is all we take. Which connection a call goes to, when one is created, when it is
//! reclaimed and what happens when a server crashes are not in the SDK — they are [`crate::pool`].
//! # A stdio server's stderr has to be read, not inherited
//! Two reasons, and the second is the one that bites. Inherited stderr means a chatty server writes
//! over a terminal UI. And an un-drained pipe **fills and blocks the child**: a server logging to
//! stderr would hang mid-request with nothing in any log to explain it. So the pipe is drained
//! continuously into tracing, and the last few lines are kept — because "the server exited" is
//! useless on its own, while "the server exited: `command not found: uvx`" is the whole answer.

use std::collections::{BTreeSet, VecDeque};
use std::fmt;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rmcp::RoleClient;
use rmcp::ServiceExt;
use rmcp::handler::client::ClientHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, ClientInfo, ElicitRequestParams,
    ElicitResult, ElicitationAction, ElicitationCapability, FormElicitationCapability,
    Implementation, JsonObject, Tool as RmcpTool,
};
use rmcp::service::{RequestContext, RunningService};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{
    AuthClient, ConfigureCommandExt, StreamableHttpClientTransport, TokioChildProcess,
};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::resolve::ResolvedTransport;
use crate::security::sanitize_untrusted_text;
use crate::{McpError, Result};

/// How we introduce ourselves in the handshake. Servers log this, and some gate behaviour on it.
fn client_info(elicitation: bool) -> ClientInfo {
    let mut capabilities = ClientCapabilities::default();
    if elicitation {
        capabilities.elicitation = Some(
            ElicitationCapability::new()
                .with_form(FormElicitationCapability::new().with_schema_validation(false)),
        );
    }
    ClientInfo::new(
        capabilities,
        Implementation::new("zlogic", env!("CARGO_PKG_VERSION")),
    )
}

#[derive(Clone)]
struct ZlogicClient {
    label: Label,
}

impl ClientHandler for ZlogicClient {
    fn get_info(&self) -> ClientInfo {
        client_info(self.label.interaction.is_some())
    }

    async fn create_elicitation(
        &self,
        request: ElicitRequestParams,
        _: RequestContext<RoleClient>,
    ) -> std::result::Result<ElicitResult, rmcp::ErrorData> {
        handle_elicitation(&self.label, request).await
    }
}

pub struct Connection {
    /// The definition this was created for. The pool evicts by it (disable, remove, token change),
    /// which is why it is recorded here even though it is not part of the pool key.
    pub server_id: String,
    /// Who caused it to exist. Observability only — a connection is shared, so this is the *first*
    /// user, not the only one.
    pub created_for: Label,
    service: RunningService<RoleClient, ZlogicClient>,
    stderr: Arc<StderrTail>,
    created: Instant,
}

/// Who first asked for a connection.
#[derive(Clone)]
pub struct Label {
    pub workspace_root: std::path::PathBuf,
    pub session: Option<zlogic_protocol::SessionId>,
    pub turn: Option<zlogic_protocol::TurnId>,
    pub call: Option<zlogic_protocol::CallId>,
    pub interaction: Option<Arc<dyn zlogic_protocol::InteractionPort>>,
}

impl fmt::Debug for Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Label")
            .field("workspace_root", &self.workspace_root)
            .field("session", &self.session)
            .field("turn", &self.turn)
            .field("call", &self.call)
            .field("interaction", &self.interaction.is_some())
            .finish()
    }
}

impl PartialEq for Label {
    fn eq(&self, other: &Self) -> bool {
        self.workspace_root == other.workspace_root
            && self.session == other.session
            && self.turn == other.turn
            && self.call == other.call
            && self.interaction.is_some() == other.interaction.is_some()
    }
}

impl Eq for Label {}

async fn handle_elicitation(
    label: &Label,
    request: ElicitRequestParams,
) -> std::result::Result<ElicitResult, rmcp::ErrorData> {
    let ElicitRequestParams::FormElicitationParams {
        message,
        requested_schema,
        ..
    } = request
    else {
        // Zlogic does not advertise URL elicitation. Declining is safer than opening an untrusted
        // server-provided URL if a server sends one anyway.
        return Ok(ElicitResult::new(ElicitationAction::Decline));
    };
    let port = label.interaction.as_ref().ok_or_else(|| {
        rmcp::ErrorData::internal_error("MCP elicitation requires an attached UI", None)
    })?;
    let session_id = label.session.ok_or_else(|| {
        rmcp::ErrorData::internal_error("MCP elicitation has no zlogic session", None)
    })?;
    let turn_id = label.turn.ok_or_else(|| {
        rmcp::ErrorData::internal_error("MCP elicitation has no zlogic turn", None)
    })?;
    let form = elicitation_form(message, &requested_schema)?;
    let decision = port
        .ask(zlogic_protocol::InteractionRequest {
            interaction_id: zlogic_protocol::EntryId::new().to_string(),
            session_id,
            turn_id,
            call_id: label.call.clone(),
            body: zlogic_protocol::InteractionBody::Form(form),
        })
        .await
        .map_err(|error| rmcp::ErrorData::internal_error(error, None))?;
    match decision {
        zlogic_protocol::InteractionDecision::Submitted(answer) => {
            let content = answer
                .values
                .into_iter()
                .map(|(key, value)| (key, field_value_to_json(value)))
                .collect();
            Ok(ElicitResult::new(ElicitationAction::Accept).with_content(Value::Object(content)))
        }
        zlogic_protocol::InteractionDecision::Deny { .. } => {
            Ok(ElicitResult::new(ElicitationAction::Decline))
        }
        zlogic_protocol::InteractionDecision::Cancelled => {
            Ok(ElicitResult::new(ElicitationAction::Cancel))
        }
        zlogic_protocol::InteractionDecision::Allow { .. } => Err(rmcp::ErrorData::internal_error(
            "unexpected permission answer for MCP form",
            None,
        )),
    }
}

fn field_value_to_json(value: zlogic_protocol::FieldValue) -> Value {
    match value {
        zlogic_protocol::FieldValue::Text(value) | zlogic_protocol::FieldValue::Choice(value) => {
            Value::String(value)
        }
        zlogic_protocol::FieldValue::Number(value) => serde_json::Number::from_f64(value)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        zlogic_protocol::FieldValue::Bool(value) => Value::Bool(value),
        zlogic_protocol::FieldValue::Choices(values) => {
            Value::Array(values.into_iter().map(Value::String).collect())
        }
    }
}

fn elicitation_form(
    message: String,
    schema: &rmcp::model::ElicitationSchema,
) -> std::result::Result<zlogic_protocol::Form, rmcp::ErrorData> {
    let value = serde_json::to_value(schema)
        .map_err(|error| rmcp::ErrorData::invalid_params(error.to_string(), None))?;
    let required: BTreeSet<&str> = value
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let properties = value
        .get("properties")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            rmcp::ErrorData::invalid_params("elicitation schema has no properties", None)
        })?;
    let mut fields = Vec::with_capacity(properties.len());
    for (key, property) in properties {
        let key_text: &str = key;
        let is_required = required.iter().any(|candidate| *candidate == key_text);
        fields.push(elicitation_field(key, property, is_required)?);
    }
    Ok(zlogic_protocol::Form {
        title: value
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("MCP input")
            .to_string(),
        message: Some(message),
        fields,
        submit_label: Some("Continue".into()),
        cancel_label: Some("Cancel".into()),
    })
}

fn elicitation_field(
    key: &str,
    property: &Value,
    required: bool,
) -> std::result::Result<zlogic_protocol::FormField, rmcp::ErrorData> {
    let kind = property
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("string");
    let label = property.get("title").and_then(Value::as_str).unwrap_or(key);
    let control = if kind == "array" {
        let items = property.get("items").unwrap_or(&Value::Null);
        zlogic_protocol::Control::MultiSelect {
            options: choices(items)?,
            defaults: property
                .get("default")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
            min: usize_value(property, "minItems"),
            max: usize_value(property, "maxItems"),
        }
    } else if property.get("enum").is_some() || property.get("oneOf").is_some() {
        zlogic_protocol::Control::Select {
            options: choices(property)?,
            default: property
                .get("default")
                .and_then(Value::as_str)
                .map(str::to_string),
            // MCP enum fields are closed sets — no "other" escape hatch.
            free_text: false,
        }
    } else {
        match kind {
            "string" => zlogic_protocol::Control::Input {
                default: property
                    .get("default")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                placeholder: None,
                max_len: usize_value(property, "maxLength"),
            },
            "number" | "integer" => zlogic_protocol::Control::Number {
                default: property.get("default").and_then(Value::as_f64),
                min: property.get("minimum").and_then(Value::as_f64),
                max: property.get("maximum").and_then(Value::as_f64),
                integer: kind == "integer",
            },
            "boolean" => zlogic_protocol::Control::Confirm {
                default: property.get("default").and_then(Value::as_bool),
            },
            other => {
                return Err(rmcp::ErrorData::invalid_params(
                    format!("unsupported elicitation field type `{other}`"),
                    None,
                ));
            }
        }
    };
    let mut field = if required {
        zlogic_protocol::FormField::required(key, label, control)
    } else {
        zlogic_protocol::FormField::new(key, label, control)
    };
    if let Some(help) = property.get("description").and_then(Value::as_str) {
        field = field.with_help(help);
    }
    Ok(field)
}

fn choices(value: &Value) -> std::result::Result<Vec<zlogic_protocol::Choice>, rmcp::ErrorData> {
    let source = value.get("items").unwrap_or(value);
    if let Some(values) = source.get("enum").and_then(Value::as_array) {
        let labels = source
            .get("enumNames")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        return Ok(values
            .iter()
            .enumerate()
            .filter_map(|(index, value)| {
                let value = value.as_str()?;
                let label = labels.get(index).and_then(Value::as_str).unwrap_or(value);
                Some(zlogic_protocol::Choice::new(value, label))
            })
            .collect());
    }
    let titled = source
        .get("oneOf")
        .or_else(|| source.get("anyOf"))
        .and_then(Value::as_array)
        .ok_or_else(|| rmcp::ErrorData::invalid_params("elicitation enum has no choices", None))?;
    Ok(titled
        .iter()
        .filter_map(|item| {
            let value = item.get("const")?.as_str()?;
            let label = item.get("title").and_then(Value::as_str).unwrap_or(value);
            Some(zlogic_protocol::Choice::new(value, label))
        })
        .collect())
}

fn usize_value(value: &Value, key: &str) -> Option<usize> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
}

impl Connection {
    /// Connects and completes the handshake, or fails within `timeout`.
    /// A handshake that never answers is the common failure for a wrapper script that prints usage
    /// and waits: without a bound, the first tool call would hang for as long as the turn lasts.
    pub async fn open(
        server_id: &str,
        transport: &ResolvedTransport,
        created_for: Label,
        timeout: Duration,
    ) -> Result<Self> {
        let fail = |reason: String| McpError::Connect {
            server: server_id.to_string(),
            reason,
        };
        let stderr = Arc::new(StderrTail::default());

        match transport {
            ResolvedTransport::Stdio {
                command,
                args,
                env,
                cwd,
            } => {
                let cmd = tokio::process::Command::new(command).configure(|c| {
                    c.args(args).envs(env).current_dir(cwd);
                    // MCP servers are often console binaries; from a GUI host they must not open
                    // a console window for the lifetime of the connection.
                    #[cfg(windows)]
                    c.creation_flags(0x0800_0000);
                });
                let (child, pipe) = TokioChildProcess::builder(cmd)
                    .stderr(Stdio::piped())
                    .spawn()
                    .map_err(|e| {
                        // The overwhelmingly common case, and worth naming: the command is not
                        // installed, or not on zlogic's PATH (which is not the shell's).
                        fail(format!("cannot start `{command}`: {e}"))
                    })?;
                if let Some(pipe) = pipe {
                    stderr.clone().drain(server_id.to_string(), pipe);
                }
                Self::handshake(
                    server_id,
                    created_for,
                    stderr,
                    child,
                    timeout,
                    format!("`{command}` did not complete the MCP handshake"),
                    None,
                )
                .await
            }
            ResolvedTransport::Http {
                url,
                headers,
                oauth,
            } => {
                let endpoint = endpoint_label(url);
                let mut config = StreamableHttpClientTransportConfig::with_uri(url.clone());
                if *oauth {
                    let Some(manager) = crate::oauth::restore_authorization(server_id, url)
                        .await
                        .map_err(|error| fail(error.to_string()))?
                    else {
                        return Err(fail(
                            "OAuth authorization is required; authorize this MCP server first"
                                .into(),
                        ));
                    };
                    let mut default_headers = reqwest::header::HeaderMap::new();
                    for (name, value) in http_headers(server_id, headers)? {
                        default_headers.insert(name, value);
                    }
                    let client = reqwest::Client::builder()
                        .default_headers(default_headers)
                        .build()
                        .map_err(|error| {
                            fail(format!("cannot build OAuth HTTP client: {error}"))
                        })?;
                    let http = StreamableHttpClientTransport::with_client(
                        AuthClient::new(client, manager),
                        config,
                    );
                    return Self::handshake(
                        server_id,
                        created_for,
                        stderr,
                        http,
                        timeout,
                        format!("{endpoint} did not answer"),
                        Some((url.clone(), endpoint)),
                    )
                    .await;
                } else if !headers.is_empty() {
                    config = config.custom_headers(http_headers(server_id, headers)?);
                }
                let http = StreamableHttpClientTransport::from_config(config);
                Self::handshake(
                    server_id,
                    created_for,
                    stderr,
                    http,
                    timeout,
                    format!("{endpoint} did not answer"),
                    Some((url.clone(), endpoint)),
                )
                .await
            }
        }
    }

    /// The handshake, whatever the transport turned out to be.
    /// Split out so a test can hand in an in-memory pipe and talk to a real server over the real
    /// protocol: everything above this is transport construction, and that is not where the protocol
    /// assumptions live.
    async fn handshake<T, E, A>(
        server_id: &str,
        created_for: Label,
        stderr: Arc<StderrTail>,
        transport: T,
        timeout: Duration,
        timeout_message: String,
        redaction: Option<(String, String)>,
    ) -> Result<Self>
    where
        T: rmcp::transport::IntoTransport<RoleClient, E, A>,
        E: std::error::Error + Send + Sync + 'static,
    {
        let fail = |reason: String| McpError::Connect {
            server: server_id.to_string(),
            reason,
        };
        let handler = ZlogicClient {
            label: created_for.clone(),
        };
        let service = tokio::time::timeout(timeout, handler.serve(transport))
            .await
            .map_err(|_| {
                fail(format!(
                    "{timeout_message} within {}s{}",
                    timeout.as_secs(),
                    stderr.suffix()
                ))
            })?
            .map_err(|e| {
                let mut reason = e.to_string();
                if let Some((secret, replacement)) = &redaction {
                    reason = reason.replace(secret, replacement);
                }
                fail(format!("{reason}{}", stderr.suffix()))
            })?;

        Ok(Self {
            server_id: server_id.to_string(),
            created_for,
            service,
            stderr,
            created: Instant::now(),
        })
    }

    /// The server's tool list, following pagination to the end.
    pub async fn list_tools(&self) -> Result<Vec<RmcpTool>> {
        self.service
            .list_all_tools()
            .await
            .map_err(|e| McpError::Call {
                server: self.server_id.clone(),
                reason: format!("tools/list failed: {e}{}", self.stderr.suffix()),
            })
    }

    /// One `tools/call`.
    /// A protocol-level failure is an `Err` here; a tool that *ran* and reported failure comes back
    /// as `Ok` with `is_error` set, because those are different things to the model: one is "this
    /// tool is unreachable", the other is "your arguments were wrong".
    pub async fn call_tool(
        &self,
        tool: &str,
        args: Option<JsonObject>,
        timeout: Duration,
    ) -> Result<CallToolResult> {
        let mut params = CallToolRequestParams::new(tool.to_string());
        if let Some(args) = args {
            params = params.with_arguments(args);
        }
        match tokio::time::timeout(timeout, self.service.call_tool(params)).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(e)) => Err(McpError::Call {
                server: self.server_id.clone(),
                reason: format!("{tool}: {e}{}", self.stderr.suffix()),
            }),
            Err(_) => Err(McpError::Timeout {
                server: self.server_id.clone(),
                tool: tool.to_string(),
                secs: timeout.as_secs(),
            }),
        }
    }

    /// Whether the session is gone — the child exited, or the HTTP session was closed.
    /// Checked before a connection is handed out: a dead one must be replaced rather than produce a
    /// transport error the model would read as the tool being broken.
    pub fn is_closed(&self) -> bool {
        self.service.is_closed()
    }

    pub fn age(&self) -> Duration {
        self.created.elapsed()
    }

    /// The last lines the server wrote to stderr. Empty for HTTP servers.
    pub fn stderr_tail(&self) -> String {
        self.stderr.text()
    }
}

/// Enough endpoint identity to diagnose a connection without zlogicing credentials that were
/// expanded into userinfo, the path or the query string.
fn endpoint_label(raw: &str) -> String {
    let Some((scheme, rest)) = raw.split_once("://") else {
        return "configured HTTP endpoint".into();
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(authority);
    if host.is_empty() {
        "configured HTTP endpoint".into()
    } else {
        format!("{scheme}://{host}")
    }
}

fn http_headers(
    server: &str,
    headers: &std::collections::BTreeMap<String, String>,
) -> Result<std::collections::HashMap<http::HeaderName, http::HeaderValue>> {
    let mut out = std::collections::HashMap::with_capacity(headers.len());
    for (k, v) in headers {
        let name = http::HeaderName::try_from(k.as_str()).map_err(|_| McpError::Connect {
            server: server.to_string(),
            reason: format!("`{k}` is not a valid HTTP header name"),
        })?;
        // The error deliberately does not include the value: a header value is where the token is.
        let value = http::HeaderValue::from_str(v).map_err(|_| McpError::Connect {
            server: server.to_string(),
            reason: format!("the value of header `{k}` is not valid in an HTTP header"),
        })?;
        out.insert(name, value);
    }
    Ok(out)
}

/// The last few stderr lines of a stdio server.
/// Bounded on purpose: a server that logs a line per request would otherwise grow this without
/// limit for as long as the connection lives, and nothing older than the last few lines has ever
/// helped explain a failure.
#[derive(Default)]
struct StderrTail(Mutex<VecDeque<String>>);

impl StderrTail {
    const LINES: usize = 12;
    const LINE_CHARS: usize = 400;

    fn drain(self: Arc<Self>, server_id: String, pipe: tokio::process::ChildStderr) {
        tokio::spawn(async move {
            let mut lines = BufReader::new(pipe).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let line: String = sanitize_untrusted_text(&line)
                    .chars()
                    .take(Self::LINE_CHARS)
                    .collect();
                // Server logs are the server's business, not the user's: they go to the log at
                // debug, and only surface in an error message if something actually failed.
                tracing::debug!(target: "zlogic::mcp", server = %server_id, "{line}");
                let mut buf = self.0.lock().unwrap_or_else(|e| e.into_inner());
                if buf.len() == Self::LINES {
                    buf.pop_front();
                }
                buf.push_back(line);
            }
        });
    }

    fn text(&self) -> String {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The tail as a suffix for an error message, or nothing at all.
    fn suffix(&self) -> String {
        let text = self.text();
        if text.trim().is_empty() {
            String::new()
        } else {
            format!("\nserver stderr:\n{text}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_client_introduces_itself_by_name_and_claims_nothing_it_lacks() {
        let info = client_info(false);
        assert_eq!(info.client_info.name, "zlogic");
        assert!(
            info.capabilities.sampling.is_none(),
            "we do not serve sampling"
        );
        assert!(info.capabilities.roots.is_none());
    }

    #[test]
    fn header_names_are_validated_and_values_never_reach_the_error() {
        let mut h = std::collections::BTreeMap::new();
        h.insert("Authorization".to_string(), "Bearer abc".to_string());
        assert!(http_headers("s", &h).is_ok());

        let mut bad_name = std::collections::BTreeMap::new();
        bad_name.insert("not a header".to_string(), "v".to_string());
        assert!(http_headers("s", &bad_name).is_err());

        let mut bad_value = std::collections::BTreeMap::new();
        bad_value.insert("X-Token".to_string(), "line\nbreak".to_string());
        let err = http_headers("s", &bad_value).unwrap_err().to_string();
        assert!(err.contains("X-Token"));
        assert!(
            !err.contains("line"),
            "a header value can be a token: {err}"
        );
    }

    #[test]
    fn endpoint_labels_drop_every_place_a_url_may_carry_a_secret() {
        let label = endpoint_label(
            "https://user:password@mcp.example.test:8443/token/in/path?api_key=secret#fragment",
        );
        assert_eq!(label, "https://mcp.example.test:8443");
        for secret in ["user", "password", "token", "api_key", "secret", "fragment"] {
            assert!(!label.contains(secret), "{secret} leaked in {label}");
        }
    }

    #[tokio::test]
    async fn the_stderr_tail_is_bounded_and_keeps_the_end() {
        let tail = Arc::new(StderrTail::default());
        {
            let mut buf = tail.0.lock().unwrap();
            for i in 0..40 {
                if buf.len() == StderrTail::LINES {
                    buf.pop_front();
                }
                buf.push_back(format!("line {i}"));
            }
        }
        let text = tail.text();
        assert_eq!(text.lines().count(), StderrTail::LINES);
        assert!(
            text.contains("line 39"),
            "the end is what explains a failure"
        );
        assert!(!text.contains("line 0"));
        assert!(tail.suffix().contains("server stderr"));
    }

    #[test]
    fn an_empty_tail_adds_nothing_to_a_message() {
        assert!(StderrTail::default().suffix().is_empty());
    }

    /// A command that does not exist must fail as "cannot start", naming the command — the single
    /// most common MCP setup mistake there is.
    #[tokio::test]
    async fn a_missing_command_fails_by_name() {
        let transport = ResolvedTransport::Stdio {
            command: "zlogic-mcp-definitely-not-a-real-binary".into(),
            args: vec![],
            env: Default::default(),
            cwd: std::env::temp_dir(),
        };
        let label = Label {
            workspace_root: std::env::temp_dir(),
            session: None,
            turn: None,
            call: None,
            interaction: None,
        };
        let err = Connection::open("x", &transport, label, Duration::from_secs(2))
            .await
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("zlogic-mcp-definitely-not-a-real-binary"),
            "{err}"
        );
    }
}

/// Talking to a **real** MCP server, over an in-memory pipe.
/// Everything else in this crate is tested against fakes, which cannot catch the one class of
/// mistake that matters most here: an assumption about the SDK or the protocol that is simply wrong.
/// So this module implements a small server with `rmcp`'s server half and runs the real handshake,
/// `tools/list` and `tools/call` across `tokio::io::duplex` — no child process, no network, and the
/// same client code production uses.
#[cfg(test)]
mod round_trip {
    use super::*;
    use rmcp::model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool as RmcpTool,
    };
    use rmcp::service::RequestContext;
    use rmcp::{ErrorData, RoleServer, ServerHandler};
    use std::borrow::Cow;
    use std::future::Future;

    struct TinyServer;

    impl ServerHandler for TinyServer {
        fn get_info(&self) -> ServerInfo {
            let mut info = ServerInfo::default();
            info.capabilities = ServerCapabilities::builder().enable_tools().build();
            info
        }

        fn list_tools(
            &self,
            _request: Option<PaginatedRequestParams>,
            _context: RequestContext<RoleServer>,
        ) -> impl Future<Output = std::result::Result<ListToolsResult, ErrorData>> + Send + '_
        {
            let mut schema = JsonObject::new();
            schema.insert("type".into(), serde_json::json!("object"));
            schema.insert(
                "properties".into(),
                serde_json::json!({ "name": { "type": "string" } }),
            );
            let tool = RmcpTool::new(
                Cow::Borrowed("greet"),
                Cow::Borrowed("greets somebody"),
                std::sync::Arc::new(schema),
            );
            std::future::ready(Ok(ListToolsResult {
                tools: vec![tool],
                ..Default::default()
            }))
        }

        fn call_tool(
            &self,
            request: CallToolRequestParams,
            _context: RequestContext<RoleServer>,
        ) -> impl Future<Output = std::result::Result<CallToolResponse, ErrorData>> + Send + '_
        {
            let who = request
                .arguments
                .as_ref()
                .and_then(|a| a.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("nobody")
                .to_string();
            let result = match request.name.as_ref() {
                "greet" => {
                    CallToolResult::success(vec![ContentBlock::text(format!("hello {who}"))])
                }
                other => {
                    let mut failed = CallToolResult::success(vec![ContentBlock::text(format!(
                        "no tool {other}"
                    ))]);
                    failed.is_error = Some(true);
                    failed
                }
            };
            std::future::ready(Ok(CallToolResponse::Complete(result)))
        }
    }

    async fn connected() -> Connection {
        let (server_side, client_side) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            // The server's own loop. Dropping the handle would close the connection, so it is
            // awaited for as long as the client keeps it open.
            if let Ok(running) = TinyServer.serve(server_side).await {
                let _ = running.waiting().await;
            }
        });

        Connection::handshake(
            "tiny",
            Label {
                workspace_root: std::env::temp_dir(),
                session: None,
                turn: None,
                call: None,
                interaction: None,
            },
            Arc::new(StderrTail::default()),
            client_side,
            Duration::from_secs(5),
            "the test server did not answer".to_string(),
            None,
        )
        .await
        .expect("the handshake must succeed against a real server")
    }

    #[tokio::test]
    async fn the_handshake_and_the_tool_list_work_against_a_real_server() {
        let conn = connected().await;
        assert!(!conn.is_closed());

        let tools = conn.list_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "greet");

        // And the shape the registry ends up offering: this is the whole path from the wire to a
        // tool definition a provider will accept.
        let spec = crate::tool::ToolSpec::from_rmcp(&tools[0]);
        assert_eq!(spec.name, "greet");
        assert_eq!(spec.description.as_deref(), Some("greets somebody"));
        assert_eq!(spec.input_schema["type"], "object");
        assert!(spec.input_schema["properties"]["name"].is_object());
    }

    #[tokio::test]
    async fn a_call_reaches_the_server_with_its_arguments_and_comes_back_mapped() {
        let conn = connected().await;
        let mut args = JsonObject::new();
        args.insert("name".into(), serde_json::json!("zlogic"));

        let raw = conn
            .call_tool("greet", Some(args), Duration::from_secs(5))
            .await
            .unwrap();
        let out = crate::content::to_result("tiny", "greet", raw, &crate::test_ctx());
        assert_eq!(out.status, zlogic_tools::ToolExecStatus::Success);
        assert_eq!(out.model_text(), "hello zlogic");
    }

    /// A tool that ran and failed must not look like a broken connection.
    #[tokio::test]
    async fn a_tool_level_failure_is_a_result_not_an_error() {
        let conn = connected().await;
        let raw = conn
            .call_tool("nope", None, Duration::from_secs(5))
            .await
            .unwrap();
        let out = crate::content::to_result("tiny", "nope", raw, &crate::test_ctx());
        assert_eq!(out.status, zlogic_tools::ToolExecStatus::Failed);
        assert!(out.model_text().contains("no tool nope"));
    }

    /// Several calls on one connection at once — request-id multiplexing is the reason one connection
    /// can serve a whole batch of tool calls, and a whole session.
    #[tokio::test]
    async fn concurrent_calls_share_the_one_connection() {
        let conn = Arc::new(connected().await);
        let mut handles = Vec::new();
        for i in 0..8 {
            let conn = conn.clone();
            handles.push(tokio::spawn(async move {
                let mut args = JsonObject::new();
                args.insert("name".into(), serde_json::json!(format!("caller {i}")));
                conn.call_tool("greet", Some(args), Duration::from_secs(5))
                    .await
            }));
        }
        let mut seen = Vec::new();
        for handle in handles {
            let result = handle.await.unwrap().unwrap();
            seen.push(match &result.content[0] {
                ContentBlock::Text(t) => t.text.clone(),
                other => panic!("{other:?}"),
            });
        }
        seen.sort();
        // Every caller got *its own* answer: no cross-talk between concurrent requests.
        assert_eq!(seen.len(), 8);
        assert_eq!(seen[0], "hello caller 0");
        assert!(seen.iter().all(|s| s.starts_with("hello caller ")));
    }

    #[test]
    fn advertises_form_elicitation_only_when_a_ui_is_routable() {
        assert!(client_info(false).capabilities.elicitation.is_none());
        let capability = client_info(true).capabilities.elicitation.unwrap();
        assert!(capability.form.is_some());
        assert!(capability.url.is_none());
    }

    #[test]
    fn mcp_schema_becomes_an_zlogic_form() {
        let schema: rmcp::model::ElicitationSchema = serde_json::from_value(serde_json::json!({
            "type": "object",
            "title": "Deploy",
            "properties": {
                "environment": {
                    "type": "string",
                    "title": "Environment",
                    "enum": ["staging", "production"],
                    "default": "staging"
                },
                "replicas": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 10
                },
                "confirm": {
                    "type": "boolean",
                    "default": false
                }
            },
            "required": ["environment"]
        }))
        .unwrap();
        let form = elicitation_form("Choose deployment settings".into(), &schema).unwrap();
        assert_eq!(form.title, "Deploy");
        assert_eq!(form.fields.len(), 3);
        assert!(form.field("environment").unwrap().required);
        assert!(matches!(
            form.field("environment").unwrap().control,
            zlogic_protocol::Control::Select { .. }
        ));
        assert!(matches!(
            form.field("replicas").unwrap().control,
            zlogic_protocol::Control::Number { integer: true, .. }
        ));
    }
}
