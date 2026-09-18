//! Server definitions: what is written down, before anything is resolved or connected.
//! # The format is the ecosystem's, not ours
//! A definition file is the standard `{"mcpServers": {…}}` object that Claude Desktop, Claude Code
//! and VS Code all write, so a server someone already configured can be copied in verbatim. We
//! accept the two spellings that exist in the wild (`mcpServers` and VS Code's `servers`), a bare
//! single-server object, and infer the transport from `command` / `url` when `type` is absent —
//! because a definition that has to be rewritten to be understood is a definition users will get
//! wrong.
//! # A definition is data, and its identity is its location
//! The id is the key in the map (or the file stem for a single-server file), never a field inside
//! the object. Two consequences that matter: copying a file in is installing a server, and renaming
//! it is renaming the server — no index to rebuild and no metadata to keep in step. The `name`
//! field, when present, is a display label and nothing else.
//! # One bad entry is one bad entry
//! Parsing never fails a whole file because one server in it is malformed: the rest are returned
//! and the broken one comes back as a [`Problem`]. A file with three good servers and a typo in the
//! fourth has to keep working, and the user has to be told which one is wrong — "your MCP config is
//! invalid" is not something anyone can act on.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use zlogic_credential::CredentialRef;

use crate::{McpError, Result};

/// The prefix every MCP tool's name carries, so the model can never confuse one with a built-in.
pub const TOOL_PREFIX: &str = "mcp__";
/// Common denominator across the model providers zlogic supports.
const MAX_TOOL_NAME_CHARS: usize = 64;
const TOOL_NAME_HASH_CHARS: usize = 12;

/// Names the model sees: `mcp__<server>__<tool>`.
/// Both segments are sanitised because providers restrict tool names to `[A-Za-z0-9_-]`. A changed
/// segment gets a short hash of its original value, so `a.b` and `a_b` cannot silently collapse to
/// the same registered tool. The final name is capped at 64 characters; long names keep readable
/// prefixes plus a hash of the full server/tool pair.
pub fn tool_name(server_id: &str, tool: &str) -> String {
    let server = sanitize_segment(server_id);
    let tool = sanitize_segment(tool);
    let candidate = format!("{TOOL_PREFIX}{}__{}", server, tool);
    if candidate.chars().count() <= MAX_TOOL_NAME_CHARS {
        return candidate;
    }

    let pair_hash = short_hash(&format!("{server_id}\0{tool}"));
    let fixed = TOOL_PREFIX.len() + 2 + 1 + TOOL_NAME_HASH_CHARS;
    let available = MAX_TOOL_NAME_CHARS - fixed;
    // Preserve both identities. The tool gets the odd character because it is the action the model
    // is choosing; the server prefix is primarily a namespace.
    let server_budget = (available / 3).max(1);
    let tool_budget = available - server_budget;
    format!(
        "{TOOL_PREFIX}{}__{}_{}",
        take_ascii(&server, server_budget),
        take_ascii(&tool, tool_budget),
        pair_hash
    )
}

fn sanitize_segment(value: &str) -> String {
    let mapped: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = mapped.trim_matches('_');
    let base = if trimmed.is_empty() { "tool" } else { trimmed };
    if base == value {
        base.to_string()
    } else {
        format!("{base}_{}", short_hash(value))
    }
}

fn short_hash(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    digest[..TOOL_NAME_HASH_CHARS / 2]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn take_ascii(value: &str, chars: usize) -> &str {
    // The callers only ever pass ASCII (post-`sanitize_segment`), but a byte cap is
    // still not a license to slice through a multi-byte character: walk back to a
    // char boundary so a non-ASCII name degrades to a shorter prefix, never a panic.
    let mut end = value.len().min(chars);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

/// Canonical OS-keychain entry for a server's manually supplied bearer token.
pub fn token_key(server_id: &str) -> String {
    format!("mcp_{}_token", sanitize_segment(server_id))
}

/// Canonical OS-keychain entry for a server's manually supplied API key.
pub fn api_key_key(server_id: &str) -> String {
    format!("mcp_{}_api_key", sanitize_segment(server_id))
}

/// Canonical OS-keychain entry for a pre-registered OAuth client's secret.
pub fn oauth_client_secret_key(server_id: &str) -> String {
    format!("mcp_{}_oauth_client_secret", sanitize_segment(server_id))
}

/// Where a definition was found. **Derived from the location, never declared in the file.**
/// It is what tells "the user installed this for themselves" apart from "this arrived with the
/// repository" — the second one is code somebody else chose, on a checkout the user may only have
/// cloned, and it is the distinction a trust prompt has to be able to make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// The user's own install, outside any repository.
    Global,
    /// Inside the workspace — committed, shared with whoever clones it.
    Workspace,
    /// Contributed by a plugin manifest. `scope` is the plugin's own origin.
    Plugin { plugin: String, workspace: bool },
}

impl Origin {
    /// Whether the definition came from inside the repository.
    pub fn is_from_workspace(&self) -> bool {
        match self {
            Origin::Global => false,
            Origin::Workspace => true,
            Origin::Plugin { workspace, .. } => *workspace,
        }
    }
}

/// How much a connection may be shared.
/// **Not a scope declaration.** Whether two workspaces share a connection follows from the resolved
/// launch parameters (see [`crate::resolve`]) — a server whose args mention the workspace root
/// splits by itself, one whose parameters are identical everywhere is shared by itself. This field
/// exists only for the case parameters cannot express: a server that keeps state *inside* the tools
/// it offers, a browser being the standing example. MCP has no capability flag for that, so it is
/// the one thing left to declare by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Binding {
    /// Shared by anyone whose resolved parameters hash the same.
    #[default]
    Params,
    /// Additionally keyed by session, so two conversations never share tool-level state.
    Session,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportDef {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        /// `None` = the workspace root. See [`crate::resolve`] for why the default is not the
        /// process's own directory.
        cwd: Option<String>,
    },
    Http {
        url: String,
        headers: BTreeMap<String, String>,
        auth: Option<HttpAuthDef>,
    },
}

