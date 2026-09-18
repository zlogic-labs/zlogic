//! `web_search` — ask a search backend, get text a model can read.
//! # It is a **remote MCP call over plain HTTP**, not an MCP client
//! Both backends (Exa, Parallel) expose their search as one MCP tool at a fixed URL, and the whole
//! protocol we need is a single JSON-RPC `tools/call` POST. So there is no MCP session here: no
//! `initialize` handshake, no capability negotiation, no long-lived transport. This is the same
//! shape opencode uses, and it is what keeps the tool a hundred lines instead of a subsystem.
//! The response may come back as **either** a JSON body or an SSE stream carrying the same JSON —
//! the endpoints choose, based on an `Accept` header that asks for both. `extract_mcp_text` handles
//! both rather than pinning one, because which one arrives is not ours to control.
//! # What comes back is already prose
//! Both backends return a single "context string": snippets already selected and formatted for a
//! model. We do not re-rank, re-format or parse it into records — inventing structure on top of
//! text a model reads perfectly well would only add a place to lose information.
//! # Provider choice is deterministic
//! Configured provider wins; otherwise whichever one has a key; otherwise Exa. Deliberately not
//! opencode's session-id hash A/B split — that is experiment infrastructure, and a tool whose
//! backend silently differs per session is one nobody can reason about when results differ.
//! # The key is asked for on every call
//! Keys come from a [`SearchKeySource`] — the engine wires it to the same credential chain the
//! providers use — rather than from a field frozen at registration, so a key written from the
//! settings page applies to the very next search. Freezing it turns "I just entered a key" into
//! "still on the free tier, still rate limited", which is one restart away from being wrong in a
//! way nothing in the transcript explains.
//! # The query leaves this machine
//! Unavoidably: that is what a search tool does. Worth stating because it is the one thing here a
//! user might not expect — the query text (not the workspace, not any file) goes to the configured
//! backend, keyless by default at their free tier.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use zlogic_protocol::llm::ToolDefinition;

use crate::{Recovery, Result, Tool, ToolCtx, ToolExecResult, ToolMeta, ToolRisk, parse_args};

/// Exa's hosted MCP endpoint. A key, when present, rides as a query parameter — that is the
/// interface Exa documents for it, not a shortcut.
pub const EXA_URL: &str = "https://mcp.exa.ai/mcp";
/// Parallel's hosted MCP endpoint. Its key goes in `Authorization`.
pub const PARALLEL_URL: &str = "https://search.parallel.ai/mcp";

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(25);
/// Nothing useful is ever this big, and the cap is on **bytes read**: a backend having a bad day
/// must not get to decide how much memory this process uses.
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const DEFAULT_RESULTS: u32 = 8;
const MAX_RESULTS: u32 = 20;
const MAX_CONTEXT_CHARS: u32 = 50_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchProvider {
    Exa,
    Parallel,
}

impl SearchProvider {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "exa" => Some(Self::Exa),
            "parallel" => Some(Self::Parallel),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Exa => "exa",
            Self::Parallel => "parallel",
        }
    }

    pub fn env_var(self) -> &'static str {
        match self {
            Self::Exa => "EXA_API_KEY",
            Self::Parallel => "PARALLEL_API_KEY",
        }
    }

    pub fn key_page(self) -> &'static str {
        match self {
            Self::Exa => "https://dashboard.exa.ai/api-keys",
            Self::Parallel => "https://platform.parallel.ai",
        }
    }
}

pub trait SearchKeySource: Send + Sync + std::fmt::Debug {
    fn key(&self, backend: SearchProvider) -> Option<String>;
}

/// Where to search and with whose key.
/// Owned by the tool instance rather than read from the environment at call time, so a deployment
/// that configures a key (keyring, a different endpoint) does not have to put it in the process
/// environment — where `shell` would then have to scrub it back out.
#[derive(Debug, Clone)]
pub struct WebSearchSettings {
    /// `None` = decide from which key is present. See the module docs.
    pub provider: Option<SearchProvider>,
    pub key_source: Option<Arc<dyn SearchKeySource>>,
    pub exa_key: Option<String>,
    pub parallel_key: Option<String>,
    /// Overridable so a self-hosted or proxied endpoint can be pointed at, and so tests can aim
    /// this at a dead port instead of the internet.
    pub exa_url: String,
    pub parallel_url: String,
    pub timeout: Duration,
}

