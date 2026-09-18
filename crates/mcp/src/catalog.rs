//! Which servers exist, what tools they have, and the [`ToolSource`] that offers them.
//! # Discovery is a directory scan, every time
//! There is no index and nothing to rebuild. A definition copied in by hand, or arriving with a
//! `git pull`, is visible on the next load — because the load *is* the scan. Enumerating a couple of
//! directories costs microseconds, and every mechanism that would make it cheaper (a cache, a
//! watcher, a registry file) also makes it possible for what zlogic believes to differ from what is on
//! disk.
//! # Tool lists come from a cache, and that is what makes the source synchronous
//! [`ToolSource::discover`] cannot await, and it must not start every configured server just to
//! answer "what tools are there" — twenty definitions would mean twenty processes on the first turn,
//! most of which the model never calls. So a server's tool list is read from a file under the cache
//! directory, written the first time the server is actually reached.
//! The consequence is deliberate and worth stating plainly: **a newly added server contributes no
//! tools until its list has been fetched once.** [`Catalog::refresh`] is the fetch, it is async, and
//! the host calls it at turn start — so in practice the first turn after adding a server pays one
//! connection and every later turn pays nothing.
//! A cached list is keyed by both the definition's fingerprint and its resolved connection key.
//! Editing the command invalidates it, while a server whose cwd or arguments depend on the workspace
//! gets a separate list for each workspace. Lists also expire ([`CACHE_TTL`]): a server that adds a
//! tool without its definition changing is picked up within a day, and until then the old list is
//! still used rather than the tool set silently emptying.
//! # Fetching a list is bounded: batched concurrency, and a deadline on waiting
//! A turn waits at most [`RefreshLimits::deadline`] for the lists it is missing. Whatever does not
//! arrive in time is **not dropped**: its fetch is detached and keeps going, writes the cache, and the
//! next turn hits it. The reason is the first turn: a server that cannot be reached takes until
//! `connect_timeout` (30s by default), and that is 30 seconds of a user watching an interface that has
//! not said anything. Better a turn with a few tools missing.
//! Concurrency is bounded per transport (3 stdio / 20 remote). Each stdio server is **a process on the
//! user's machine** and twenty starting at once is felt; a remote one is network IO, where concurrency
//! is worth far more.
//! # The per-server allowlist takes effect here
//! [`crate::def::ToolFilter`] is applied **as a list enters [`Loaded`]**; the cache still holds the
//! server's own, unfiltered list. So editing an allowlist costs no connection — the filter says which
//! of the server's tools *we* want, which is not a fact about the server — and a name in the allowlist
//! that the server does not have is reported as a [`Problem`]. Quietly offering one tool fewer is not
//! something anyone can debug.
//! # Precedence, when the same id is defined twice
//! Global, then plugins, then the workspace — later wins. The workspace overriding the user's own
//! install is the one that matters: a repository that pins a specific server for its contributors
//! should get it, and it is also the direction a user can see, because the definition is a file in
//! their checkout.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use sha2::{Digest, Sha256};
use zlogic_tools::{Tool, ToolSource};

use crate::conn::Label;
use crate::def::{self, Origin, Problem, ServerDef};
use crate::pool::McpPool;
use crate::resolve::Resolver;
use crate::tool::{McpTool, ToolSpec};

/// How many characters of a tool definition go to one token, for estimation.
/// It is an **estimate**, and its only job is deciding whether to tell the user how much of the
/// window their MCP servers are taking — paying for a token-counting API call to sharpen that number
/// would cost more than the number is worth.
pub const CHARS_PER_TOKEN: f32 = 2.5;

/// Last observed runtime health for a configured server.
/// This is deliberately process-local. Connectivity is live state, not configuration, and must not
/// be written into the conversation transcript or an on-disk extension database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeStatus {
    Connecting,
    Ready,
    AuthenticationRequired(String),
    Unavailable(String),
}

impl RuntimeStatus {
    pub fn failure(message: impl Into<String>) -> Self {
        let message = message.into();
        let lower = message.to_ascii_lowercase();
        if lower.contains("oauth authorization is required")
            || lower.contains("credential is not set")
            || lower.contains("unauthorized")
            || lower.contains("status 401")
            || lower.contains("status 403")
        {
            Self::AuthenticationRequired(message)
        } else {
            Self::Unavailable(message)
        }
    }
}

fn runtime_status_priority(status: &RuntimeStatus) -> u8 {
    match status {
        RuntimeStatus::Ready => 0,
        RuntimeStatus::Connecting => 1,
        RuntimeStatus::Unavailable(_) => 2,
        RuntimeStatus::AuthenticationRequired(_) => 3,
    }
}

/// The bounds on fetching missing tool lists.
#[derive(Debug, Clone)]
pub struct RefreshLimits {
    /// How long a turn waits. What misses it is left for the next turn — the fetch keeps running and
    /// writes the cache.
    pub deadline: std::time::Duration,
    /// How many stdio servers may start at once. Each one is a process on this machine.
    pub stdio_batch: usize,
    /// How many remote servers may be contacted at once. Network IO, so this can be far higher.
    pub remote_batch: usize,
}

impl Default for RefreshLimits {
    fn default() -> Self {
        Self {
            deadline: std::time::Duration::from_secs(5),
            stdio_batch: 3,
            remote_batch: 20,
        }
    }
}

/// How long a cached tool list is trusted before it is fetched again.
/// A day, because the list only changes when someone edits the server, and a shorter window would
/// mean reconnecting to every configured server far more often than the model calls them.
pub const CACHE_TTL: chrono::TimeDelta = chrono::TimeDelta::hours(24);
/// Bump when cached [`ToolSpec`] normalization or annotation semantics change.
const TOOL_CACHE_VERSION: u8 = 4;

/// Where definitions are read from and where tool lists are cached.
/// Four paths rather than one root, because they are four different kinds of thing: a file the user
/// edits, a directory installs land in, files that live in the repository, and a cache that must be
/// safe to delete.
#[derive(Debug, Clone)]
pub struct CatalogDirs {
    /// `<config>/mcp.json` — the hand-edited one.
    pub global_file: PathBuf,
    /// `<data>/extensions/mcp/*.json` — one file per installed server.
    pub global_dir: PathBuf,
    /// `<cache>/mcp` — tool lists. Deleting it costs one reconnection per server, nothing else.
    pub cache: PathBuf,
}

impl CatalogDirs {
    pub fn under(dirs: &zlogic_config::Dirs) -> Self {
        Self {
            global_file: dirs.config.join("mcp.json"),
            global_dir: dirs.data.join("extensions").join("mcp"),
            cache: dirs.cache.join("mcp"),
        }
    }

    /// The definition files inside a workspace, in load order.
    /// `.mcp.json` at the root first, because that is the file the rest of the ecosystem writes and a
    /// checkout that has one should work without being converted.
    pub fn workspace_files(root: &Path) -> Vec<PathBuf> {
        let mut files = vec![
            root.join(".mcp.json"),
            root.join(".zlogic").join("mcp.json"),
        ];
        files.extend(json_files(
            &root.join(".zlogic").join("extensions").join("mcp"),
        ));
        files
    }
}

/// Reads one file into the accumulating map, if it exists. A file that is absent is the normal case
/// and contributes nothing; a file that cannot be parsed contributes a problem.
fn read_file(
    path: &Path,
    origin: Origin,
    by_id: &mut BTreeMap<String, Arc<ServerDef>>,
    problems: &mut Vec<Problem>,
) {
    if !path.is_file() {
        return;
    }
    match def::parse_file(path, origin) {
        Ok(parsed) => {
            problems.extend(parsed.problems);
            for server in parsed.servers {
                by_id.insert(server.id.clone(), Arc::new(server));
            }
        }
        Err(e) => problems.push(Problem::new(None, Some(path.to_path_buf()), e.to_string())),
    }
}