/// Authentication declared for a streamable HTTP server.
/// The config contains only a [`CredentialRef`]; a literal token is not representable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpAuthDef {
    Bearer {
        credential: CredentialRef,
    },
    OAuth {
        client_id: Option<String>,
        client_secret: Option<CredentialRef>,
        scopes: Vec<String>,
    },
}

impl TransportDef {
    pub fn is_stdio(&self) -> bool {
        matches!(self, TransportDef::Stdio { .. })
    }
}

/// Offering only some of a server's tools.
/// # Why this is a field in the definition rather than a mechanism somewhere else
/// A filesystem server offers a couple of dozen tools, most of which a given project never needs; an
/// OpenAPI-derived one can offer hundreds. A tool definition is **tokens paid on every single round**,
/// so "just these three" has to be expressible — and expressible in the definition file, because that
/// is the thing that travels with the install, can be committed, and can be shared with a team.
/// Empty = everything. With both set, `exclude` is applied second (pick, then remove).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolFilter {
    /// The allowlist. When non-empty, **only** the tools named here are offered.
    pub include: Vec<String>,
    /// The denylist. Always applied.
    pub exclude: Vec<String>,
}

impl ToolFilter {
    pub fn is_empty(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty()
    }

    pub fn allows(&self, tool: &str) -> bool {
        if self.exclude.iter().any(|e| e == tool) {
            return false;
        }
        self.include.is_empty() || self.include.iter().any(|i| i == tool)
    }

    pub fn missing<'a>(&'a self, available: &[String]) -> Vec<&'a str> {
        self.include
            .iter()
            .filter(|want| !available.iter().any(|have| have == *want))
            .map(String::as_str)
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerDef {
    /// The map key or file stem. Appears in every tool name this server contributes.
    pub id: String,
    /// Display only.
    pub label: Option<String>,
    /// `None` is enabled: a definition that exists and says nothing is one the user put there.
    pub enabled: Option<bool>,
    pub transport: TransportDef,
    pub binding: Binding,
    /// Offering only some of the server's tools. See [`ToolFilter`].
    pub filter: ToolFilter,
    pub origin: Origin,
    /// The file it was read from, for error messages. `None` for definitions built in memory.
    pub file: Option<PathBuf>,
}

impl ServerDef {
    pub fn is_enabled(&self) -> bool {
        self.enabled != Some(false)
    }

    /// A stable hash of everything that determines what this server *is*.
    /// It keys the cached tool list, so editing a definition invalidates the cache rather than
    /// leaving the model offered tools the new command does not have. `origin`, `label` and the file
    /// path are deliberately out: moving a definition between directories does not change the tools.
    /// [`ServerDef::filter`] is out for the same reason, and it matters more: the filter says which
    /// of the server's tools *we* want, not what the server has. Including it would re-fetch the whole
    /// list every time somebody edited an allowlist — a connection paid for a decision that changes
    /// nothing about the server.
    pub fn fingerprint(&self) -> String {
        let mut w = Fields::new();
        w.field("id", &self.id);
        match &self.transport {
            TransportDef::Stdio {
                command,
                args,
                env,
                cwd,
            } => {
                w.field("kind", "stdio");
                w.field("command", command);
                for a in args {
                    w.field("arg", a);
                }
                for (k, v) in env {
                    w.field("env", &format!("{k}={v}"));
                }
                w.field("cwd", cwd.as_deref().unwrap_or(""));
            }
            TransportDef::Http { url, headers, auth } => {
                w.field("kind", "http");
                w.field("url", url);
                for (k, v) in headers {
                    w.field("header", &format!("{k}={v}"));
                }
                if let Some(HttpAuthDef::Bearer { credential }) = auth {
                    w.field("auth", &format!("bearer:{credential}"));
                } else if let Some(HttpAuthDef::OAuth {
                    client_id,
                    client_secret,
                    scopes,
                }) = auth
                {
                    w.field("auth", "oauth");
                    w.field("oauth_client_id", client_id.as_deref().unwrap_or(""));
                    let client_secret = client_secret.as_ref().map(ToString::to_string);
                    w.field(
                        "oauth_client_secret",
                        client_secret.as_deref().unwrap_or(""),
                    );
                    for scope in scopes {
                        w.field("oauth_scope", scope);
                    }
                }
            }
        }
        w.field(
            "binding",
            if self.binding == Binding::Session {
                "session"
            } else {
                "params"
            },
        );
        w.finish()
    }

