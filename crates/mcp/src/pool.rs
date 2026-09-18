//! The connection pool: which connection serves a call, when one is made, when it goes away.
//! # A connection is looked up per call, never attached to a tool
//! Every `mcp__server__tool` in the registry records a server id and nothing else. The connection is
//! found at call time by hashing the resolved parameters (see [`crate::resolve`]), so reconnecting
//! after a crash, or rebuilding the whole pool, has no effect on the tool catalogue and no effect on
//! what the model sees.
//! # Lazy, and reclaimed when idle
//! Nothing connects until a call needs it — listing tools comes from a cache on disk, so opening a
//! session does not start every configured server. Symmetrically, a connection with no calls for
//! [`PoolConfig::idle_timeout`] is dropped: a stdio server is a process on the user's machine, and
//! keeping twenty of them alive because they were each used once is not acceptable.
//! A dropped [`crate::conn::Connection`] closes itself, so eviction here is just removing the entry —
//! but a caller mid-call holds its own `Arc`, and its call finishes. Eviction is "no new calls go
//! here", never "the work in flight is killed".
//! # A server that crashes is retried with a delay
//! Without a delay, a server whose command does not exist would be re-spawned on every single tool
//! call in a round — dozens of process launches, each failing the same way. The backoff is per pool
//! key, resets on success, and its *only* observable effect is that a failing call comes back
//! immediately with the previous error rather than after another failed launch.
//! # The bookkeeping is testable because connecting is behind a trait
//! `LRU eviction`, idle reclaim and backoff are pure logic and are where the subtle mistakes live,
//! so the pool talks to a [`Connector`] rather than to a child process directly. Production uses
//! [`RmcpConnector`]; the tests in this module use a fake that can be told to fail, to die, or to
//! block — none of which is arrangeable with a real server.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rmcp::model::{CallToolResult, JsonObject, Tool as RmcpTool};

use crate::conn::{Connection, Label};
use crate::def::ServerDef;
use crate::resolve::{PoolKey, Resolved, ResolvedTransport};
use crate::{McpError, Result};

#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// No calls for this long and the connection is dropped.
    pub idle_timeout: Duration,
    /// A backstop, not a budget: the real bound is (servers × workspaces). Reaching this means a
    /// template variable is producing values nobody intended, and LRU eviction keeps that from
    /// becoming an unbounded number of child processes.
    pub max_connections: usize,
    /// Includes the `initialize` handshake — a server that starts but never answers is a failure to
    /// connect, not a connection.
    pub connect_timeout: Duration,
    /// Per `tools/call`. Generous, because some MCP tools legitimately take minutes, but finite:
    /// core's own tool timeout is off by default, so without this a hung server would hang the turn
    /// for as long as the user is willing to watch it.
    pub call_timeout: Duration,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            idle_timeout: Duration::from_secs(10 * 60),
            max_connections: 32,
            connect_timeout: Duration::from_secs(30),
            call_timeout: Duration::from_secs(300),
        }
    }
}

/// What the pool needs from one live session. Implemented by [`Connection`].
#[async_trait]
pub(crate) trait Session: Send + Sync {
    async fn list_tools(&self) -> Result<Vec<RmcpTool>>;
    /// Implementations enforce `timeout` themselves and report [`McpError::Timeout`] — the pool does
    /// not wrap this, because a session knows what winding a request down means and a dropped future
    /// leaves the server working on something nobody will read.
    async fn call_tool(
        &self,
        tool: &str,
        args: Option<JsonObject>,
        timeout: Duration,
    ) -> Result<CallToolResult>;
    /// Whether the session is gone (the child exited, the HTTP session was closed).
    fn is_closed(&self) -> bool;
}

pub(crate) type SharedSession = Arc<dyn Session>;

#[async_trait]
pub(crate) trait Connector: Send + Sync {
    async fn open(
        &self,
        server_id: &str,
        transport: &ResolvedTransport,
        label: Label,
        timeout: Duration,
    ) -> Result<SharedSession>;
}

/// The production connector: a real child process, or a real HTTP session.
pub(crate) struct RmcpConnector;

#[async_trait]
impl Connector for RmcpConnector {
    async fn open(
        &self,
        server_id: &str,
        transport: &ResolvedTransport,
        label: Label,
        timeout: Duration,
    ) -> Result<SharedSession> {
        Ok(Arc::new(
            Connection::open(server_id, transport, label, timeout).await?,
        ))
    }
}