/// Every `*.json` in a directory, sorted. A missing directory is not an error — it is the normal
/// case for a user who has installed nothing.
fn json_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json") && p.is_file())
        .collect();
    // Sorted so precedence between two files in the same directory is at least deterministic.
    files.sort();
    files
}

/// The servers that apply to one workspace, with whatever tool lists are known.
pub struct Loaded {
    /// Enabled and disabled alike, deduplicated by id with the winner kept.
    pub servers: Vec<Arc<ServerDef>>,
    /// Definitions that could not be used. Surfaced to the user, never only logged.
    pub problems: Vec<Problem>,
    /// Servers whose list is still being fetched, and which this turn therefore goes without. **Not a
    /// problem** — it fixes itself next turn, and warning about it teaches the user to ignore warnings.
    pub pending: Vec<String>,
    tools: BTreeMap<String, CachedList>,
}

struct CachedList {
    tools: Vec<Arc<ToolSpec>>,
    fetched_at: chrono::DateTime<chrono::Utc>,
}

impl Loaded {
    pub fn enabled(&self) -> impl Iterator<Item = &Arc<ServerDef>> {
        self.servers.iter().filter(|s| s.is_enabled())
    }

    pub fn get(&self, id: &str) -> Option<&Arc<ServerDef>> {
        self.servers.iter().find(|s| s.id == id)
    }

    pub fn tools_of(&self, id: &str) -> &[Arc<ToolSpec>] {
        self.tools
            .get(id)
            .map(|c| c.tools.as_slice())
            .unwrap_or_default()
    }

    /// How many tools this would contribute to the registry right now.
    pub fn tool_count(&self) -> usize {
        self.enabled().map(|s| self.tools_of(&s.id).len()).sum()
    }

    /// Keeps only the servers the caller permits, dropping their cached tool lists with them.
    /// This is where a host applies policy this crate deliberately does not have — an enable
    /// resolution across scopes, a trust decision about definitions that arrived with the repository.
    /// It has to happen **before** [`Catalog::refresh`]: a server the user disabled must not be
    /// started merely to ask it what tools it has, which would be exactly the thing disabling it was
    /// meant to prevent.
    pub fn retain(&mut self, mut keep: impl FnMut(&ServerDef) -> bool) {
        self.servers.retain(|def| keep(def));
        let kept: std::collections::BTreeSet<&str> =
            self.servers.iter().map(|s| s.id.as_str()).collect();
        self.tools.retain(|id, _| kept.contains(id.as_str()));
    }

    /// Applies each server's [`ToolFilter`](crate::def::ToolFilter), and reports a name in an
    /// allowlist that the server does not have.
    /// Runs once after lists arrive and before anybody reads them. It re-runs on every `load` because
    /// the cache holds the unfiltered list — which is exactly what makes editing an allowlist free.
    fn apply_filters(&mut self) {
        let ids: std::collections::BTreeSet<String> = self.tools.keys().cloned().collect();
        self.apply_filters_to(&ids);
    }

    /// Only the lists that just arrived.
    /// Narrowing by id is correctness, not an optimisation: every run reports a name in an allowlist
    /// that the server does not have, and one list can arrive twice — once from the cache in `load`,
    /// once refreshed. Without narrowing, one typo would be reported twice.
    fn apply_filters_to(&mut self, ids: &std::collections::BTreeSet<String>) {
        let servers = self.servers.clone();
        for def in &servers {
            if def.filter.is_empty() || !ids.contains(&def.id) {
                continue;
            }
            let Some(cached) = self.tools.get_mut(&def.id) else {
                continue;
            };
            let available: Vec<String> = cached.tools.iter().map(|t| t.name.clone()).collect();
            for missing in def.filter.missing(&available) {
                self.problems.push(Problem::new(
                    Some(def.id.clone()),
                    def.file.clone(),
                    format!(
                        "`{missing}` is on the allowlist but the server does not provide it (it has: {})",
                        available.join(", ")
                    ),
                ));
            }
            cached.tools.retain(|spec| def.filter.allows(&spec.name));
        }
    }

    /// Roughly how many tokens each server's tool definitions cost, largest first.
    /// For the "your MCP servers are taking 30% of the window" notice — which has to **name** them,
    /// because otherwise the user knows there is a problem and not which switch to turn off.
    pub fn token_estimates(&self) -> Vec<(String, usize)> {
        let mut out: Vec<(String, usize)> = self
            .enabled()
            .map(|def| {
                let chars: usize = self
                    .tools_of(&def.id)
                    .iter()
                    .map(|spec| spec_chars(&def.id, spec))
                    .sum();
                (def.id.clone(), (chars as f32 / CHARS_PER_TOKEN) as usize)
            })
            .filter(|(_, tokens)| *tokens > 0)
            .collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        out
    }

    /// Roughly what every MCP tool definition that will be offered costs, together.
    pub fn estimated_tokens(&self) -> usize {
        self.token_estimates().iter().map(|(_, t)| t).sum()
    }

    /// Enabled servers that answered with a tool list and the list was **empty**.
    /// Not the same as "contributed no tools": a server that could not be reached has no list at all,
    /// and that failure is already a [`Problem`]. Reporting both would tell the user twice about one
    /// thing — while a server that connects fine and offers nothing is a different situation with no
    /// other symptom, since in the registry it looks exactly like one that is broken.
    pub fn silent_servers(&self) -> Vec<&str> {
        self.enabled()
            .filter(|s| self.tools.get(&s.id).is_some_and(|c| c.tools.is_empty()))
            .map(|s| s.id.as_str())
            .collect()
    }

    /// Enabled servers whose list is missing or older than [`CACHE_TTL`] — what [`Catalog::refresh`]
    /// has to connect to.
    pub fn needs_refresh(&self, now: chrono::DateTime<chrono::Utc>) -> Vec<Arc<ServerDef>> {
        self.enabled()
            .filter(|s| match self.tools.get(&s.id) {
                None => true,
                Some(c) => now.signed_duration_since(c.fetched_at) > CACHE_TTL,
            })
            .cloned()
            .collect()
    }
}

pub struct Catalog {
    dirs: CatalogDirs,
    pool: Arc<McpPool>,
    limits: RefreshLimits,
    /// Servers whose tool list is being fetched right now.
    /// Needed because a fetch outlives the turn that started it (see [`Catalog::refresh`]): without
    /// this, the next turn — which still sees no cached list — would start a second fetch of the same
    /// server, and the two would race to write the same cache file. One fetch per server and resolved
    /// connection scope at a time; a turn that finds one already running reports it as pending.
    inflight: Arc<std::sync::Mutex<std::collections::BTreeSet<String>>>,
    runtime_status: Arc<Mutex<BTreeMap<(String, String), RuntimeStatus>>>,
}

/// Releases a server's in-flight claim when the fetch task ends — including when it panics.
struct InflightGuard {
    id: String,
    inflight: Arc<std::sync::Mutex<std::collections::BTreeSet<String>>>,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.inflight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.id);
    }
}