    /// What this definition can do, **derived from the definition and nothing else**.
    /// A trust prompt that asks "do you trust this?" is a question nobody can answer. What can be
    /// answered is "this will start `npx …` on your machine and hand it your `GITHUB_TOKEN`" — so the
    /// disclosure is computed, and computed *statically*: showing what an extension does must not
    /// require running it first.
    pub fn capabilities(&self) -> Capabilities {
        match &self.transport {
            TransportDef::Stdio { command, .. } => Capabilities {
                spawns: Some(command.clone()),
                // A local process can open any socket it likes. Claiming "no network" because no URL
                // appears in the definition would be a disclosure that is simply false.
                network: Reach::Unknown,
                secrets: self.referenced_credentials(),
                from_workspace: self.origin.is_from_workspace(),
            },
            TransportDef::Http { url, .. } => Capabilities {
                spawns: None,
                network: match host_of(url) {
                    Some(host) => Reach::Host(host),
                    // A URL that is still a template resolves to somewhere we cannot name yet.
                    None => Reach::Unknown,
                },
                secrets: self.referenced_credentials(),
                from_workspace: self.origin.is_from_workspace(),
            },
        }
    }

    /// The credential placeholders this definition mentions, in the order they appear.
    /// Used to explain a failure: "this server needs `GITHUB_TOKEN`" is actionable, whereas the
    /// underlying "template `${env:GITHUB_TOKEN}` could not be resolved" reads like a bug in zlogic.
    pub fn referenced_credentials(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut add = |s: &str| {
            for placeholder in placeholders(s) {
                if (placeholder.starts_with("env:") || placeholder.starts_with("keyring:"))
                    && !out.contains(&placeholder)
                {
                    out.push(placeholder);
                }
            }
        };
        match &self.transport {
            TransportDef::Stdio {
                command,
                args,
                env,
                cwd,
            } => {
                add(command);
                args.iter().for_each(|a| add(a));
                env.values().for_each(|v| add(v));
                if let Some(c) = cwd {
                    add(c);
                }
            }
            TransportDef::Http { url, headers, auth } => {
                add(url);
                headers.values().for_each(|v| add(v));
                if let Some(HttpAuthDef::Bearer { credential }) = auth {
                    let reference = credential.to_string();
                    if !out.contains(&reference) {
                        out.push(reference);
                    }
                } else if let Some(HttpAuthDef::OAuth {
                    client_secret: Some(credential),
                    ..
                }) = auth
                {
                    let reference = credential.to_string();
                    if !out.contains(&reference) {
                        out.push(reference);
                    }
                }
            }
        }
        out
    }
}

/// What a definition discloses about itself. Derived, never self-declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// The command that will be started on this machine, for a stdio server.
    pub spawns: Option<String>,
    pub network: Reach,
    /// The credential references it will be handed (`env:GITHUB_TOKEN`, `keyring:…`).
    pub secrets: Vec<String>,
    /// Whether the definition arrived with the repository — which is to say, whether the user chose
    /// it or merely cloned it.
    pub from_workspace: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reach {
    /// The definition names where it connects.
    Host(String),
    /// Not decidable from the definition: a local process can open any socket, and a templated URL is
    /// not resolvable until it is expanded.
    Unknown,
}

impl Capabilities {
    /// Whether this is the combination that deserves a second look: something the user did not
    /// install, that starts a process, and that is handed credentials.
    /// Not a veto — a repository shipping a server that needs a token is completely ordinary. It is
    /// what a prompt should put in front of the user's eyes rather than three lines down.
    pub fn is_high_risk(&self) -> bool {
        self.from_workspace && self.spawns.is_some() && !self.secrets.is_empty()
    }
}

/// The host part of a URL, without pulling in a URL parser for one field.
/// Deliberately forgiving: this string only ever goes into a disclosure, so a URL exotic enough to
/// confuse this is one we say `Unknown` about rather than one we misreport.
fn host_of(url: &str) -> Option<String> {
    let after_scheme = url.split_once("://")?.1;
    let authority = after_scheme.split(['/', '?', '#']).next()?;
    let host = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    let host = host.trim();
    if host.is_empty() || host.contains("${") {
        None
    } else {
        Some(host.to_string())
    }
}

/// Every `${…}` in a string, without resolving anything.
pub(crate) fn placeholders(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'$' && bytes[i + 1] == b'{' {
            if let Some(end) = raw[i + 2..].find('}') {
                out.push(raw[i + 2..i + 2 + end].to_string());
                i += 2 + end + 1;
                continue;
            }
            break;
        }
        i += 1;
    }
    out
}