#[async_trait]
impl Session for Connection {
    async fn list_tools(&self) -> Result<Vec<RmcpTool>> {
        Connection::list_tools(self).await
    }

    async fn call_tool(
        &self,
        tool: &str,
        args: Option<JsonObject>,
        timeout: Duration,
    ) -> Result<CallToolResult> {
        Connection::call_tool(self, tool, args, timeout).await
    }

    fn is_closed(&self) -> bool {
        Connection::is_closed(self)
    }
}

pub struct McpPool {
    config: PoolConfig,
    connector: Arc<dyn Connector>,
    slots: Mutex<HashMap<PoolKey, Slot>>,
}

struct Slot {
    server_id: String,
    /// `Some` once a handshake has succeeded. Concurrent first calls share one connect attempt:
    /// four tool calls in one round must not start four copies of the same server.
    cell: Arc<tokio::sync::OnceCell<SharedSession>>,
    last_used: Instant,
    /// Every workspace that has used this connection. A connection whose parameters do not mention
    /// any workspace is legitimately shared, so "the workspace closed" can only evict it once no
    /// remaining user is left.
    users: BTreeSet<PathBuf>,
    consecutive_failures: u32,
    retry_at: Option<Instant>,
    last_error: Option<String>,
}

/// What the UI can say about one connection without holding it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnStatus {
    pub server_id: String,
    pub key: String,
    pub connected: bool,
    pub last_error: Option<String>,
    pub idle_secs: u64,
}

impl McpPool {
    pub fn new(config: PoolConfig) -> Self {
        Self::with_connector(config, Arc::new(RmcpConnector))
    }

    pub(crate) fn with_connector(config: PoolConfig, connector: Arc<dyn Connector>) -> Self {
        Self {
            config,
            connector,
            slots: Mutex::new(HashMap::new()),
        }
    }

    pub fn config(&self) -> &PoolConfig {
        &self.config
    }