impl Catalog {
    pub fn new(dirs: CatalogDirs, pool: Arc<McpPool>) -> Self {
        Self {
            dirs,
            pool,
            limits: RefreshLimits::default(),
            inflight: Arc::new(std::sync::Mutex::new(std::collections::BTreeSet::new())),
            runtime_status: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub fn with_limits(mut self, limits: RefreshLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn limits(&self) -> &RefreshLimits {
        &self.limits
    }

    pub fn pool(&self) -> &Arc<McpPool> {
        &self.pool
    }

    pub fn dirs(&self) -> &CatalogDirs {
        &self.dirs
    }

    pub fn runtime_status(&self, id: &str) -> Option<RuntimeStatus> {
        let statuses = self
            .runtime_status
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        statuses
            .iter()
            .filter(|((server, _), _)| server == id)
            .map(|(_, status)| status)
            .max_by_key(|status| runtime_status_priority(status))
            .cloned()
    }

    /// Runtime health for this server in one workspace/connection scope. The unscoped entry is
    /// reserved for management operations such as OAuth, which happen before a workspace exists.
    pub fn runtime_status_for(
        &self,
        def: &ServerDef,
        workspace_root: &Path,
    ) -> Option<RuntimeStatus> {
        let scope = cache_scope(def, workspace_root)?;
        let statuses = self
            .runtime_status
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        statuses
            .get(&(def.id.clone(), scope))
            .or_else(|| statuses.get(&(def.id.clone(), String::new())))
            .cloned()
    }

    pub fn clear_runtime_status(&self, id: &str) {
        self.runtime_status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(server, _), _| server != id);
    }

    /// Reads every definition that applies to `workspace_root`, plus whatever plugins contributed.
    /// `contributed` comes from the plugin loader rather than being discovered here: a plugin is a
    /// directory of several things (a manifest, maybe skills, maybe servers) and only the loader
    /// knows how to read one.
    pub fn load(&self, workspace_root: &Path, contributed: Vec<ServerDef>) -> Loaded {
        let mut by_id: BTreeMap<String, Arc<ServerDef>> = BTreeMap::new();
        let mut problems = Vec::new();

        read_file(
            &self.dirs.global_file,
            Origin::Global,
            &mut by_id,
            &mut problems,
        );
        for path in json_files(&self.dirs.global_dir) {
            read_file(&path, Origin::Global, &mut by_id, &mut problems);
        }
        // Plugins sit between the two: a plugin the user installed should not override what their
        // repository pins, and should override their own loose global definitions no more than a
        // namespaced id ever collides with one.
        for server in contributed {
            by_id.insert(server.id.clone(), Arc::new(server));
        }
        for path in CatalogDirs::workspace_files(workspace_root) {
            read_file(&path, Origin::Workspace, &mut by_id, &mut problems);
        }

        let servers: Vec<Arc<ServerDef>> = by_id.into_values().collect();
        let tools = servers
            .iter()
            .filter(|s| s.is_enabled())
            .filter_map(|s| {
                self.read_cache(s, workspace_root)
                    .map(|list| (s.id.clone(), list))
            })
            .collect();

        let mut loaded = Loaded {
            servers,
            problems,
            pending: Vec::new(),
            tools,
        };
        loaded.apply_filters();
        loaded
    }

    /// Connects to the servers whose tool list is missing or stale and caches what they report.
    /// **Waits at most [`RefreshLimits::deadline`].** Missing that is neither a failure nor a loss: the
    /// fetch is detached, finishes on its own and writes the cache, so the next turn hits it. For this
    /// turn those servers land in [`Loaded::pending`].
    /// Concurrency is bounded per transport (see the module docs).
    /// A server that cannot be reached produces a [`Problem`] and **keeps its previous list**: a
    /// network blip must not empty the tool set the model was using a minute ago.
    pub async fn refresh(&self, loaded: &mut Loaded, workspace_root: &Path) {
        let stale = loaded.needs_refresh(chrono::Utc::now());
        if stale.is_empty() {
            return;
        }

        // Tracked by id so the ones that never answered can be named.
        let mut refreshed: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let mut waiting: std::collections::BTreeSet<String> =
            stale.iter().map(|d| d.id.clone()).collect();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        // Two pools of permits: every stdio connection is a process on this machine.
        let stdio = Arc::new(tokio::sync::Semaphore::new(self.limits.stdio_batch.max(1)));
        let remote = Arc::new(tokio::sync::Semaphore::new(self.limits.remote_batch.max(1)));

        for def in stale {
            // One fetch per server at a time. A previous turn's fetch may still be running (it
            // outlives the turn that started it), and starting a second would double the work and
            // race to write the same cache file. The server is still reported as pending, which is
            // exactly what it is.
            let cache_scope = cache_scope(&def, workspace_root)
                .unwrap_or_else(|| format!("{}:{}", def.fingerprint(), workspace_root.display()));
            let claim = format!("{}:{cache_scope}", def.id);
            if !self.claim(&claim) {
                continue;
            }
            self.set_runtime_status_scoped(&def.id, &cache_scope, RuntimeStatus::Connecting);
            let permits = if def.transport.is_stdio() {
                stdio.clone()
            } else {
                remote.clone()
            };
            let (pool, dirs, root, tx, runtime_status) = (
                self.pool.clone(),
                self.dirs.clone(),
                workspace_root.to_path_buf(),
                tx.clone(),
                self.runtime_status.clone(),
            );
            let guard = InflightGuard {
                id: claim,
                inflight: self.inflight_handle(),
            };
            // **Detached**: the deadline does not interrupt this. It finishes and writes the cache,
            // and that cache is the next turn's gain.
            tokio::spawn(async move {
                // Released when this task ends, whichever way it ends.
                let _guard = guard;
                let _permit = permits.acquire().await;
                let outcome = fetch_tools(&pool, &def, &root).await;
                let status = match &outcome {
                    Ok(_) => RuntimeStatus::Ready,
                    Err(error) => RuntimeStatus::failure(error.to_string()),
                };
                runtime_status
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert((def.id.clone(), cache_scope.clone()), status);
                if let Ok(tools) = &outcome {
                    let now = chrono::Utc::now();
                    if let Err(e) = write_tool_cache(&dirs, &def, &root, tools, now) {
                        tracing::warn!(
                            target: "zlogic::mcp",
                            server = %def.id,
                            "could not cache the tool list: {e}"
                        );
                    }
                }
                // Nobody listening only means this turn stopped waiting — the cache is already
                // written, so that is not an error.
                let _ = tx.send((def, cache_scope, outcome));
            });
        }
        drop(tx);

        let deadline = tokio::time::sleep(self.limits.deadline);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                received = rx.recv() => match received {
                    Some((def, scope, outcome)) => {
                        waiting.remove(&def.id);
                        if outcome.is_ok() {
                            refreshed.insert(def.id.clone());
                        }
                        self.absorb(loaded, &def, &scope, outcome);
                        if waiting.is_empty() {
                            break;
                        }
                    }
                    None => break,
                },
                () = &mut deadline => {
                    tracing::debug!(
                        target: "zlogic::mcp",
                        "{} server(s) did not report a tool list within {:?}; left for the next turn",
                        waiting.len(),
                        self.limits.deadline
                    );
                    break;
                }
            }
        }
        loaded.pending.extend(waiting);
        loaded.apply_filters_to(&refreshed);
    }