/// One definition that could not be used, and why.
/// Carried rather than logged: a server the user configured and cannot see in the tool list has to
/// be explainable in the UI, and "check the logs" is not an explanation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// `None` when the failure is the file itself rather than one server in it.
    pub server: Option<String>,
    pub file: Option<PathBuf>,
    pub reason: String,
}

impl Problem {
    pub fn new(server: Option<String>, file: Option<PathBuf>, reason: impl Into<String>) -> Self {
        Self {
            server,
            file,
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (&self.server, &self.file) {
            (Some(s), Some(p)) => write!(f, "MCP server `{s}` ({}): {}", p.display(), self.reason),
            (Some(s), None) => write!(f, "MCP server `{s}`: {}", self.reason),
            (None, Some(p)) => write!(f, "{}: {}", p.display(), self.reason),
            (None, None) => f.write_str(&self.reason),
        }
    }
}

#[derive(Debug, Default)]
pub struct Parsed {
    pub servers: Vec<ServerDef>,
    pub problems: Vec<Problem>,
}

/// Reads one definition file.
/// A file that is not valid JSON is a single [`McpError`] — there is nothing to salvage from it.
/// Everything after that point degrades per server.
pub fn parse_file(path: &Path, origin: Origin) -> Result<Parsed> {
    let raw = std::fs::read_to_string(path)?;
    let value: Value = serde_json::from_str(&raw).map_err(|e| McpError::Parse {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("mcp");
    Ok(parse_value(&value, stem, origin, Some(path.to_path_buf())))
}

/// Parses an already-decoded definition object. YAML manifests come through here too — the plugin
/// loader converts them to [`Value`] first, so there is one parser and not one per file format.
/// `default_id` is used only for a bare single-server object.
pub fn parse_value(
    value: &Value,
    default_id: &str,
    origin: Origin,
    file: Option<PathBuf>,
) -> Parsed {
    let mut out = Parsed::default();

    // `mcpServers` is the de-facto standard; `servers` is VS Code's spelling.
    let map = value
        .get("mcpServers")
        .or_else(|| value.get("servers"))
        .or_else(|| value.get("mcp").and_then(|m| m.get("servers")))
        .and_then(|v| v.as_object());

    match map {
        Some(map) => {
            for (id, raw) in map {
                match parse_server(id, raw, &origin, file.clone()) {
                    Ok(def) => out.servers.push(def),
                    Err(reason) => {
                        out.problems
                            .push(Problem::new(Some(id.clone()), file.clone(), reason))
                    }
                }
            }
        }
        // A bare single-server object: `{"command": "npx", …}` in `<id>.json`.
        None if value.get("command").is_some() || value.get("url").is_some() => {
            match parse_server(default_id, value, &origin, file.clone()) {
                Ok(def) => out.servers.push(def),
                Err(reason) => out.problems.push(Problem::new(
                    Some(default_id.to_string()),
                    file.clone(),
                    reason,
                )),
            }
        }
        None => out.problems.push(Problem::new(
            None,
            file,
            "no `mcpServers` object and no `command` / `url` — nothing here defines a server",
        )),
    }
    out
}

fn parse_server(
    id: &str,
    raw: &Value,
    origin: &Origin,
    file: Option<PathBuf>,
) -> std::result::Result<ServerDef, String> {
    if id.trim().is_empty() {
        return Err("the server id is empty".into());
    }
    let obj = raw.as_object().ok_or("expected an object")?;
    // Some writers nest the transport, most inline it. Look in the nested object first and fall
    // back to the outer one, so both shapes parse without asking the user which they wrote.
    let inner = obj.get("transport").and_then(|t| t.as_object());
    let get = |key: &str| inner.and_then(|i| i.get(key)).or_else(|| obj.get(key));

    let declared = get("type")
        .and_then(|v| v.as_str())
        .map(|s| s.to_ascii_lowercase());
    let kind = match declared.as_deref() {
        Some("stdio" | "local") => "stdio",
        Some("http" | "streamable-http" | "streamablehttp" | "streamable_http" | "remote") => {
            "http"
        }
        // The 2024 SSE transport is a different protocol, not a variant of this one, and we do not
        // speak it. Saying so names the fix; inferring "close enough to HTTP" would produce a
        // handshake that fails with something unrelated.
        Some("sse") => {
            return Err(
                "the legacy SSE transport is not supported — use a streamable HTTP endpoint \
                        (`\"type\": \"http\"`) or run the server over stdio"
                    .into(),
            );
        }
        Some(other) => return Err(format!("unknown transport type `{other}`")),
        None if get("command").is_some() => "stdio",
        None if get("url").is_some() => "http",
        None => return Err("neither `command` (stdio) nor `url` (http) is set".into()),
    };

    let transport = if kind == "stdio" {
        let command = get("command")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .filter(|c| !c.trim().is_empty())
            .ok_or("`command` is required for a stdio server")?;
        TransportDef::Stdio {
            command,
            args: string_list(get("args")),
            env: string_map(get("env")),
            cwd: get("cwd").and_then(|v| v.as_str()).map(str::to_string),
        }
    } else {
        let url = get("url")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .filter(|u| !u.trim().is_empty())
            .ok_or("`url` is required for an http server")?;
        // Checked here rather than at connect time: a `ws://` or a bare host is a mistake in the
        // file, and the file is what the user can fix.
        if !(url.starts_with("http://") || url.starts_with("https://") || url.contains("${")) {
            return Err(format!("`url` must be http(s): {url}"));
        }
        let headers = string_map(get("headers"));
        let auth = match get("auth") {
            None => None,
            Some(Value::Object(auth)) => {
                let kind = auth
                    .get("type")
                    .and_then(Value::as_str)
                    .ok_or("`auth.type` is required")?;
                if headers
                    .keys()
                    .any(|name| name.eq_ignore_ascii_case("authorization"))
                {
                    return Err(
                        "`auth` and an `Authorization` header cannot both be configured".into(),
                    );
                }
                match kind {
                    "bearer" => {
                        let raw = auth
                            .get("credential")
                            .and_then(Value::as_str)
                            .ok_or("`auth.credential` is required for bearer auth")?;
                        let credential = raw.parse::<CredentialRef>().map_err(|e| e.to_string())?;
                        Some(HttpAuthDef::Bearer { credential })
                    }
                    "oauth" => {
                        if !url.contains("${") && !is_secure_oauth_endpoint(&url) {
                            return Err(
                                "an OAuth MCP endpoint must use HTTPS (HTTP is allowed only for \
                                 localhost/loopback development)"
                                    .into(),
                            );
                        }
                        let client_id = auth
                            .get("client_id")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                            .map(str::to_string);
                        let client_secret = auth
                            .get("client_secret")
                            .and_then(Value::as_str)
                            .map(|raw| raw.parse::<CredentialRef>().map_err(|e| e.to_string()))
                            .transpose()?;
                        if client_secret.is_some() && client_id.is_none() {
                            return Err(
                                "`auth.client_secret` requires `auth.client_id` for OAuth".into()
                            );
                        }
                        Some(HttpAuthDef::OAuth {
                            client_id,
                            client_secret,
                            scopes: string_list(auth.get("scopes")),
                        })
                    }
                    other => return Err(format!("unsupported MCP auth type `{other}`")),
                }
            }
            Some(_) => return Err("`auth` must be an object".into()),
        };
        TransportDef::Http { url, headers, auth }
    };

    let filter = ToolFilter {
        // `tools` is the allowlist; `exclude` / `excludeTools` / `exclude_tools` the denylist — all
        // three spellings appear in the wild.
        include: string_list(obj.get("tools")),
        exclude: {
            let mut out = string_list(obj.get("exclude"));
            out.extend(string_list(obj.get("excludeTools")));
            out.extend(string_list(obj.get("exclude_tools")));
            out
        },
    };

    Ok(ServerDef {
        id: id.to_string(),
        label: obj.get("name").and_then(|v| v.as_str()).map(str::to_string),
        enabled: obj.get("enabled").and_then(|v| v.as_bool()),
        transport,
        binding: match obj.get("binding").and_then(|v| v.as_str()) {
            Some("session") => Binding::Session,
            _ => Binding::Params,
        },
        filter,
        origin: origin.clone(),
        file,
    })
}

fn is_secure_oauth_endpoint(raw: &str) -> bool {
    let Ok(url) = url::Url::parse(raw) else {
        return false;
    };
    if url.scheme() == "https" {
        return true;
    }
    url.scheme() == "http"
        && url.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|address| address.is_loopback())
        })
}