    /// Reclaims idle connections on a timer for as long as the pool is alive.
    /// Holds a [`std::sync::Weak`], so dropping the pool ends the task — a strong reference here
    /// would keep every child process running for the life of the process.
    pub fn spawn_reaper(pool: &Arc<Self>) {
        let weak = Arc::downgrade(pool);
        // Half the idle timeout: a connection then lives at most 1.5× its idle window, which is
        // close enough for reclaiming processes and cheap enough to be unnoticeable.
        let period = (pool.config.idle_timeout / 2).max(Duration::from_secs(30));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(period).await;
                match weak.upgrade() {
                    Some(pool) => {
                        let reaped = pool.reap_idle();
                        if reaped > 0 {
                            tracing::debug!(target: "zlogic::mcp", "reclaimed {reaped} idle MCP connection(s)");
                        }
                    }
                    None => break,
                }
            }
        });
    }

    /// The connection for these parameters, connecting if there is not one yet.
    pub(crate) async fn session(
        &self,
        def: &ServerDef,
        resolved: &Resolved,
        label: Label,
    ) -> Result<SharedSession> {
        let now = Instant::now();
        let cell = {
            let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());

            // A dead session must not be handed out: the caller would get a transport error that
            // reads, to the model, like the tool being broken rather than the server having exited.
            if let Some(slot) = slots.get(&resolved.key)
                && slot.cell.get().is_some_and(|s| s.is_closed())
            {
                slots.remove(&resolved.key);
            }

            if let Some(slot) = slots.get_mut(&resolved.key) {
                slot.last_used = now;
                slot.users.insert(label.workspace_root.clone());
                if let Some(session) = slot.cell.get() {
                    return Ok(session.clone());
                }
                // Still inside its backoff window: answer with what went wrong last time rather
                // than spending another launch to rediscover it.
                if let Some(retry_at) = slot.retry_at
                    && retry_at > now
                {
                    return Err(McpError::Connect {
                        server: def.id.clone(),
                        reason: slot
                            .last_error
                            .clone()
                            .unwrap_or_else(|| "the last connection attempt failed".into()),
                    });
                }
                slot.cell.clone()
            } else {
                self.enforce_cap(&mut slots);
                let cell = Arc::new(tokio::sync::OnceCell::new());
                slots.insert(
                    resolved.key.clone(),
                    Slot {
                        server_id: def.id.clone(),
                        cell: cell.clone(),
                        last_used: now,
                        users: [label.workspace_root.clone()].into_iter().collect(),
                        consecutive_failures: 0,
                        retry_at: None,
                        last_error: None,
                    },
                );
                cell
            }
        };

        // Connecting happens **outside** the lock: a server that takes ten seconds to start must not
        // block calls to every other server. `OnceCell` is what keeps concurrent callers to one
        // attempt; a failed attempt leaves the cell empty, so the next call retries it.
        let outcome = cell
            .get_or_try_init(|| async {
                self.connector
                    .open(
                        &def.id,
                        &resolved.transport,
                        label.clone(),
                        self.config.connect_timeout,
                    )
                    .await
            })
            .await;

        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        match outcome {
            Ok(session) => {
                if let Some(slot) = slots.get_mut(&resolved.key) {
                    slot.consecutive_failures = 0;
                    slot.retry_at = None;
                    slot.last_error = None;
                }
                Ok(session.clone())
            }
            Err(e) => {
                if let Some(slot) = slots.get_mut(&resolved.key) {
                    slot.consecutive_failures += 1;
                    // The **reason**, not the rendered error: the replay above wraps it in a
                    // `Connect` again, and storing the rendered form would nest the sentence one
                    // level deeper on every retry ("server `x` is unavailable: server `x` is
                    // unavailable: …"). Callers also compare these strings — a notice deduplicated
                    // by message would fire again every round.
                    slot.last_error = Some(match &e {
                        McpError::Connect { reason, .. } => reason.clone(),
                        other => other.to_string(),
                    });
                    slot.retry_at = Some(Instant::now() + backoff(slot.consecutive_failures));
                }
                Err(e)
            }
        }
    }

    /// Drops connections unused for longer than the idle timeout. Returns how many.
    pub fn reap_idle(&self) -> usize {
        self.reap_idle_at(Instant::now())
    }

    pub(crate) fn reap_idle_at(&self, now: Instant) -> usize {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let before = slots.len();
        slots.retain(|_, slot| {
            let idle = now.saturating_duration_since(slot.last_used);
            // A slot that has never connected is bookkeeping (a backoff window), and expires the
            // same way — otherwise a server that failed once would keep its entry for ever.
            idle < self.config.idle_timeout
        });
        before - slots.len()
    }

    /// Everything belonging to one server. Used when a definition changes, is disabled, is removed,
    /// or its credentials change — all of which make the running connection the wrong one.
    pub fn evict_server(&self, server_id: &str) -> usize {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let before = slots.len();
        slots.retain(|_, slot| slot.server_id != server_id);
        before - slots.len()
    }

    /// Called when a workspace closes. Only drops connections no other workspace is using — a
    /// connection whose parameters never mentioned a workspace is shared, and closing one project
    /// must not disconnect the others.
    pub fn evict_workspace(&self, root: &std::path::Path) -> usize {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let before = slots.len();
        slots.retain(|_, slot| {
            slot.users.remove(root);
            !slot.users.is_empty()
        });
        before - slots.len()
    }

    /// Called when a session is deleted. Only session-bound connections are keyed by session, so
    /// this touches nothing else.
    pub fn evict_session(&self, session: zlogic_protocol::SessionId) -> usize {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let before = slots.len();
        slots.retain(|key, _| key.session() != Some(session));
        before - slots.len()
    }

    /// Drops everything. Connections close as their last `Arc` goes away.
    pub fn shutdown(&self) {
        self.slots.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    pub fn status(&self) -> Vec<ConnStatus> {
        let now = Instant::now();
        let slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let mut out: Vec<ConnStatus> = slots
            .iter()
            .map(|(key, slot)| ConnStatus {
                server_id: slot.server_id.clone(),
                key: key.to_string(),
                connected: slot.cell.get().is_some_and(|s| !s.is_closed()),
                last_error: slot.last_error.clone(),
                idle_secs: now.saturating_duration_since(slot.last_used).as_secs(),
            })
            .collect();
        out.sort_by(|a, b| (&a.server_id, &a.key).cmp(&(&b.server_id, &b.key)));
        out
    }

    pub fn len(&self) -> usize {
        self.slots.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Makes room for one more, dropping the least recently used entry.
    fn enforce_cap(&self, slots: &mut HashMap<PoolKey, Slot>) {
        while slots.len() >= self.config.max_connections {
            let Some(victim) = slots
                .iter()
                .min_by_key(|(_, slot)| slot.last_used)
                .map(|(key, _)| key.clone())
            else {
                return;
            };
            tracing::warn!(
                target: "zlogic::mcp",
                "MCP connection pool is at its {} limit; dropping the least recently used one",
                self.config.max_connections
            );
            slots.remove(&victim);
        }
    }
}

/// 1s, 2s, 4s … capped at 30s.
fn backoff(failures: u32) -> Duration {
    let secs = 1u64 << failures.saturating_sub(1).min(5);
    Duration::from_secs(secs.min(30))
}

/// Fakes shared by this crate's tests.
/// A real server cannot be told to hang, to die mid-session, or to refuse the next three
/// connections — and those are exactly the paths worth testing.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Default)]
    pub(crate) struct FakeConnector {
        pub(crate) opens: AtomicUsize,
        pub(crate) connect_delay: Mutex<Duration>,
        /// Attempts up to and including this number fail.
        pub(crate) fail_until: AtomicUsize,
        sessions: Mutex<Vec<Arc<FakeSession>>>,
        behaviour: Mutex<Behaviour>,
        /// What `tools/list` answers.
        tools: Mutex<Vec<String>>,
        /// The last `tools/call` any of its sessions received.
        last_call: Arc<Mutex<Option<(String, Option<JsonObject>)>>>,
    }

    #[derive(Default, Clone)]
    enum Behaviour {
        #[default]
        Empty,
        Reply(CallToolResult),
        /// Never answers, so a timeout or a cancellation can be observed.
        Hang,
    }

    impl FakeConnector {
        pub(crate) fn reply(&self, result: CallToolResult) {
            *self.behaviour.lock().unwrap() = Behaviour::Reply(result);
        }

        pub(crate) fn slow_connect(&self, delay: Duration) {
            *self.connect_delay.lock().unwrap() = delay;
        }

        pub(crate) fn hang(&self) {
            *self.behaviour.lock().unwrap() = Behaviour::Hang;
        }

        /// What every session opened from here reports for `tools/list`.
        pub(crate) fn tools(&self, names: Vec<&str>) {
            *self.tools.lock().unwrap() = names.into_iter().map(str::to_string).collect();
        }

        pub(crate) fn last_tool(&self) -> Option<String> {
            self.last_call
                .lock()
                .unwrap()
                .as_ref()
                .map(|(t, _)| t.clone())
        }

        pub(crate) fn last_args(&self) -> Option<serde_json::Value> {
            self.last_call
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|(_, a)| a.clone())
                .map(serde_json::Value::Object)
        }

        pub(crate) fn kill_all(&self) {
            for s in self.sessions.lock().unwrap().iter() {
                s.closed.store(true, Ordering::Relaxed);
            }
        }
    }

    pub(crate) struct FakeSession {
        pub(crate) closed: AtomicBool,
        behaviour: Behaviour,
        tools: Vec<String>,
        last_call: Arc<Mutex<Option<(String, Option<JsonObject>)>>>,
    }

    #[async_trait]
    impl Session for FakeSession {
        async fn list_tools(&self) -> Result<Vec<RmcpTool>> {
            Ok(self
                .tools
                .iter()
                .map(|name| {
                    RmcpTool::new(
                        name.clone(),
                        format!("the {name} tool"),
                        Arc::new(rmcp::model::JsonObject::new()),
                    )
                })
                .collect())
        }

        async fn call_tool(
            &self,
            tool: &str,
            args: Option<JsonObject>,
            timeout: Duration,
        ) -> Result<CallToolResult> {
            *self.last_call.lock().unwrap() = Some((tool.to_string(), args));
            match &self.behaviour {
                Behaviour::Empty => Ok(CallToolResult::success(Vec::new())),
                Behaviour::Reply(r) => Ok(r.clone()),
                // Honouring the timeout is part of the trait's contract, not a courtesy: a fake that
                // ignored it would make a hang look like a hang in zlogic.
                Behaviour::Hang => {
                    tokio::time::sleep(timeout).await;
                    Err(McpError::Timeout {
                        server: "fake".into(),
                        tool: tool.to_string(),
                        secs: timeout.as_secs(),
                    })
                }
            }
        }

        fn is_closed(&self) -> bool {
            self.closed.load(Ordering::Relaxed)
        }
    }

    #[async_trait]
    impl Connector for FakeConnector {
        async fn open(
            &self,
            server_id: &str,
            _transport: &ResolvedTransport,
            _label: Label,
            _timeout: Duration,
        ) -> Result<SharedSession> {
            let n = self.opens.fetch_add(1, Ordering::SeqCst) + 1;
            let delay = *self.connect_delay.lock().unwrap();
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            if n <= self.fail_until.load(Ordering::SeqCst) {
                return Err(McpError::Connect {
                    server: server_id.to_string(),
                    reason: format!("refused attempt {n}"),
                });
            }
            let session = Arc::new(FakeSession {
                closed: AtomicBool::new(false),
                behaviour: self.behaviour.lock().unwrap().clone(),
                tools: self.tools.lock().unwrap().clone(),
                last_call: self.last_call.clone(),
            });
            self.sessions.lock().unwrap().push(session.clone());
            Ok(session)
        }
    }

    pub(crate) fn fake_pool(connector: Arc<FakeConnector>, config: PoolConfig) -> Arc<McpPool> {
        Arc::new(McpPool::with_connector(config, connector))
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{FakeConnector, fake_pool as pool};
    use super::*;
    use crate::def::{Origin, parse_value};
    use crate::resolve::Resolver;
    use serde_json::json;
    use std::sync::atomic::Ordering;

    fn def(id: &str, v: serde_json::Value) -> ServerDef {
        let mut p = parse_value(&v, id, Origin::Global, None);
        assert!(p.problems.is_empty(), "{:?}", p.problems);
        let mut d = p.servers.remove(0);
        d.id = id.to_string();
        d
    }

    fn label(root: &str) -> Label {
        Label {
            workspace_root: PathBuf::from(root),
            session: None,
            turn: None,
            call: None,
            interaction: None,
        }
    }

    async fn resolved(d: &ServerDef, root: &str) -> Resolved {
        Resolver::with_lookups(root, Arc::new(|_| None), Arc::new(|_| None))
            .resolve(d, None)
            .unwrap()
    }

    /// The point of the pool: the second call reuses the first connection.
    #[tokio::test]
    async fn a_second_call_reuses_the_connection() {
        let c = Arc::new(FakeConnector::default());
        let p = pool(c.clone(), PoolConfig::default());
        let d = def("s", json!({ "command": "x", "cwd": "/fixed" }));
        let r = resolved(&d, "/w").await;

        p.session(&d, &r, label("/w")).await.unwrap();
        p.session(&d, &r, label("/w")).await.unwrap();
        assert_eq!(c.opens.load(Ordering::SeqCst), 1);
        assert_eq!(p.len(), 1);
    }

    /// Two workspaces, parameters that never mention one: one connection, and the second workspace
    /// is recorded as a user of it.
    #[tokio::test]
    async fn workspaces_share_a_connection_whose_parameters_do_not_vary() {
        let c = Arc::new(FakeConnector::default());
        let p = pool(c.clone(), PoolConfig::default());
        let d = def("s", json!({ "command": "x", "cwd": "/fixed" }));

        p.session(&d, &resolved(&d, "/w/a").await, label("/w/a"))
            .await
            .unwrap();
        p.session(&d, &resolved(&d, "/w/b").await, label("/w/b"))
            .await
            .unwrap();
        assert_eq!(c.opens.load(Ordering::SeqCst), 1);

        // Closing one workspace must not disconnect the other.
        assert_eq!(p.evict_workspace(std::path::Path::new("/w/a")), 0);
        assert_eq!(p.len(), 1);
        assert_eq!(p.evict_workspace(std::path::Path::new("/w/b")), 1);
        assert!(p.is_empty());
    }

    #[tokio::test]
    async fn parameters_that_mention_the_workspace_get_a_connection_each() {
        let c = Arc::new(FakeConnector::default());
        let p = pool(c.clone(), PoolConfig::default());
        let d = def(
            "fs",
            json!({ "command": "fs", "args": ["${workspaceRoot}"] }),
        );

        p.session(&d, &resolved(&d, "/w/a").await, label("/w/a"))
            .await
            .unwrap();
        p.session(&d, &resolved(&d, "/w/b").await, label("/w/b"))
            .await
            .unwrap();
        assert_eq!(c.opens.load(Ordering::SeqCst), 2);
        assert_eq!(p.len(), 2);
    }

    /// Concurrent first calls in one round must not start several copies of the same server.
    #[tokio::test]
    async fn concurrent_first_calls_share_one_connect() {
        let c = Arc::new(FakeConnector::default());
        let p = pool(c.clone(), PoolConfig::default());
        let d = Arc::new(def("s", json!({ "command": "x", "cwd": "/fixed" })));
        let r = Arc::new(resolved(&d, "/w").await);

        let mut set = Vec::new();
        for _ in 0..8 {
            let (p, d, r) = (p.clone(), d.clone(), r.clone());
            set.push(tokio::spawn(async move {
                p.session(&d, &r, label("/w")).await.map(|_| ())
            }));
        }
        for handle in set {
            handle.await.unwrap().unwrap();
        }
        assert_eq!(c.opens.load(Ordering::SeqCst), 1);
    }

    /// A dead session is replaced rather than handed out.
    #[tokio::test]
    async fn a_closed_session_is_replaced() {
        let c = Arc::new(FakeConnector::default());
        let p = pool(c.clone(), PoolConfig::default());
        let d = def("s", json!({ "command": "x", "cwd": "/fixed" }));
        let r = resolved(&d, "/w").await;

        p.session(&d, &r, label("/w")).await.unwrap();
        c.kill_all();

        p.session(&d, &r, label("/w")).await.unwrap();
        assert_eq!(
            c.opens.load(Ordering::SeqCst),
            2,
            "the dead one must not be reused"
        );
        assert_eq!(p.len(), 1);
    }

    /// A crashing server is not re-launched once per tool call.
    #[tokio::test]
    async fn a_failed_connect_is_not_retried_immediately() {
        let c = Arc::new(FakeConnector::default());
        c.fail_until.store(10, Ordering::SeqCst);
        let p = pool(c.clone(), PoolConfig::default());
        let d = def("s", json!({ "command": "x", "cwd": "/fixed" }));
        let r = resolved(&d, "/w").await;

        let first = p
            .session(&d, &r, label("/w"))
            .await
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(first.contains("refused attempt 1"), "{first}");

        // Inside the backoff window: the previous error comes back without another launch, and it
        // comes back **identically** — a caller deduplicating messages must not see a new one.
        let second = p
            .session(&d, &r, label("/w"))
            .await
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert_eq!(c.opens.load(Ordering::SeqCst), 1, "no second launch");
        assert_eq!(
            second, first,
            "the replayed failure must not nest another sentence"
        );
    }

    /// And once it works, the backoff is gone.
    #[tokio::test]
    async fn a_successful_connect_clears_the_backoff() {
        let c = Arc::new(FakeConnector::default());
        c.fail_until.store(1, Ordering::SeqCst);
        let p = pool(
            c.clone(),
            PoolConfig {
                connect_timeout: Duration::from_secs(1),
                ..PoolConfig::default()
            },
        );
        let d = def("s", json!({ "command": "x", "cwd": "/fixed" }));
        let r = resolved(&d, "/w").await;

        assert!(p.session(&d, &r, label("/w")).await.is_err());
        // Simulate the window having passed rather than sleeping for it.
        {
            let mut slots = p.slots.lock().unwrap();
            slots.get_mut(&r.key).unwrap().retry_at = None;
        }
        assert!(p.session(&d, &r, label("/w")).await.is_ok());
        let status = p.status();
        assert!(status[0].connected);
        assert!(
            status[0].last_error.is_none(),
            "a working connection reports no error"
        );
    }

    #[test]
    fn the_backoff_grows_and_is_capped() {
        assert_eq!(backoff(1), Duration::from_secs(1));
        assert_eq!(backoff(2), Duration::from_secs(2));
        assert_eq!(backoff(3), Duration::from_secs(4));
        assert_eq!(
            backoff(50),
            Duration::from_secs(30),
            "capped, never unbounded"
        );
    }

    #[tokio::test]
    async fn idle_connections_are_reclaimed() {
        let c = Arc::new(FakeConnector::default());
        let p = pool(
            c.clone(),
            PoolConfig {
                idle_timeout: Duration::from_secs(60),
                ..Default::default()
            },
        );
        let d = def("s", json!({ "command": "x", "cwd": "/fixed" }));
        p.session(&d, &resolved(&d, "/w").await, label("/w"))
            .await
            .unwrap();

        assert_eq!(p.reap_idle(), 0, "just used");
        let later = Instant::now() + Duration::from_secs(61);
        assert_eq!(p.reap_idle_at(later), 1);
        assert!(p.is_empty());
    }

    /// The cap is a defence against a misconfigured template, so it must actually bound the pool.
    #[tokio::test]
    async fn the_pool_is_capped_and_evicts_the_least_recently_used() {
        let c = Arc::new(FakeConnector::default());
        let p = pool(
            c.clone(),
            PoolConfig {
                max_connections: 2,
                ..Default::default()
            },
        );

        for i in 0..5 {
            let d = def(
                "fs",
                json!({ "command": "fs", "args": ["${workspaceRoot}"] }),
            );
            let root = format!("/w/{i}");
            p.session(&d, &resolved(&d, &root).await, label(&root))
                .await
                .unwrap();
            assert!(p.len() <= 2, "the cap must hold at every step");
        }
        assert_eq!(p.len(), 2);
    }

    /// Disable, remove, or a rotated credential: everything for that server goes.
    #[tokio::test]
    async fn evicting_a_server_takes_all_of_its_connections() {
        let c = Arc::new(FakeConnector::default());
        let p = pool(c.clone(), PoolConfig::default());
        let fs = def(
            "fs",
            json!({ "command": "fs", "args": ["${workspaceRoot}"] }),
        );
        let gh = def("gh", json!({ "command": "gh", "cwd": "/fixed" }));

        p.session(&fs, &resolved(&fs, "/w/a").await, label("/w/a"))
            .await
            .unwrap();
        p.session(&fs, &resolved(&fs, "/w/b").await, label("/w/b"))
            .await
            .unwrap();
        p.session(&gh, &resolved(&gh, "/w/a").await, label("/w/a"))
            .await
            .unwrap();
        assert_eq!(p.len(), 3);

        assert_eq!(p.evict_server("fs"), 2);
        assert_eq!(p.status().len(), 1);
        assert_eq!(p.status()[0].server_id, "gh");
    }

    /// Only session-bound definitions are keyed by session, so deleting a session must leave the
    /// shared connections alone.
    #[tokio::test]
    async fn evicting_a_session_only_touches_session_bound_connections() {
        let c = Arc::new(FakeConnector::default());
        let p = pool(c.clone(), PoolConfig::default());
        let session = zlogic_protocol::SessionId::new();

        let shared = def("gh", json!({ "command": "gh", "cwd": "/fixed" }));
        let bound = def(
            "browser",
            json!({ "command": "b", "cwd": "/fixed", "binding": "session" }),
        );
        let r_shared = Resolver::with_lookups("/w", Arc::new(|_| None), Arc::new(|_| None))
            .resolve(&shared, Some(session))
            .unwrap();
        let r_bound = Resolver::with_lookups("/w", Arc::new(|_| None), Arc::new(|_| None))
            .resolve(&bound, Some(session))
            .unwrap();

        p.session(&shared, &r_shared, label("/w")).await.unwrap();
        p.session(&bound, &r_bound, label("/w")).await.unwrap();

        assert_eq!(p.evict_session(session), 1);
        assert_eq!(p.status()[0].server_id, "gh");
    }

    #[tokio::test]
    async fn shutdown_drops_everything() {
        let c = Arc::new(FakeConnector::default());
        let p = pool(c.clone(), PoolConfig::default());
        let d = def("s", json!({ "command": "x", "cwd": "/fixed" }));
        p.session(&d, &resolved(&d, "/w").await, label("/w"))
            .await
            .unwrap();
        p.shutdown();
        assert!(p.is_empty());
    }

    /// A failed server leaves bookkeeping behind; that must expire too.
    #[tokio::test]
    async fn a_backoff_entry_expires_like_any_other() {
        let c = Arc::new(FakeConnector::default());
        c.fail_until.store(10, Ordering::SeqCst);
        let p = pool(
            c.clone(),
            PoolConfig {
                idle_timeout: Duration::from_secs(60),
                ..Default::default()
            },
        );
        let d = def("s", json!({ "command": "x", "cwd": "/fixed" }));
        assert!(
            p.session(&d, &resolved(&d, "/w").await, label("/w"))
                .await
                .is_err()
        );
        assert_eq!(p.len(), 1);
        assert_eq!(p.reap_idle_at(Instant::now() + Duration::from_secs(61)), 1);
    }
}