    /// Takes the right to fetch this server's list, or reports that somebody already has it.
    fn claim(&self, id: &str) -> bool {
        self.inflight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.to_string())
    }

    fn inflight_handle(&self) -> Arc<std::sync::Mutex<std::collections::BTreeSet<String>>> {
        self.inflight.clone()
    }

    pub fn set_runtime_status(&self, id: &str, status: RuntimeStatus) {
        self.runtime_status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert((id.to_string(), String::new()), status);
    }

    fn set_runtime_status_scoped(&self, id: &str, scope: &str, status: RuntimeStatus) {
        self.runtime_status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert((id.to_string(), scope.to_string()), status);
    }

    /// One fetch's outcome, folded into `Loaded`.
    fn absorb(
        &self,
        loaded: &mut Loaded,
        def: &Arc<ServerDef>,
        scope: &str,
        outcome: crate::Result<Vec<ToolSpec>>,
    ) {
        match outcome {
            Ok(tools) => {
                self.set_runtime_status_scoped(&def.id, scope, RuntimeStatus::Ready);
                let tools = tools.into_iter().map(Arc::new).collect();
                loaded.tools.insert(
                    def.id.clone(),
                    CachedList {
                        tools,
                        fetched_at: chrono::Utc::now(),
                    },
                );
            }
            Err(e) => {
                let message = e.to_string();
                let status = RuntimeStatus::failure(message.clone());
                self.set_runtime_status_scoped(&def.id, scope, status);
                let had_cached = loaded.tools.contains_key(&def.id);
                loaded.problems.push(Problem::new(
                    Some(def.id.clone()),
                    def.file.clone(),
                    if had_cached {
                        format!("{e} (using the tool list from the last successful connection)")
                    } else {
                        message
                    },
                ));
            }
        }
    }

    /// The [`ToolSource`] for these servers. Cheap: it clones handles, connects to nothing.
    pub fn source(&self, loaded: &Loaded) -> McpSource {
        let entries = loaded
            .enabled()
            .map(|def| (def.clone(), loaded.tools_of(&def.id).to_vec()))
            .collect();
        McpSource {
            entries,
            pool: self.pool.clone(),
        }
    }

    fn read_cache(&self, def: &ServerDef, workspace_root: &Path) -> Option<CachedList> {
        read_cache(&self.dirs, def, workspace_root)
    }
}

/// The tool-list cache follows the same sharing boundary as live connections. A stdio server whose
/// cwd defaults to the workspace therefore gets one list per workspace, while a remote server (or a
/// stdio server with a fixed cwd) naturally reuses one list everywhere.
fn cache_scope(def: &ServerDef, workspace_root: &Path) -> Option<String> {
    let placeholder = Arc::new(|_: &str| Some("catalog-placeholder".to_string()));
    Resolver::with_lookups(workspace_root, placeholder.clone(), placeholder)
        .resolve(def, None)
        .ok()
        .map(|resolved| resolved.key.params().to_string())
}

/// Stable pruning partition. It is shared when launch parameters do not depend on the workspace;
/// otherwise it is the hash of this workspace root. Unlike `cache_scope`, this stays stable across
/// edits to the command, so an obsolete fingerprint can be removed without touching another
/// workspace's list.
fn cache_partition(def: &ServerDef, workspace_root: &Path) -> Option<String> {
    let current = cache_scope(def, workspace_root)?;
    let probe = cache_scope(def, &workspace_root.join(".zlogic-cache-scope-probe"))?;
    if current == probe {
        return Some("shared".into());
    }
    let digest = Sha256::digest(workspace_root.to_string_lossy().as_bytes());
    Some(format!("workspace-{digest:x}"))
}

fn cache_path(dirs: &CatalogDirs, def: &ServerDef, workspace_root: &Path) -> Option<PathBuf> {
    let scope = cache_scope(def, workspace_root)?;
    let partition = cache_partition(def, workspace_root)?;
    Some(dirs.cache.join(format!(
        "{}-v{}-{}-{}-{}.tools.json",
        file_stem(&def.id),
        TOOL_CACHE_VERSION,
        partition,
        def.fingerprint(),
        scope,
    )))
}

fn read_cache(dirs: &CatalogDirs, def: &ServerDef, workspace_root: &Path) -> Option<CachedList> {
    {
        let raw = std::fs::read_to_string(cache_path(dirs, def, workspace_root)?).ok()?;
        let file: CacheFile = match serde_json::from_str(&raw) {
            Ok(f) => f,
            Err(e) => {
                // Corrupt cache is not an error the user should see: it re-fetches.
                tracing::debug!(target: "zlogic::mcp", server = %def.id, "unusable tool cache: {e}");
                return None;
            }
        };
        Some(CachedList {
            tools: file.tools.into_iter().map(Arc::new).collect(),
            fetched_at: file.fetched_at,
        })
    }
}

/// Writes a server's tool list into the cache.
/// **The single writer of this format**, and public for that reason: the file name carries a version
/// and a fingerprint, so anything that hand-builds it (a host priming a list, a test seeding one) goes
/// stale silently the moment either changes — the list is then simply never found, and the only symptom
/// is a server that connects when it should not have needed to.
/// What is stored is **the server's own, unfiltered list**; the allowlist is applied on the way out
/// (see the module docs).
pub fn write_tool_cache(
    dirs: &CatalogDirs,
    def: &ServerDef,
    workspace_root: &Path,
    tools: &[ToolSpec],
    now: chrono::DateTime<chrono::Utc>,
) -> std::io::Result<()> {
    std::fs::create_dir_all(&dirs.cache)?;
    let path = cache_path(dirs, def, workspace_root).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("could not resolve cache scope for MCP server `{}`", def.id),
        )
    })?;
    let body = serde_json::to_vec_pretty(&CacheFile {
        server: def.id.clone(),
        fetched_at: now,
        tools: tools.to_vec(),
    })
    .map_err(std::io::Error::other)?;

    // Written through a temporary file: the CLI and the GUI host are separate processes over the
    // same cache, and a half-written list read by the other one would look corrupt.
    // The name carries a per-write counter as well as the pid. The pid alone is not enough: two
    // fetches of the *same* server can be in flight inside one process (a turn that gave up waiting
    // leaves its fetch running — see `Catalog::refresh`), and they would then write the same temp
    // path, leaving one of them to rename a file the other had already moved.
    static WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let unique = WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp{}-{unique}", std::process::id()));
    std::fs::write(&tmp, &body)?;
    std::fs::rename(&tmp, &path)?;
    prune_old_cache(dirs, def, workspace_root, &path);
    Ok(())
}