/// Scalars are accepted where strings are expected: a port written as `8080` rather than `"8080"`
/// is a mistake nobody should have to debug through a handshake failure.
fn scalar(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn string_list(v: Option<&Value>) -> Vec<String> {
    v.and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(scalar).collect())
        .unwrap_or_default()
}

fn string_map(v: Option<&Value>) -> BTreeMap<String, String> {
    v.and_then(|v| v.as_object())
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| scalar(v).map(|v| (k.clone(), v)))
                .collect()
        })
        .unwrap_or_default()
}

/// Length-prefixed field writer, for hashes that must not be forgeable by content.
/// `command=a args=[bc]` and `command=ab args=[c]` concatenate to the same bytes without the
/// lengths, and two definitions that hash the same would share a cached tool list that belongs to
/// neither.
pub(crate) struct Fields(String);

impl Fields {
    pub(crate) fn new() -> Self {
        Self(String::new())
    }

    pub(crate) fn field(&mut self, key: &str, value: &str) {
        self.0.push_str(key);
        self.0.push(':');
        self.0.push_str(&value.len().to_string());
        self.0.push(':');
        self.0.push_str(value);
        self.0.push(';');
    }

    pub(crate) fn finish(self) -> String {
        let digest = Sha256::digest(self.0.as_bytes());
        // Half of a SHA-256 is 64 bits of collision resistance against accident, which is what this
        // guards against — it is not a security boundary, and a 64-character file name is worse to
        // read in a cache directory.
        digest[..16].iter().map(|b| format!("{b:02x}")).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(v: Value) -> Parsed {
        parse_value(&v, "fallback", Origin::Global, None)
    }

    /// `take_ascii` must not slice through a multi-byte character if it ever receives
    /// non-ASCII input; it degrades to a shorter prefix instead of panicking.
    #[test]
    fn take_ascii_never_slices_mid_character() {
        let s = "天气服务工具";
        // 5 is a byte count, not a char count: byte 5 lies inside the second character.
        assert_eq!(take_ascii(s, 5), "天");
        // ASCII input is untouched.
        assert_eq!(take_ascii("abcde", 3), "abc");
        // The whole string when within budget.
        assert_eq!(take_ascii("abc", 10), "abc");
    }

    #[test]
    fn the_standard_shape_parses() {
        let p = parse(json!({
            "mcpServers": {
                "github": { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"],
                            "env": { "GITHUB_TOKEN": "${env:GH}" } }
            }
        }));
        assert!(p.problems.is_empty());
        let s = &p.servers[0];
        assert_eq!(s.id, "github");
        assert!(s.is_enabled(), "a definition that says nothing is enabled");
        match &s.transport {
            TransportDef::Stdio {
                command,
                args,
                env,
                cwd,
            } => {
                assert_eq!(command, "npx");
                assert_eq!(args.len(), 2);
                assert_eq!(env["GITHUB_TOKEN"], "${env:GH}");
                assert!(
                    cwd.is_none(),
                    "the default is the workspace root, decided at resolve time"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    /// VS Code writes `servers`; the same file must not need editing to work here.
    #[test]
    fn the_vscode_spelling_parses_too() {
        let p = parse(json!({ "servers": { "x": { "command": "run" } } }));
        assert_eq!(p.servers.len(), 1);
    }

    #[test]
    fn a_bare_single_server_object_takes_the_file_name_as_its_id() {
        let p = parse(json!({ "command": "run" }));
        assert_eq!(p.servers[0].id, "fallback");
    }

    #[test]
    fn a_nested_transport_object_parses() {
        let p = parse(json!({
            "mcpServers": { "x": { "transport": { "type": "http", "url": "https://h/mcp" } } }
        }));
        assert!(matches!(p.servers[0].transport, TransportDef::Http { .. }));
    }

    #[test]
    fn bearer_auth_accepts_only_a_credential_reference() {
        let p = parse(json!({
            "mcpServers": {
                "github": {
                    "url": "https://mcp.example.test",
                    "auth": {
                        "type": "bearer",
                        "credential": "keyring:mcp_github_token"
                    }
                }
            }
        }));
        assert!(p.problems.is_empty(), "{:?}", p.problems);
        assert!(matches!(
            &p.servers[0].transport,
            TransportDef::Http {
                auth: Some(HttpAuthDef::Bearer {
                    credential: CredentialRef::Keyring(name)
                }),
                ..
            } if name == "mcp_github_token"
        ));

        let rejected = parse(json!({
            "mcpServers": {
                "github": {
                    "url": "https://mcp.example.test",
                    "auth": { "type": "bearer", "credential": "literal-secret" }
                }
            }
        }));
        assert!(rejected.servers.is_empty());
        let message = rejected.problems[0].to_string();
        assert!(message.contains("env:") && message.contains("keyring:"));
        assert!(!message.contains("literal-secret"));
    }

    #[test]
    fn auth_and_an_authorization_header_are_not_ambiguous() {
        let p = parse(json!({
            "mcpServers": {
                "x": {
                    "url": "https://mcp.example.test",
                    "headers": { "authorization": "Bearer ${env:TOKEN}" },
                    "auth": { "type": "bearer", "credential": "env:TOKEN" }
                }
            }
        }));
        assert!(p.servers.is_empty());
        assert!(p.problems[0].to_string().contains("cannot both"));
    }

    /// The transport is inferred, because most files in the wild do not say.
    #[test]
    fn the_transport_is_inferred_from_the_fields() {
        let p = parse(json!({
            "mcpServers": { "a": { "command": "x" }, "b": { "url": "https://h/mcp" } }
        }));
        assert!(
            p.servers
                .iter()
                .find(|s| s.id == "a")
                .unwrap()
                .transport
                .is_stdio()
        );
        assert!(
            !p.servers
                .iter()
                .find(|s| s.id == "b")
                .unwrap()
                .transport
                .is_stdio()
        );
    }

    /// One broken entry must not take the good ones down with it.
    #[test]
    fn a_malformed_server_is_reported_and_the_rest_survive() {
        let p = parse(json!({
            "mcpServers": {
                "good": { "command": "x" },
                "empty": {},
                "bad-url": { "url": "ftp://nope" }
            }
        }));
        assert_eq!(p.servers.len(), 1);
        assert_eq!(p.servers[0].id, "good");
        assert_eq!(p.problems.len(), 2);
        assert!(
            p.problems
                .iter()
                .any(|pr| pr.server.as_deref() == Some("empty"))
        );
    }

    /// Naming the unsupported transport is the whole value of the message: a user with an `/sse`
    /// endpoint has to know it is the transport and not their token.
    #[test]
    fn the_legacy_sse_transport_is_refused_by_name() {
        let p = parse(json!({ "mcpServers": { "x": { "type": "sse", "url": "https://h/sse" } } }));
        assert!(p.servers.is_empty());
        assert!(
            p.problems[0].reason.contains("SSE"),
            "{}",
            p.problems[0].reason
        );
    }

    #[test]
    fn a_file_with_nothing_in_it_is_one_problem_not_zero() {
        let p = parse(json!({ "unrelated": true }));
        assert!(p.servers.is_empty());
        assert_eq!(p.problems.len(), 1);
        assert!(p.problems[0].server.is_none());
    }

    #[test]
    fn disabled_is_explicit_and_only_false_counts() {
        let p = parse(json!({
            "mcpServers": { "off": { "command": "x", "enabled": false },
                            "on": { "command": "x", "enabled": true } }
        }));
        let off = p.servers.iter().find(|s| s.id == "off").unwrap();
        let on = p.servers.iter().find(|s| s.id == "on").unwrap();
        assert!(!off.is_enabled());
        assert!(on.is_enabled());
    }

    #[test]
    fn scalars_are_accepted_where_strings_belong() {
        let p = parse(json!({
            "mcpServers": { "x": { "command": "x", "args": [8080, true], "env": { "N": 3 } } }
        }));
        match &p.servers[0].transport {
            TransportDef::Stdio { args, env, .. } => {
                assert_eq!(args, &["8080", "true"]);
                assert_eq!(env["N"], "3");
            }
            other => panic!("{other:?}"),
        }
    }

    /// A server offers two dozen tools and the project wants three: that has to be sayable.
    #[test]
    fn a_definition_can_name_the_tools_it_wants() {
        let p = parse(json!({
            "mcpServers": { "fs": { "command": "x", "tools": ["read_file", "list_dir"],
                                    "exclude": ["list_dir"] } }
        }));
        let filter = &p.servers[0].filter;
        assert!(filter.allows("read_file"));
        assert!(!filter.allows("write_file"), "outside the allowlist");
        assert!(!filter.allows("list_dir"), "the denylist is applied second");
        assert!(filter.missing(&["read_file".into()]).contains(&"list_dir"));
    }

    #[test]
    fn no_filter_allows_everything() {
        let p = parse(json!({ "mcpServers": { "fs": { "command": "x" } } }));
        let filter = &p.servers[0].filter;
        assert!(filter.is_empty());
        assert!(filter.allows("anything"));
        assert!(filter.missing(&[]).is_empty());
    }

    #[test]
    fn both_spellings_of_the_blacklist_are_accepted() {
        for key in ["exclude", "excludeTools", "exclude_tools"] {
            let p = parse(json!({ "mcpServers": { "s": { "command": "x", key: ["nope"] } } }));
            assert!(!p.servers[0].filter.allows("nope"), "{key}");
        }
    }

    /// Editing an allowlist must not re-fetch the whole list: it says which ones we want, not which
    /// ones the server has.
    #[test]
    fn the_filter_is_not_part_of_the_fingerprint() {
        let bare = parse(json!({ "mcpServers": { "s": { "command": "x" } } }));
        let filtered = parse(json!({ "mcpServers": { "s": { "command": "x", "tools": ["a"] } } }));
        assert_eq!(
            bare.servers[0].fingerprint(),
            filtered.servers[0].fingerprint()
        );
    }

    #[test]
    fn tool_names_are_namespaced_and_sanitised() {
        assert_eq!(
            tool_name("github", "create_issue"),
            "mcp__github__create_issue"
        );
        let dotted = tool_name("my.plugin.fs", "read");
        assert!(dotted.starts_with("mcp__my_plugin_fs_"), "{dotted}");
        assert_ne!(
            tool_name("a.b", "read"),
            tool_name("a_b", "read"),
            "sanitising must not silently merge two servers"
        );
        assert_ne!(
            tool_name("s", "a.b"),
            tool_name("s", "a_b"),
            "sanitising must not silently merge two tools"
        );
        assert!(
            tool_name(&"server".repeat(30), &"tool".repeat(30))
                .chars()
                .count()
                <= MAX_TOOL_NAME_CHARS
        );
        assert!(tool_name("a", "b").starts_with(TOOL_PREFIX));
    }

    /// The fingerprint keys the cached tool list, so it must move when the command does and stay
    /// put when only the label does.
    #[test]
    fn the_fingerprint_tracks_what_the_server_is_not_how_it_is_described() {
        let base = parse(json!({ "mcpServers": { "x": { "command": "a", "args": ["1"] } } }));
        let same = parse(
            json!({ "mcpServers": { "x": { "command": "a", "args": ["1"],
                                                        "name": "Pretty" } } }),
        );
        let moved = parse(json!({ "mcpServers": { "x": { "command": "a", "args": ["2"] } } }));

        assert_eq!(base.servers[0].fingerprint(), same.servers[0].fingerprint());
        assert_ne!(
            base.servers[0].fingerprint(),
            moved.servers[0].fingerprint()
        );
    }

    /// Length prefixing: without it, moving a character between two fields would not move the hash.
    #[test]
    fn field_boundaries_are_part_of_the_hash() {
        let mut a = Fields::new();
        a.field("command", "ab");
        a.field("arg", "c");
        let mut b = Fields::new();
        b.field("command", "a");
        b.field("arg", "bc");
        assert_ne!(a.finish(), b.finish());
    }

    #[test]
    fn referenced_credentials_are_listed_for_the_error_message() {
        let p = parse(json!({
            "mcpServers": { "x": { "url": "https://h/mcp",
                                   "headers": { "Authorization": "Bearer ${keyring:mcp/x/token}",
                                                "X-Tenant": "${env:TENANT}" } } }
        }));
        let refs = p.servers[0].referenced_credentials();
        assert!(
            refs.contains(&"keyring:mcp/x/token".to_string()),
            "{refs:?}"
        );
        assert!(refs.contains(&"env:TENANT".to_string()), "{refs:?}");
    }

    /// The disclosure has to name the command, because "do you trust this extension?" is not a
    /// question anybody can answer.
    #[test]
    fn a_stdio_definition_discloses_the_command_and_never_claims_no_network() {
        let mut p = parse(json!({
            "mcpServers": { "x": { "command": "npx", "args": ["-y", "srv"],
                                   "env": { "T": "${env:GITHUB_TOKEN}" } } }
        }));
        p.servers[0].origin = Origin::Workspace;
        let caps = p.servers[0].capabilities();

        assert_eq!(caps.spawns.as_deref(), Some("npx"));
        assert_eq!(
            caps.network,
            Reach::Unknown,
            "a local process can reach anywhere"
        );
        assert_eq!(caps.secrets, ["env:GITHUB_TOKEN"]);
        assert!(caps.from_workspace);
        assert!(
            caps.is_high_risk(),
            "from the repository, starts a process, gets a credential"
        );
    }

    #[test]
    fn a_remote_definition_discloses_the_host_it_reaches() {
        let p = parse(json!({
            "mcpServers": { "x": { "url": "https://mcp.example.test:8443/v1/mcp" } }
        }));
        let caps = p.servers[0].capabilities();
        assert!(caps.spawns.is_none());
        assert_eq!(caps.network, Reach::Host("mcp.example.test:8443".into()));
        assert!(
            !caps.is_high_risk(),
            "globally installed, no process, no credentials"
        );
    }

    /// A URL that is still a template cannot be reported as a host — saying `${env:HOST}` reaches
    /// `${env:HOST}` is worse than saying we do not know.
    #[test]
    fn a_templated_url_reaches_somewhere_we_will_not_name() {
        let p = parse(json!({ "mcpServers": { "x": { "url": "https://${env:HOST}/mcp" } } }));
        assert_eq!(p.servers[0].capabilities().network, Reach::Unknown);
    }

    #[test]
    fn the_host_extraction_survives_the_shapes_urls_come_in() {
        assert_eq!(host_of("https://h/mcp").as_deref(), Some("h"));
        assert_eq!(
            host_of("http://user:pw@h:1/mcp?a=b").as_deref(),
            Some("h:1")
        );
        assert_eq!(host_of("https://h").as_deref(), Some("h"));
        assert_eq!(host_of("not a url"), None);
        assert_eq!(host_of("https://"), None);
    }

    #[test]
    fn placeholders_are_found_without_being_resolved() {
        assert_eq!(placeholders("a${x}b${y:-1}"), ["x", "y:-1"]);
        assert!(placeholders("${unclosed").is_empty());
        assert!(placeholders("no vars").is_empty());
    }

    /// Where a definition came from is a property of the directory it was in, and a plugin inside a
    /// repository is inside the repository.
    #[test]
    fn workspace_origin_is_transitive_through_plugins() {
        assert!(!Origin::Global.is_from_workspace());
        assert!(Origin::Workspace.is_from_workspace());
        assert!(
            Origin::Plugin {
                plugin: "p".into(),
                workspace: true
            }
            .is_from_workspace()
        );
        assert!(
            !Origin::Plugin {
                plugin: "p".into(),
                workspace: false
            }
            .is_from_workspace()
        );
    }

    #[test]
    fn a_file_that_is_not_json_is_a_single_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(matches!(
            parse_file(&path, Origin::Global),
            Err(McpError::Parse { .. })
        ));
    }

    #[test]
    fn a_file_is_read_and_keeps_its_path_for_the_error_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fs.json");
        std::fs::write(&path, r#"{"command":"run","args":[]}"#).unwrap();
        let p = parse_file(&path, Origin::Workspace).unwrap();
        assert_eq!(p.servers[0].id, "fs", "the file stem is the id");
        assert_eq!(p.servers[0].file.as_deref(), Some(path.as_path()));
        assert_eq!(p.servers[0].origin, Origin::Workspace);
    }
}