impl Default for WebSearchSettings {
    fn default() -> Self {
        Self {
            provider: None,
            key_source: None,
            exa_key: None,
            parallel_key: None,
            exa_url: EXA_URL.to_string(),
            parallel_url: PARALLEL_URL.to_string(),
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

impl WebSearchSettings {
    /// The zero-configuration path: `export EXA_API_KEY=…` and it works.
    pub fn from_env() -> Self {
        Self {
            provider: std::env::var("ZLOGIC_WEB_SEARCH_PROVIDER")
                .ok()
                .as_deref()
                .and_then(SearchProvider::parse),
            exa_key: non_empty(std::env::var("EXA_API_KEY").ok()),
            parallel_key: non_empty(std::env::var("PARALLEL_API_KEY").ok()),
            ..Self::default()
        }
    }

    pub fn key_for(&self, provider: SearchProvider) -> Option<String> {
        let pinned = match provider {
            SearchProvider::Exa => self.exa_key.clone(),
            SearchProvider::Parallel => self.parallel_key.clone(),
        };
        pinned.filter(|key| !key.trim().is_empty()).or_else(|| {
            self.key_source
                .as_ref()
                .and_then(|source| source.key(provider))
        })
    }

    /// Configured provider, else whichever has a key, else Exa.
    fn resolve_provider(&self) -> SearchProvider {
        if let Some(p) = self.provider {
            return p;
        }
        match (
            self.key_for(SearchProvider::Exa).is_some(),
            self.key_for(SearchProvider::Parallel).is_some(),
        ) {
            // Both keyed: Exa, because it is the one whose knobs this tool exposes.
            (true, _) => SearchProvider::Exa,
            (false, true) => SearchProvider::Parallel,
            // Neither: both endpoints answer without a key at their free tier.
            (false, false) => SearchProvider::Exa,
        }
    }
}

fn non_empty(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}

#[derive(Debug, Deserialize)]
struct Args {
    query: String,
    num_results: Option<u32>,
    /// `auto` / `fast` / `deep`. Exa only — see the definition.
    mode: Option<String>,
    /// Prefer freshly crawled pages over cached ones.
    #[serde(default)]
    fresh: bool,
    max_chars: Option<u32>,
}

pub struct WebSearch {
    settings: WebSearchSettings,
}

impl Default for WebSearch {
    fn default() -> Self {
        Self::new(WebSearchSettings::from_env())
    }
}

impl WebSearch {
    pub fn new(settings: WebSearchSettings) -> Self {
        Self { settings }
    }
}

#[async_trait]
impl Tool for WebSearch {
    fn meta(&self) -> ToolMeta {
        // Read, like `web_fetch`: nothing on this machine changes. That the query reaches a third
        // party is something the approval pipeline can see in the arguments; a static level cannot
        // express it.
        ToolMeta {
            name: "web_search".into(),
            source: "builtin",
            risk: ToolRisk::Read,
        }
    }

    fn definition(&self) -> ToolDefinition {
        let provider = self.settings.resolve_provider();
        let properties = match provider {
            SearchProvider::Exa => json!({
                "query": {
                    "type": "string",
                    "minLength": 1,
                    "description": "What to search for. Write it as a search query, not as a question to an assistant."
                },
                "num_results": {
                    "type": "integer", "minimum": 1, "maximum": MAX_RESULTS,
                    "description": format!("How many results to draw on (default {DEFAULT_RESULTS}, maximum {MAX_RESULTS})")
                },
                "mode": {
                    "type": "string", "enum": ["auto", "fast", "deep"],
                    "description": "`fast` for a quick lookup, `deep` when the answer needs several sources, `auto` (default) in between"
                },
                "fresh": {
                    "type": "boolean",
                    "description": "Crawl pages now instead of preferring cached copies. Slower; use it for things that changed recently"
                },
                "max_chars": {
                    "type": "integer", "minimum": 500, "maximum": MAX_CONTEXT_CHARS,
                    "description": "Cap on the returned text (default 10000). Raise it only when excerpts are being cut mid-thought"
                }
            }),
            SearchProvider::Parallel => json!({
                "query": {
                    "type": "string",
                    "minLength": 1,
                    "description": "The research objective sent to Parallel. State the information needed and relevant constraints."
                }
            }),
        };
        ToolDefinition {
            name: "web_search".into(),
            description: format!(
                "Search the web through {} and get back relevant excerpts as \
                          text — for anything \
                          past your knowledge cutoff, anything version-specific, or anything you \
                          would otherwise be guessing about. The query is sent to a third-party \
                          search backend. Follow up with web_fetch when you need a whole page \
                          rather than excerpts.",
                provider.label()
            ),
            parameters: json!({
                "type": "object",
                "properties": properties,
                "required": ["query"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, ctx: &ToolCtx, args: &str) -> Result<ToolExecResult> {
        let a: Args = parse_args(args)?;
        let query = a.query.trim();
        if query.is_empty() {
            return Ok(ToolExecResult::failed("query is required"));
        }
        if let Some(mode) = &a.mode
            && !matches!(mode.as_str(), "auto" | "fast" | "deep")
        {
            return Ok(ToolExecResult::failed(format!(
                "mode must be \"auto\", \"fast\" or \"deep\", not {mode:?}"
            )));
        }

        let provider = self.settings.resolve_provider();
        let key = self.settings.key_for(provider);
        let (url, body, auth) = match provider {
            SearchProvider::Exa => (
                exa_url(&self.settings.exa_url, key.as_deref()),
                exa_body(query, &a),
                None,
            ),
            SearchProvider::Parallel => (
                self.settings.parallel_url.clone(),
                parallel_body(query, &ctx.session_id.to_string()),
                key,
            ),
        };

        let client = match reqwest::Client::builder()
            .timeout(self.settings.timeout)
            .user_agent("zlogic/0.1 (web_search)")
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolExecResult::failed(format!(
                    "cannot build an HTTP client: {e}"
                )));
            }
        };

        // Serialised by hand rather than with reqwest's `json` feature: this crate deliberately
        // builds reqwest without it (see Cargo.toml), and one `to_string` is the whole difference.
        let mut request = client
            .post(&url)
            // Both are acceptable and the endpoint picks; see the module docs.
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .body(body.to_string());
        if let Some(key) = auth {
            request = request.bearer_auth(key);
        }

        ctx.progress(&format!("searching ({}) …", provider.label()));

        let response = tokio::select! {
            r = request.send() => r,
            // A search nobody is waiting for should not hold a connection open.
            _ = ctx.cancel.cancelled() => return Ok(ToolExecResult::cancelled("search interrupted")),
        };
        let response = match response {
            Ok(r) => r,
            Err(e) if e.is_timeout() => {
                return Ok(ToolExecResult::failed(format!(
                    "the {} search backend did not respond within {}s. Try again, or a narrower \
                     query — `deep` mode and `fresh` are both slow.",
                    provider.label(),
                    self.settings.timeout.as_secs()
                )));
            }
            Err(e) => {
                return Ok(ToolExecResult::failed(format!(
                    "cannot reach the {} search backend ({}): {e}",
                    provider.label(),
                    redact_key(&url)
                )));
            }
        };

        let status = response.status();
        let raw = read_capped(response, MAX_RESPONSE_BYTES).await;
        if !status.is_success() {
            // The backend's own message, not just the status line: for a rejected key or an
            // exhausted quota it is the only thing that says which.
            let hint = match status.as_u16() {
                401 | 403 => format!(
                    " The {} key was rejected. Set it in Settings → Tools → web search \
                     ({}), or run `zlogic key set {}`; the keychain entry is `{}`, the \
                     environment variable {}.",
                    provider.label(),
                    provider.key_page(),
                    provider.label(),
                    zlogic_credential::keyring_entry(provider.label()),
                    provider.env_var(),
                ),
                429 => {
                    format!(
                        " The free tier's rate limit is the usual cause; a key raises it — set \
                         one in Settings → Tools → web search, or get one at {}.",
                        provider.key_page()
                    )
                }
                _ => String::new(),
            };
            return Ok(ToolExecResult::failed(format!(
                "the {} search backend ({}) returned HTTP {}{}{hint}",
                provider.label(),
                redact_key(&url),
                status.as_u16(),
                match first_chars(raw.trim(), 500) {
                    s if s.is_empty() => String::new(),
                    s => format!(": {s}"),
                }
            )));
        }

        let text = match extract_mcp_text(&raw) {
            Ok(Some(text)) => text,
            // A search that found nothing is an answer, not a failure — but it must not look like
            // the tool silently returned empty.
            Ok(None) => {
                return Ok(ToolExecResult::success(format!(
                    "No results for {query:?} from {}. Try different words, fewer constraints, or \
                     `fresh: true` if this is about something very recent.",
                    provider.label()
                )));
            }
            Err(e) => {
                return Ok(ToolExecResult::failed(format!(
                    "the {} search backend reported an error: {e}",
                    provider.label()
                )));
            }
        };

        let header = format!("web_search ({}) — {query}\n\n", provider.label());
        ctx.offload_if_large(&format!("{header}{text}"), Recovery::Narrow)
    }
}

/// Exa takes its key as a query parameter.
/// Appended textually rather than through `Url`: the configured endpoint may already carry a query
/// string (a proxy), and this keeps that intact without pulling in URL parsing for one join.
fn exa_url(base: &str, key: Option<&str>) -> String {
    match key {
        None => base.to_string(),
        Some(k) if base.contains('?') => format!("{base}&exaApiKey={k}"),
        Some(k) => format!("{base}?exaApiKey={k}"),
    }
}

/// Hides the key before a URL is put anywhere a person or a model will see it.
/// Exa's key travels in the query string, so **every** message naming the endpoint has to go
/// through this. A tool result is persisted, replayed and fed back to the model on the next round:
/// a key that reaches it once is in the transcript for good.
fn redact_key(url: &str) -> String {
    let Some(at) = url.find("exaApiKey=") else {
        return url.to_string();
    };
    let value_start = at + "exaApiKey=".len();
    let value_end = url[value_start..]
        .find('&')
        .map(|i| value_start + i)
        .unwrap_or(url.len());
    format!("{}exaApiKey=***{}", &url[..at], &url[value_end..])
}

/// One JSON-RPC `tools/call`. `id` is always 1: there is exactly one request per connection, so
/// there is nothing to correlate.
fn mcp_call(tool: &str, arguments: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": tool, "arguments": arguments }
    })
}

fn exa_body(query: &str, a: &Args) -> Value {
    let mut arguments = json!({
        "query": query,
        "type": a.mode.clone().unwrap_or_else(|| "auto".into()),
        "numResults": a.num_results.unwrap_or(DEFAULT_RESULTS).min(MAX_RESULTS),
        // `fallback` = use a cached copy when there is one. `preferred` = crawl now.
        "livecrawl": if a.fresh { "preferred" } else { "fallback" },
    });
    if let Some(max) = a.max_chars {
        arguments["contextMaxCharacters"] = json!(max.min(MAX_CONTEXT_CHARS));
    }
    mcp_call("web_search_exa", arguments)
}

/// Parallel's shape is different enough to be worth naming: it takes an *objective* plus a list of
/// queries, and has no knobs matching `mode` / `fresh` / `max_chars`.
/// The session id goes along because Parallel uses it to relate follow-up searches in one
/// conversation — nothing else about the session is sent.
fn parallel_body(query: &str, session_id: &str) -> Value {
    mcp_call(
        "web_search",
        json!({
            "objective": query,
            "search_queries": [query],
            "session_id": session_id,
        }),
    )
}

/// Pulls the text out of an MCP result, whichever transport shape it arrived in.
/// `Ok(None)` = a well-formed response with nothing in it. `Err` = the backend said what went
/// wrong and that message is worth more than "search failed".
fn extract_mcp_text(body: &str) -> std::result::Result<Option<String>, String> {
    if let Some(found) = parse_payload(body.trim())? {
        return Ok(Some(found));
    }
    // SSE: the same JSON, one `data:` line at a time. Later frames can carry the payload, so every
    // line is tried rather than only the first.
    for line in body.lines() {
        let Some(payload) = line.strip_prefix("data: ") else {
            continue;
        };
        if let Some(found) = parse_payload(payload.trim())? {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

/// One JSON-RPC envelope. Not-JSON is not an error here — in an SSE body most lines are not.
fn parse_payload(payload: &str) -> std::result::Result<Option<String>, String> {
    if !payload.starts_with('{') {
        return Ok(None);
    }
    let Ok(value) = serde_json::from_str::<Value>(payload) else {
        return Ok(None);
    };

    if let Some(message) = value.get("error").and_then(|e| {
        e.get("message")
            .and_then(Value::as_str)
            .or_else(|| e.as_str())
    }) {
        return Err(message.to_string());
    }

    let content = value
        .get("result")
        .and_then(|r| r.get("content"))
        .and_then(Value::as_array);
    let Some(content) = content else {
        return Ok(None);
    };
    Ok(content
        .iter()
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .find(|t| !t.trim().is_empty())
        .map(str::to_string))
}

/// Reads the body chunk by chunk and stops at `cap`.
/// Not `response.text()`: that trusts `content-length` (or its absence) with this process's memory.
async fn read_capped(mut response: reqwest::Response, cap: usize) -> String {
    let mut bytes: Vec<u8> = Vec::new();
    while bytes.len() < cap {
        match response.chunk().await {
            Ok(Some(chunk)) => bytes.extend_from_slice(&chunk),
            // A truncated body is still worth parsing — the useful frame may already be in hand.
            Ok(None) | Err(_) => break,
        }
    }
    bytes.truncate(cap);
    String::from_utf8_lossy(&bytes).into_owned()
}

fn first_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ToolExecStatus, test_ctx};
    use std::path::Path;
    use std::sync::Mutex;

    fn args(v: Value) -> Args {
        serde_json::from_value(v).unwrap()
    }

    #[derive(Debug, Default)]
    struct MutableKeys(Mutex<(Option<String>, Option<String>)>);

    impl MutableKeys {
        fn set(&self, provider: SearchProvider, key: Option<&str>) {
            let mut guard = self.0.lock().unwrap();
            match provider {
                SearchProvider::Exa => guard.0 = key.map(str::to_string),
                SearchProvider::Parallel => guard.1 = key.map(str::to_string),
            }
        }
    }

    impl SearchKeySource for MutableKeys {
        fn key(&self, provider: SearchProvider) -> Option<String> {
            let guard = self.0.lock().unwrap();
            match provider {
                SearchProvider::Exa => guard.0.clone(),
                SearchProvider::Parallel => guard.1.clone(),
            }
        }
    }

    #[test]
    fn a_key_stored_after_registration_is_used_on_the_next_call() {
        let keys = Arc::new(MutableKeys::default());
        let settings = WebSearchSettings {
            key_source: Some(keys.clone()),
            ..Default::default()
        };
        assert_eq!(settings.key_for(SearchProvider::Exa), None);

        keys.set(SearchProvider::Exa, Some("sk-just-entered"));
        assert_eq!(
            settings.key_for(SearchProvider::Exa).as_deref(),
            Some("sk-just-entered")
        );
        assert_eq!(settings.resolve_provider(), SearchProvider::Exa);

        keys.set(SearchProvider::Exa, None);
        assert_eq!(settings.key_for(SearchProvider::Exa), None);
    }

    #[test]
    fn a_static_key_wins_over_the_source() {
        let keys = Arc::new(MutableKeys::default());
        keys.set(SearchProvider::Parallel, Some("from-keychain"));
        let settings = WebSearchSettings {
            key_source: Some(keys),
            parallel_key: Some("pinned".into()),
            ..Default::default()
        };
        assert_eq!(
            settings.key_for(SearchProvider::Parallel).as_deref(),
            Some("pinned")
        );
    }

    /// Configured wins; then whichever has a key; then Exa. No coin flip.
    #[test]
    fn provider_choice_is_deterministic() {
        let s = WebSearchSettings::default();
        assert_eq!(
            s.resolve_provider(),
            SearchProvider::Exa,
            "neither keyed → exa"
        );

        let keyed = WebSearchSettings {
            parallel_key: Some("k".into()),
            ..Default::default()
        };
        assert_eq!(keyed.resolve_provider(), SearchProvider::Parallel);

        let both = WebSearchSettings {
            exa_key: Some("k".into()),
            parallel_key: Some("k".into()),
            ..Default::default()
        };
        assert_eq!(both.resolve_provider(), SearchProvider::Exa);

        let forced = WebSearchSettings {
            provider: Some(SearchProvider::Parallel),
            exa_key: Some("k".into()),
            ..Default::default()
        };
        assert_eq!(
            forced.resolve_provider(),
            SearchProvider::Parallel,
            "config wins"
        );
    }

    #[test]
    fn definition_only_exposes_parameters_supported_by_the_selected_provider() {
        let exa = WebSearch::new(WebSearchSettings {
            provider: Some(SearchProvider::Exa),
            ..WebSearchSettings::default()
        })
        .definition();
        assert!(exa.parameters["properties"].get("mode").is_some());
        assert!(exa.parameters["properties"].get("fresh").is_some());

        let parallel = WebSearch::new(WebSearchSettings {
            provider: Some(SearchProvider::Parallel),
            ..WebSearchSettings::default()
        })
        .definition();
        assert_eq!(
            parallel.parameters["properties"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            vec!["query"]
        );
        assert!(parallel.description.contains("parallel"));
    }

    #[test]
    fn the_exa_key_rides_in_the_query_string_and_an_existing_one_survives() {
        assert_eq!(exa_url(EXA_URL, None), EXA_URL);
        assert_eq!(
            exa_url(EXA_URL, Some("abc")),
            format!("{EXA_URL}?exaApiKey=abc")
        );
        assert_eq!(
            exa_url("https://proxy.test/mcp?tenant=x", Some("abc")),
            "https://proxy.test/mcp?tenant=x&exaApiKey=abc"
        );
    }

    /// The request is a plain JSON-RPC `tools/call` — the whole "MCP client" this tool needs.
    #[test]
    fn the_exa_request_is_a_tools_call_with_the_documented_arguments() {
        let body = exa_body(
            "rust async traits",
            &args(
                json!({ "query": "x", "mode": "deep", "num_results": 3, "fresh": true, "max_chars": 2000 }),
            ),
        );
        assert_eq!(body["method"], "tools/call");
        assert_eq!(body["params"]["name"], "web_search_exa");
        let a = &body["params"]["arguments"];
        assert_eq!(a["query"], "rust async traits");
        assert_eq!(a["type"], "deep");
        assert_eq!(a["numResults"], 3);
        assert_eq!(a["livecrawl"], "preferred", "fresh means crawl now");
        assert_eq!(a["contextMaxCharacters"], 2000);
    }

    #[test]
    fn defaults_are_filled_in_and_caps_are_clamped_not_rejected() {
        let body = exa_body(
            "q",
            &args(json!({ "query": "q", "num_results": 500, "max_chars": 9_999_999 })),
        );
        let a = &body["params"]["arguments"];
        assert_eq!(a["type"], "auto");
        assert_eq!(a["numResults"], MAX_RESULTS);
        assert_eq!(a["livecrawl"], "fallback");
        assert_eq!(a["contextMaxCharacters"], MAX_CONTEXT_CHARS);
    }

    /// Parallel's shape is genuinely different — an objective plus queries, no knobs.
    #[test]
    fn the_parallel_request_carries_an_objective_and_the_session() {
        let body = parallel_body("who shipped it", "sess-1");
        assert_eq!(body["params"]["name"], "web_search");
        let a = &body["params"]["arguments"];
        assert_eq!(a["objective"], "who shipped it");
        assert_eq!(a["search_queries"][0], "who shipped it");
        assert_eq!(a["session_id"], "sess-1");
    }

    #[test]
    fn a_plain_json_result_yields_its_text() {
        let body = json!({
            "result": { "content": [{ "type": "text", "text": "the answer" }] }
        })
        .to_string();
        assert_eq!(
            extract_mcp_text(&body).unwrap().as_deref(),
            Some("the answer")
        );
    }

    /// The same payload can arrive as SSE instead, and the useful frame is not always the first.
    #[test]
    fn an_sse_body_is_parsed_too() {
        let body = "event: message\n\
                    data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"\"}]}}\n\n\
                    data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"found it\"}]}}\n\n";
        assert_eq!(extract_mcp_text(body).unwrap().as_deref(), Some("found it"));
    }

    /// The backend's own message is worth more than "search failed".
    #[test]
    fn a_json_rpc_error_is_reported_verbatim() {
        let body = json!({ "error": { "code": -32602, "message": "invalid api key" } }).to_string();
        assert_eq!(extract_mcp_text(&body).unwrap_err(), "invalid api key");
    }

    #[test]
    fn a_well_formed_response_with_nothing_in_it_is_not_an_error() {
        assert_eq!(
            extract_mcp_text(&json!({ "result": { "content": [] } }).to_string()).unwrap(),
            None
        );
        assert_eq!(extract_mcp_text("not json at all").unwrap(), None);
        assert_eq!(extract_mcp_text("").unwrap(), None);
    }

    #[tokio::test]
    async fn an_empty_query_is_refused_before_any_request() {
        let ctx = test_ctx(Path::new("/work"));
        let tool = WebSearch::new(WebSearchSettings::default());
        let out = tool.execute(&ctx, r#"{"query":"   "}"#).await.unwrap();
        assert_eq!(out.status, ToolExecStatus::Failed);
        assert!(out.model_text().contains("query is required"));
    }

    #[tokio::test]
    async fn an_unknown_mode_is_reported_to_the_model() {
        let ctx = test_ctx(Path::new("/work"));
        let tool = WebSearch::new(WebSearchSettings::default());
        let out = tool
            .execute(&ctx, r#"{"query":"x","mode":"thorough"}"#)
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Failed);
        assert!(
            out.model_text().contains("\"deep\""),
            "{}",
            out.model_text()
        );
    }

    /// A key in the query string must never reach a tool result: those are persisted, replayed and
    /// fed back to the model, so once is forever.
    #[test]
    fn the_key_is_redacted_out_of_any_url_we_report() {
        let url = exa_url(EXA_URL, Some("sk-secret"));
        let shown = redact_key(&url);
        assert!(!shown.contains("sk-secret"), "{shown}");
        assert!(shown.ends_with("exaApiKey=***"), "{shown}");
        // A key in the middle of a query string keeps what follows it.
        assert_eq!(
            redact_key("https://p.test/mcp?exaApiKey=sk-secret&tenant=x"),
            "https://p.test/mcp?exaApiKey=***&tenant=x"
        );
        // Nothing to hide is left alone.
        assert_eq!(redact_key(PARALLEL_URL), PARALLEL_URL);
    }

    #[test]
    fn a_key_problem_points_at_the_vendors_page() {
        for provider in [SearchProvider::Exa, SearchProvider::Parallel] {
            let page = provider.key_page();
            assert!(page.starts_with("https://"), "{page}");
            assert!(
                provider.env_var().ends_with("_API_KEY"),
                "{}",
                provider.env_var()
            );
        }
        assert_eq!(
            SearchProvider::Exa.key_page(),
            "https://dashboard.exa.ai/api-keys"
        );
        assert_eq!(
            SearchProvider::Parallel.key_page(),
            "https://platform.parallel.ai"
        );
    }

    /// A backend that cannot be used names the endpoint, so a wrong `exa_url` (or a corporate
    /// proxy answering in its place) is diagnosable from the transcript alone.
    /// Port 1 refuses immediately, so this needs no network — but on a machine with `HTTP_PROXY`
    /// set the proxy answers instead of the connection failing. Both are failures that must name
    /// the endpoint, which is what this asserts rather than which of the two happened.
    #[tokio::test]
    async fn a_backend_that_cannot_be_used_says_which_endpoint_it_was() {
        let ctx = test_ctx(Path::new("/work"));
        let tool = WebSearch::new(WebSearchSettings {
            exa_url: "http://127.0.0.1:1/mcp".into(),
            ..Default::default()
        });
        let out = tool.execute(&ctx, r#"{"query":"anything"}"#).await.unwrap();
        assert_eq!(out.status, ToolExecStatus::Failed);
        let text = out.model_text();
        assert!(text.contains("127.0.0.1:1"), "{text}");
        assert!(text.contains("exa"), "{text}");
    }
}