/// Removes lists for earlier fingerprints of the same server, so editing a definition repeatedly
/// does not leave a file behind each time.
fn prune_old_cache(dirs: &CatalogDirs, def: &ServerDef, workspace_root: &Path, keep: &Path) {
    {
        let Some(partition) = cache_partition(def, workspace_root) else {
            return;
        };
        let prefix = format!(
            "{}-v{}-{}-",
            file_stem(&def.id),
            TOOL_CACHE_VERSION,
            partition
        );
        for path in json_files(&dirs.cache) {
            let is_same_server = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".tools.json"));
            if is_same_server && path != keep {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

/// Roughly how many characters one tool definition is: the namespaced name, the description, the
/// schema.
fn spec_chars(server_id: &str, spec: &ToolSpec) -> usize {
    crate::def::tool_name(server_id, &spec.name).chars().count()
        + spec
            .description
            .as_deref()
            .map(|d| d.chars().count())
            .unwrap_or(0)
        + serde_json::to_string(&spec.input_schema)
            .map(|s| s.chars().count())
            .unwrap_or(0)
}

async fn fetch_tools(
    pool: &McpPool,
    def: &ServerDef,
    workspace_root: &Path,
) -> crate::Result<Vec<ToolSpec>> {
    let resolved = Resolver::system(workspace_root).resolve(def, None)?;
    let label = Label {
        workspace_root: workspace_root.to_path_buf(),
        session: None,
        turn: None,
        call: None,
        interaction: None,
    };
    let session = pool.session(def, &resolved, label).await?;
    Ok(session
        .list_tools()
        .await?
        .iter()
        .map(ToolSpec::from_rmcp)
        .collect())
}

/// A file name that cannot escape the cache directory, whatever the id was.
fn file_stem(id: &str) -> String {
    let mapped: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let bounded: String = mapped.chars().take(64).collect();
    if bounded.is_empty() {
        "server".into()
    } else {
        bounded
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CacheFile {
    /// Informational: the file name already carries identity. Here so the file is readable on its
    /// own, which matters when somebody is looking at a cache directory wondering what is in it.
    server: String,
    fetched_at: chrono::DateTime<chrono::Utc>,
    tools: Vec<ToolSpec>,
}

/// The servers of one workspace, as tools.
pub struct McpSource {
    entries: Vec<(Arc<ServerDef>, Vec<Arc<ToolSpec>>)>,
    pool: Arc<McpPool>,
}

impl ToolSource for McpSource {
    fn name(&self) -> &'static str {
        "mcp"
    }

    fn discover(&self) -> Vec<Arc<dyn Tool>> {
        self.entries
            .iter()
            .flat_map(|(def, tools)| {
                tools.iter().map(move |spec| {
                    Arc::new(McpTool::new(def.clone(), spec.clone(), self.pool.clone()))
                        as Arc<dyn Tool>
                })
            })
            .collect()
    }
}

/// Fixtures shared by this module's test modules.
#[cfg(test)]
mod tests_support {
    use super::*;
    use crate::pool::PoolConfig;
    use crate::pool::testing::{FakeConnector, fake_pool};

    pub(super) struct Fixture {
        pub(super) _tmp: tempfile::TempDir,
        pub(super) dirs: CatalogDirs,
        pub(super) workspace: PathBuf,
    }

    pub(super) fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let dirs = CatalogDirs {
            global_file: root.join("config/mcp.json"),
            global_dir: root.join("data/extensions/mcp"),
            cache: root.join("cache/mcp"),
        };
        let workspace = root.join("work");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&dirs.global_dir).unwrap();
        std::fs::create_dir_all(dirs.global_file.parent().unwrap()).unwrap();
        Fixture {
            _tmp: tmp,
            dirs,
            workspace,
        }
    }

    pub(super) fn write(path: &Path, value: serde_json::Value) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_string_pretty(&value).unwrap()).unwrap();
    }

    pub(super) fn catalog_with_limits(
        f: &Fixture,
        limits: RefreshLimits,
    ) -> (Catalog, Arc<FakeConnector>) {
        let (catalog, connector) = catalog(f);
        (catalog.with_limits(limits), connector)
    }

    pub(super) fn catalog(f: &Fixture) -> (Catalog, Arc<FakeConnector>) {
        let connector = Arc::new(FakeConnector::default());
        let pool = fake_pool(connector.clone(), PoolConfig::default());
        (Catalog::new(f.dirs.clone(), pool), connector)
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::*;
    use super::*;
    use crate::def::TransportDef;
    use serde_json::json;

    #[test]
    fn definitions_are_found_in_all_four_places() {
        let f = fixture();
        write(
            &f.dirs.global_file,
            json!({ "mcpServers": { "hand": { "command": "a" } } }),
        );
        write(
            &f.dirs.global_dir.join("installed.json"),
            json!({ "command": "b" }),
        );
        write(
            &f.workspace.join(".mcp.json"),
            json!({ "mcpServers": { "repo": { "command": "c" } } }),
        );
        let (cat, _) = catalog(&f);

        let contributed = vec![ServerDef {
            id: "plug.srv".into(),
            label: None,
            enabled: None,
            transport: TransportDef::Stdio {
                command: "d".into(),
                args: vec![],
                env: Default::default(),
                cwd: None,
            },
            binding: Default::default(),
            filter: Default::default(),
            origin: Origin::Plugin {
                plugin: "plug".into(),
                workspace: false,
            },
            file: None,
        }];

        let loaded = cat.load(&f.workspace, contributed);
        let ids: Vec<&str> = loaded.servers.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(&"hand"));
        assert!(
            ids.contains(&"installed"),
            "the file stem is the id: {ids:?}"
        );
        assert!(ids.contains(&"repo"));
        assert!(ids.contains(&"plug.srv"));
        assert!(loaded.problems.is_empty(), "{:?}", loaded.problems);
    }

    /// A repository that pins a server must win over the user's own install of the same id — and the
    /// user can see why, because the file is in their checkout.
    #[test]
    fn the_workspace_wins_over_a_global_definition_of_the_same_id() {
        let f = fixture();
        write(
            &f.dirs.global_dir.join("fs.json"),
            json!({ "command": "global-fs" }),
        );
        write(
            &f.workspace.join(".mcp.json"),
            json!({ "mcpServers": { "fs": { "command": "repo-fs" } } }),
        );
        let (cat, _) = catalog(&f);

        let loaded = cat.load(&f.workspace, vec![]);
        assert_eq!(loaded.servers.len(), 1, "not two rows for one id");
        let server = loaded.get("fs").unwrap();
        assert_eq!(server.origin, Origin::Workspace);
        match &server.transport {
            TransportDef::Stdio { command, .. } => assert_eq!(command, "repo-fs"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn nothing_configured_is_not_a_problem() {
        let f = fixture();
        let (cat, _) = catalog(&f);
        let loaded = cat.load(&f.workspace, vec![]);
        assert!(loaded.servers.is_empty());
        assert!(
            loaded.problems.is_empty(),
            "an empty install is the normal case"
        );
        assert_eq!(cat.source(&loaded).discover().len(), 0);
    }

    #[test]
    fn an_unreadable_file_is_a_problem_that_names_it_and_the_rest_still_load() {
        let f = fixture();
        std::fs::write(f.dirs.global_dir.join("broken.json"), "{ nope").unwrap();
        write(
            &f.dirs.global_dir.join("fine.json"),
            json!({ "command": "a" }),
        );
        let (cat, _) = catalog(&f);

        let loaded = cat.load(&f.workspace, vec![]);
        assert_eq!(loaded.servers.len(), 1);
        assert_eq!(loaded.problems.len(), 1);
        assert!(loaded.problems[0].to_string().contains("broken.json"));
    }

    /// The claim that makes `discover` synchronous: no cache, no tools, no connection.
    #[tokio::test]
    async fn a_server_contributes_no_tools_until_its_list_has_been_fetched() {
        let f = fixture();
        write(
            &f.dirs.global_dir.join("s.json"),
            json!({ "command": "a", "cwd": "/fixed" }),
        );
        let (cat, connector) = catalog(&f);

        let mut loaded = cat.load(&f.workspace, vec![]);
        assert_eq!(cat.source(&loaded).discover().len(), 0);
        assert_eq!(
            connector.opens.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "nothing started"
        );
        assert_eq!(loaded.needs_refresh(chrono::Utc::now()).len(), 1);

        connector.tools(vec!["alpha", "beta"]);
        cat.refresh(&mut loaded, &f.workspace).await;

        assert_eq!(loaded.tool_count(), 2);
        let names: Vec<String> = cat
            .source(&loaded)
            .discover()
            .iter()
            .map(|t| t.meta().name)
            .collect();
        assert_eq!(names, ["mcp__s__alpha", "mcp__s__beta"]);
    }

    /// And the next load needs no connection at all — the point of the cache.
    #[tokio::test]
    async fn a_cached_list_is_reused_without_connecting() {
        let f = fixture();
        write(
            &f.dirs.global_dir.join("s.json"),
            json!({ "command": "a", "cwd": "/fixed" }),
        );
        let (cat, connector) = catalog(&f);
        connector.tools(vec!["alpha"]);

        let mut loaded = cat.load(&f.workspace, vec![]);
        cat.refresh(&mut loaded, &f.workspace).await;
        let opens = connector.opens.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(opens, 1);

        // A fresh catalogue and a fresh pool: only the file on disk carries over.
        let (cat2, connector2) = catalog(&f);
        let mut loaded2 = cat2.load(&f.workspace, vec![]);
        assert_eq!(loaded2.tool_count(), 1, "read from the cache");
        assert!(loaded2.needs_refresh(chrono::Utc::now()).is_empty());
        cat2.refresh(&mut loaded2, &f.workspace).await;
        assert_eq!(
            connector2.opens.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    /// Editing the command must not leave the model offered tools it does not have.
    #[tokio::test]
    async fn changing_the_definition_invalidates_the_cached_list() {
        let f = fixture();
        let path = f.dirs.global_dir.join("s.json");
        write(&path, json!({ "command": "old", "cwd": "/fixed" }));
        let (cat, connector) = catalog(&f);
        connector.tools(vec!["alpha"]);
        let mut loaded = cat.load(&f.workspace, vec![]);
        cat.refresh(&mut loaded, &f.workspace).await;
        assert_eq!(loaded.tool_count(), 1);

        write(&path, json!({ "command": "new", "cwd": "/fixed" }));
        let (cat2, _) = catalog(&f);
        let loaded2 = cat2.load(&f.workspace, vec![]);
        assert_eq!(
            loaded2.tool_count(),
            0,
            "the old list belongs to the old command"
        );
        assert_eq!(loaded2.needs_refresh(chrono::Utc::now()).len(), 1);
    }

    /// A cache directory with a file per edit is a directory nobody can read.
    #[tokio::test]
    async fn refetching_after_an_edit_leaves_one_cache_file() {
        let f = fixture();
        let path = f.dirs.global_dir.join("s.json");
        let (cat, connector) = catalog(&f);
        connector.tools(vec!["alpha"]);

        for command in ["one", "two", "three"] {
            write(&path, json!({ "command": command, "cwd": "/fixed" }));
            let mut loaded = cat.load(&f.workspace, vec![]);
            cat.refresh(&mut loaded, &f.workspace).await;
        }
        assert_eq!(json_files(&f.dirs.cache).len(), 1);
    }

    /// A server that will not start must not empty a tool set that was working.
    #[tokio::test]
    async fn a_failed_refresh_keeps_the_previous_list_and_says_so() {
        let f = fixture();
        write(
            &f.dirs.global_dir.join("s.json"),
            json!({ "command": "a", "cwd": "/fixed" }),
        );
        let (cat, connector) = catalog(&f);
        connector.tools(vec!["alpha"]);
        let mut loaded = cat.load(&f.workspace, vec![]);
        cat.refresh(&mut loaded, &f.workspace).await;

        // Force the list to look stale, then take the server away: dropped from the pool (as a
        // crashed one would be) and refusing to come back.
        loaded.tools.get_mut("s").unwrap().fetched_at =
            chrono::Utc::now() - chrono::TimeDelta::days(2);
        connector
            .fail_until
            .store(usize::MAX, std::sync::atomic::Ordering::SeqCst);
        cat.pool().evict_server("s");
        cat.refresh(&mut loaded, &f.workspace).await;

        assert_eq!(
            loaded.tool_count(),
            1,
            "the working list survives a failed refresh"
        );
        assert_eq!(loaded.problems.len(), 1);
        assert!(
            loaded.problems[0]
                .reason
                .contains("last successful connection"),
            "{}",
            loaded.problems[0].reason
        );
    }

    /// A server nobody can reach and that has never been reached is a problem the user has to see.
    #[tokio::test]
    async fn a_server_that_never_connected_is_reported() {
        let f = fixture();
        write(
            &f.dirs.global_dir.join("gh.json"),
            json!({ "command": "a", "cwd": "/fixed" }),
        );
        let (cat, connector) = catalog(&f);
        connector
            .fail_until
            .store(usize::MAX, std::sync::atomic::Ordering::SeqCst);

        let mut loaded = cat.load(&f.workspace, vec![]);
        cat.refresh(&mut loaded, &f.workspace).await;
        assert_eq!(loaded.tool_count(), 0);
        assert_eq!(loaded.problems.len(), 1);
        assert_eq!(loaded.problems[0].server.as_deref(), Some("gh"));
    }

    #[tokio::test]
    async fn a_disabled_server_is_never_connected_and_offers_nothing() {
        let f = fixture();
        write(
            &f.dirs.global_dir.join("s.json"),
            json!({ "command": "a", "cwd": "/fixed", "enabled": false }),
        );
        let (cat, connector) = catalog(&f);
        connector.tools(vec!["alpha"]);

        let mut loaded = cat.load(&f.workspace, vec![]);
        assert!(loaded.needs_refresh(chrono::Utc::now()).is_empty());
        cat.refresh(&mut loaded, &f.workspace).await;
        assert_eq!(connector.opens.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(cat.source(&loaded).discover().len(), 0);
        assert_eq!(
            loaded.servers.len(),
            1,
            "it is still listed, just not offered"
        );
    }

    #[tokio::test]
    async fn a_stale_list_is_refetched() {
        let f = fixture();
        write(
            &f.dirs.global_dir.join("s.json"),
            json!({ "command": "a", "cwd": "/fixed" }),
        );
        let (cat, connector) = catalog(&f);
        connector.tools(vec!["alpha"]);
        let mut loaded = cat.load(&f.workspace, vec![]);
        cat.refresh(&mut loaded, &f.workspace).await;

        loaded.tools.get_mut("s").unwrap().fetched_at =
            chrono::Utc::now() - chrono::TimeDelta::days(2);
        assert_eq!(loaded.needs_refresh(chrono::Utc::now()).len(), 1);

        // The server was restarted with one tool more. A live connection is reused as it is — only a
        // reconnection can report a different list, which is why the old one is dropped here.
        connector.tools(vec!["alpha", "gamma"]);
        cat.pool().evict_server("s");
        cat.refresh(&mut loaded, &f.workspace).await;
        assert_eq!(
            loaded.tool_count(),
            2,
            "a server that grew a tool is picked up"
        );
    }

    /// A server offering nothing is different from a server that works, and the distinction is
    /// reachable rather than guessed at from an empty registry.
    #[tokio::test]
    async fn a_server_with_no_tools_is_visible_as_silent() {
        let f = fixture();
        write(
            &f.dirs.global_dir.join("s.json"),
            json!({ "command": "a", "cwd": "/fixed" }),
        );
        let (cat, connector) = catalog(&f);
        connector.tools(vec![]);

        let mut loaded = cat.load(&f.workspace, vec![]);
        cat.refresh(&mut loaded, &f.workspace).await;
        assert_eq!(loaded.silent_servers(), ["s"]);
    }

    /// A server that could not be reached is one problem, not also a "silent" one — the user must
    /// not be told twice about a single failure.
    #[tokio::test]
    async fn a_server_that_never_answered_is_not_reported_as_silent() {
        let f = fixture();
        write(
            &f.dirs.global_dir.join("gh.json"),
            json!({ "command": "a", "cwd": "/fixed" }),
        );
        let (cat, connector) = catalog(&f);
        connector
            .fail_until
            .store(usize::MAX, std::sync::atomic::Ordering::SeqCst);

        let mut loaded = cat.load(&f.workspace, vec![]);
        cat.refresh(&mut loaded, &f.workspace).await;
        assert_eq!(loaded.problems.len(), 1);
        assert!(
            loaded.silent_servers().is_empty(),
            "it has no list, not an empty one"
        );
    }

    /// A host's policy has to be able to remove a server *before* anything connects — that is the
    /// whole point of the hook.
    #[tokio::test]
    async fn a_retained_out_server_is_never_started() {
        let f = fixture();
        write(
            &f.dirs.global_dir.join("keep.json"),
            json!({ "command": "a", "cwd": "/fixed" }),
        );
        write(
            &f.dirs.global_dir.join("drop.json"),
            json!({ "command": "b", "cwd": "/fixed" }),
        );
        let (cat, connector) = catalog(&f);
        connector.tools(vec!["alpha"]);

        let mut loaded = cat.load(&f.workspace, vec![]);
        assert_eq!(loaded.servers.len(), 2);
        loaded.retain(|def| def.id == "keep");

        cat.refresh(&mut loaded, &f.workspace).await;
        assert_eq!(
            connector.opens.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "only the one that survived"
        );
        assert_eq!(loaded.servers.len(), 1);
        let names: Vec<String> = cat
            .source(&loaded)
            .discover()
            .iter()
            .map(|t| t.meta().name)
            .collect();
        assert_eq!(names, ["mcp__keep__alpha"]);
    }

    /// And a server removed *after* its list was cached takes the list with it, so nothing it offered
    /// survives in the registry.
    #[tokio::test]
    async fn retaining_also_drops_the_cached_list() {
        let f = fixture();
        write(
            &f.dirs.global_dir.join("s.json"),
            json!({ "command": "a", "cwd": "/fixed" }),
        );
        let (cat, connector) = catalog(&f);
        connector.tools(vec!["alpha"]);
        let mut loaded = cat.load(&f.workspace, vec![]);
        cat.refresh(&mut loaded, &f.workspace).await;
        assert_eq!(loaded.tool_count(), 1);

        loaded.retain(|_| false);
        assert_eq!(loaded.tool_count(), 0);
        assert_eq!(cat.source(&loaded).discover().len(), 0);
        assert!(
            loaded.needs_refresh(chrono::Utc::now()).is_empty(),
            "no servers, nothing to fetch"
        );
    }

    #[test]
    fn what_the_writer_writes_the_loader_finds() {
        let f = fixture();
        write(
            &f.dirs.global_dir.join("s.json"),
            json!({ "command": "a", "cwd": "/fixed" }),
        );
        let (cat, _) = catalog(&f);
        let def = cat.load(&f.workspace, vec![]).servers[0].clone();

        let specs = vec![ToolSpec {
            name: "alpha".into(),
            description: Some("d".into()),
            input_schema: json!({ "type": "object", "properties": {} }),
            hints: Default::default(),
        }];
        write_tool_cache(cat.dirs(), &def, &f.workspace, &specs, chrono::Utc::now()).unwrap();

        let loaded = cat.load(&f.workspace, vec![]);
        assert_eq!(loaded.tool_count(), 1);
        assert!(
            loaded.needs_refresh(chrono::Utc::now()).is_empty(),
            "once written, it must not be connected to again"
        );
    }

    /// The cache must follow the resolved connection boundary, not just the literal definition.
    /// A plain stdio server inherits the workspace as cwd and may expose a different list there.
    #[tokio::test]
    async fn workspace_dependent_servers_do_not_share_tool_list_cache() {
        let f = fixture();
        write(&f.dirs.global_dir.join("s.json"), json!({ "command": "a" }));
        let other = f._tmp.path().join("other-workspace");
        std::fs::create_dir_all(&other).unwrap();
        let (cat, connector) = catalog(&f);

        connector.tools(vec!["alpha"]);
        let mut first = cat.load(&f.workspace, vec![]);
        cat.refresh(&mut first, &f.workspace).await;
        assert_eq!(first.tools_of("s")[0].name, "alpha");

        connector.tools(vec!["beta"]);
        let mut second = cat.load(&other, vec![]);
        assert_eq!(
            second.tool_count(),
            0,
            "another workspace must not inherit the first workspace's list"
        );
        cat.refresh(&mut second, &other).await;
        assert_eq!(second.tools_of("s")[0].name, "beta");

        let first_again = cat.load(&f.workspace, vec![]);
        assert_eq!(first_again.tools_of("s")[0].name, "alpha");
    }

    /// Fixed launch parameters resolve to one connection everywhere, so their list remains safely
    /// shareable and should not be fetched once per workspace.
    #[tokio::test]
    async fn workspace_independent_servers_share_tool_list_cache() {
        let f = fixture();
        write(
            &f.dirs.global_dir.join("s.json"),
            json!({ "command": "a", "cwd": "/fixed" }),
        );
        let other = f._tmp.path().join("other-workspace");
        std::fs::create_dir_all(&other).unwrap();
        let (cat, connector) = catalog(&f);
        connector.tools(vec!["alpha"]);

        let mut first = cat.load(&f.workspace, vec![]);
        cat.refresh(&mut first, &f.workspace).await;
        let second = cat.load(&other, vec![]);

        assert_eq!(second.tools_of("s")[0].name, "alpha");
        assert!(second.needs_refresh(chrono::Utc::now()).is_empty());
        assert_eq!(connector.opens.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn runtime_health_does_not_overwrite_another_workspace() {
        let f = fixture();
        write(&f.dirs.global_dir.join("s.json"), json!({ "command": "a" }));
        let other = f._tmp.path().join("other-workspace");
        let (cat, _) = catalog(&f);
        let def = cat.load(&f.workspace, vec![]).servers[0].clone();
        let first_scope = cache_scope(&def, &f.workspace).unwrap();
        let second_scope = cache_scope(&def, &other).unwrap();

        cat.set_runtime_status_scoped("s", &first_scope, RuntimeStatus::Ready);
        cat.set_runtime_status_scoped(
            "s",
            &second_scope,
            RuntimeStatus::Unavailable("not reachable here".into()),
        );

        assert_eq!(
            cat.runtime_status_for(&def, &f.workspace),
            Some(RuntimeStatus::Ready)
        );
        assert_eq!(
            cat.runtime_status_for(&def, &other),
            Some(RuntimeStatus::Unavailable("not reachable here".into()))
        );
        assert_eq!(
            cat.runtime_status("s"),
            Some(RuntimeStatus::Unavailable("not reachable here".into())),
            "the unscoped management view reports the most actionable state"
        );
    }

    #[test]
    fn a_cache_file_name_cannot_escape_the_cache_directory() {
        assert_eq!(file_stem("../../etc/passwd"), "______etc_passwd");
        assert_eq!(file_stem("plug.srv"), "plug_srv");
        assert_eq!(file_stem(""), "server");
        assert_eq!(file_stem(&"server".repeat(100)).chars().count(), 64);
    }

    #[test]
    fn the_workspace_files_are_the_ecosystem_standard_first() {
        let files = CatalogDirs::workspace_files(Path::new("/w"));
        assert_eq!(files[0], PathBuf::from("/w/.mcp.json"));
        assert_eq!(files[1], PathBuf::from("/w/.zlogic/mcp.json"));
    }
}

#[cfg(test)]
mod limits_tests {
    use super::tests_support::*;
    use super::*;
    use serde_json::json;
    use std::sync::atomic::Ordering;

    #[tokio::test]
    async fn an_allowlist_narrows_what_reaches_the_registry() {
        let f = fixture();
        write(
            &f.dirs.global_dir.join("fs.json"),
            json!({ "command": "a", "cwd": "/fixed", "tools": ["read", "nope"] }),
        );
        let (cat, connector) = catalog(&f);
        connector.tools(vec!["read", "write", "delete"]);

        let mut loaded = cat.load(&f.workspace, vec![]);
        cat.refresh(&mut loaded, &f.workspace).await;

        let names: Vec<String> = cat
            .source(&loaded)
            .discover()
            .iter()
            .map(|t| t.meta().name)
            .collect();
        assert_eq!(
            names,
            ["mcp__fs__read"],
            "only the allowlisted one gets through"
        );
        assert_eq!(loaded.problems.len(), 1);
        assert!(
            loaded.problems[0].reason.contains("nope"),
            "{}",
            loaded.problems[0].reason
        );
        assert!(
            loaded.problems[0].reason.contains("write"),
            "the message must spell out what the server actually has"
        );
    }

    #[tokio::test]
    async fn a_denylist_removes_just_those() {
        let f = fixture();
        write(
            &f.dirs.global_dir.join("fs.json"),
            json!({ "command": "a", "cwd": "/fixed", "exclude": ["delete"] }),
        );
        let (cat, connector) = catalog(&f);
        connector.tools(vec!["read", "delete"]);

        let mut loaded = cat.load(&f.workspace, vec![]);
        cat.refresh(&mut loaded, &f.workspace).await;
        let names: Vec<String> = cat
            .source(&loaded)
            .discover()
            .iter()
            .map(|t| t.meta().name)
            .collect();
        assert_eq!(names, ["mcp__fs__read"]);
        assert!(
            loaded.problems.is_empty(),
            "names on the denylist need not exist"
        );
    }

    #[tokio::test]
    async fn editing_the_allowlist_costs_no_connection() {
        let f = fixture();
        let path = f.dirs.global_dir.join("fs.json");
        write(&path, json!({ "command": "a", "cwd": "/fixed" }));
        let (cat, connector) = catalog(&f);
        connector.tools(vec!["read", "write"]);

        let mut loaded = cat.load(&f.workspace, vec![]);
        cat.refresh(&mut loaded, &f.workspace).await;
        assert_eq!(loaded.tool_count(), 2);
        let opens = connector.opens.load(Ordering::SeqCst);

        write(
            &path,
            json!({ "command": "a", "cwd": "/fixed", "tools": ["read"] }),
        );
        let (cat2, connector2) = catalog(&f);
        let mut loaded2 = cat2.load(&f.workspace, vec![]);
        assert_eq!(
            loaded2.tool_count(),
            1,
            "the allowlist takes effect immediately"
        );
        assert!(
            loaded2.needs_refresh(chrono::Utc::now()).is_empty(),
            "and no tool list has to be fetched again"
        );
        cat2.refresh(&mut loaded2, &f.workspace).await;
        assert_eq!(connector2.opens.load(Ordering::SeqCst), 0);
        assert_eq!(opens, 1);
    }

    #[tokio::test]
    async fn a_slow_server_is_left_for_the_next_turn() {
        let f = fixture();
        write(
            &f.dirs.global_dir.join("slow.json"),
            json!({ "command": "a", "cwd": "/fixed" }),
        );
        let (cat, connector) = catalog_with_limits(
            &f,
            RefreshLimits {
                deadline: std::time::Duration::from_millis(60),
                ..RefreshLimits::default()
            },
        );
        connector.tools(vec!["alpha"]);
        connector.slow_connect(std::time::Duration::from_millis(400));

        let started = std::time::Instant::now();
        let mut loaded = cat.load(&f.workspace, vec![]);
        cat.refresh(&mut loaded, &f.workspace).await;

        assert!(
            started.elapsed() < std::time::Duration::from_millis(300),
            "it did not wait for the connection to finish"
        );
        assert_eq!(loaded.pending, ["slow"], "it is left out of this turn");
        assert_eq!(loaded.tool_count(), 0);
        assert!(
            loaded.problems.is_empty(),
            "still fetching is not a problem"
        );

        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let next = cat.load(&f.workspace, vec![]);
            if next.tool_count() == 1 {
                assert_eq!(cat.runtime_status("slow"), Some(RuntimeStatus::Ready));
                return;
            }
        }
        panic!("the background fetch did not write the cache");
    }

    #[tokio::test]
    async fn a_detached_failed_fetch_updates_runtime_health() {
        let f = fixture();
        write(
            &f.dirs.global_dir.join("slow.json"),
            json!({ "command": "a", "cwd": "/fixed" }),
        );
        let (cat, connector) = catalog_with_limits(
            &f,
            RefreshLimits {
                deadline: std::time::Duration::from_millis(40),
                ..RefreshLimits::default()
            },
        );
        connector.slow_connect(std::time::Duration::from_millis(180));
        connector
            .fail_until
            .store(usize::MAX, std::sync::atomic::Ordering::SeqCst);

        let mut loaded = cat.load(&f.workspace, vec![]);
        cat.refresh(&mut loaded, &f.workspace).await;
        assert_eq!(loaded.pending, ["slow"]);
        assert_eq!(cat.runtime_status("slow"), Some(RuntimeStatus::Connecting));

        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            if matches!(
                cat.runtime_status("slow"),
                Some(RuntimeStatus::Unavailable(_))
            ) {
                return;
            }
        }
        panic!("detached connection failure never reached runtime health");
    }

    #[tokio::test]
    async fn a_fetch_that_outlived_its_turn_is_not_started_twice() {
        let f = fixture();
        write(
            &f.dirs.global_dir.join("slow.json"),
            json!({ "command": "a", "cwd": "/fixed" }),
        );
        let (cat, connector) = catalog_with_limits(
            &f,
            RefreshLimits {
                deadline: std::time::Duration::from_millis(50),
                ..RefreshLimits::default()
            },
        );
        connector.tools(vec!["alpha"]);
        connector.slow_connect(std::time::Duration::from_millis(600));

        let mut first = cat.load(&f.workspace, vec![]);
        cat.refresh(&mut first, &f.workspace).await;
        assert_eq!(first.pending, ["slow"]);

        let mut second = cat.load(&f.workspace, vec![]);
        cat.refresh(&mut second, &f.workspace).await;
        assert_eq!(
            second.pending,
            ["slow"],
            "it still reports the fetch as in flight"
        );
        assert_eq!(
            connector.opens.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "there should be only one connection attempt"
        );

        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let next = cat.load(&f.workspace, vec![]);
            if next.tool_count() == 1 {
                assert!(next.pending.is_empty());
                return;
            }
        }
        panic!("the background fetch did not write the cache");
    }

    #[tokio::test]
    async fn the_token_estimate_names_the_biggest_servers() {
        let f = fixture();
        write(
            &f.dirs.global_dir.join("small.json"),
            json!({ "command": "a", "cwd": "/1" }),
        );
        write(
            &f.dirs.global_dir.join("big.json"),
            json!({ "command": "b", "cwd": "/2" }),
        );
        let (cat, connector) = catalog(&f);

        connector.tools(vec!["one"]);
        let mut loaded = cat.load(&f.workspace, vec![]);
        loaded.retain(|d| d.id == "small");
        cat.refresh(&mut loaded, &f.workspace).await;

        connector.tools(vec!["a", "b", "c", "d", "e", "f", "g", "h"]);
        let mut all = cat.load(&f.workspace, vec![]);
        cat.refresh(&mut all, &f.workspace).await;

        let estimates = all.token_estimates();
        assert_eq!(estimates.len(), 2);
        assert_eq!(estimates[0].0, "big", "largest first: {estimates:?}");
        assert!(estimates[0].1 > estimates[1].1);
        assert_eq!(
            all.estimated_tokens(),
            estimates.iter().map(|(_, t)| t).sum::<usize>()
        );
    }

    #[test]
    fn nothing_configured_estimates_zero() {
        let f = fixture();
        let (cat, _) = catalog(&f);
        let loaded = cat.load(&f.workspace, vec![]);
        assert_eq!(loaded.estimated_tokens(), 0);
        assert!(loaded.token_estimates().is_empty());
    }
}
