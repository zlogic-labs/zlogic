//! # zlogic-mcp
//! Tools that live in someone else's process.
//! An MCP server is a program (local, over stdio) or an endpoint (remote, over streamable HTTP) that
//! offers tools. This crate turns configured servers into entries in zlogic's tool registry, and turns
//! a model's call on one of those entries into a JSON-RPC request on a live session.
//! ```text
//! definition file ─┐
//! plugin manifest ─┴→ def::ServerDef ─→ catalog ─→ McpSource: ToolSource ─→ ToolRegistry
//!                                          │                                     │
//!                                    tool list cache                    mcp__server__tool
//!                                     (no connection)                            │
//!                                                          resolve ─→ pool ─→ conn ─→ tools/call
//! ```
//! # Four decisions worth knowing before reading the code
//! **The tool catalogue does not require a connection.** [`zlogic_tools::ToolSource::discover`] is
//! synchronous, and for good reason: assembling the tool set must not start every configured server,
//! and it must not wait on the network. Tool lists therefore come from a cache on disk, written the
//! first time a server is actually reached. See [`catalog`].
//! **A tool records a server id, never a connection.** Which connection serves a call is decided at
//! call time by hashing the resolved launch parameters. Reconnecting after a crash, or discarding the
//! whole pool, is invisible to the registry and to the model. See [`resolve`] and [`pool`].
//! **Sharing is derived, not declared.** There is no `scope: global | workspace` field. A server
//! whose parameters mention the workspace gets one connection per workspace because its parameters
//! differ; one whose parameters are identical everywhere is shared because they are not. The single
//! exception — a server keeping state inside its tools, which MCP gives us no way to detect — is
//! [`def::Binding::Session`].
//! **A failing server is a failing call, never a failing turn.** A server that will not start, a
//! credential that is not set, a call that times out: each comes back as a tool result the model can
//! read and work around. Nothing here aborts a turn.
//! # What this crate deliberately does not do
//! It makes no permission decisions. MCP calls go through the same approval gate as built-in tools,
//! with [`zlogic_tools::ToolMeta::source`] set to `"mcp"` and a risk derived from the server's own tool
//! annotations — a signal for the policy layer, not a verdict.

pub mod catalog;
pub mod conn;
pub mod content;
pub mod def;
pub mod oauth;
pub mod pool;
pub mod resolve;
mod security;
pub mod tool;

use std::path::PathBuf;

pub use catalog::{Catalog, CatalogDirs, Loaded, McpSource, RuntimeStatus, write_tool_cache};
pub use conn::Label;
pub use def::{
    Binding, HttpAuthDef, Origin, Problem, ServerDef, TransportDef, api_key_key,
    oauth_client_secret_key, token_key, tool_name,
};
pub use pool::{ConnStatus, McpPool, PoolConfig};
pub use resolve::{PoolKey, Resolved, ResolvedTransport, Resolver};
pub use tool::{Hints, McpTool, ToolSpec};

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// The file could not be read or is not JSON. A malformed *server* inside a readable file is a
    /// [`Problem`] instead — see [`def`].
    #[error("{path}: {reason}", path = path.display())]
    Parse { path: PathBuf, reason: String },
    /// A `${…}` that could not be resolved. Carries the placeholder rather than the value, because
    /// the value is usually a token.
    #[error("`{placeholder}` cannot be resolved: {reason}")]
    Template { placeholder: String, reason: String },
    #[error("MCP server `{server}` is unavailable: {reason}")]
    Connect { server: String, reason: String },
    #[error("MCP server `{server}` failed: {reason}")]
    Call { server: String, reason: String },
    #[error("`{tool}` on MCP server `{server}` did not answer within {secs}s")]
    Timeout {
        server: String,
        tool: String,
        secs: u64,
    },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, McpError>;

/// A [`ToolCtx`](zlogic_tools::ToolCtx) for this crate's tests.
/// `zlogic_tools`' own helper is crate-private and the fields are public, so this is a small copy
/// rather than a dependency inversion nobody needed.
#[cfg(test)]
pub(crate) fn test_ctx() -> zlogic_tools::ToolCtx {
    test_ctx_in(std::env::temp_dir())
}

#[cfg(test)]
pub(crate) fn test_ctx_in(root: impl Into<PathBuf>) -> zlogic_tools::ToolCtx {
    let root = root.into();
    zlogic_tools::ToolCtx {
        exec_cwd: root.clone(),
        root,
        session_id: zlogic_protocol::SessionId::new(),
        turn_id: zlogic_protocol::TurnId::new(),
        call_id: zlogic_protocol::CallId::new("call_test"),
        objects: std::sync::Arc::new(zlogic_objects::MemoryObjectStore::new()),
        spawner: None,
        tasks: None,
        worktree: None,
        interaction: None,
        output: None,
        skills: None,
        max_result_chars: 2_000,
        runtime_paths: Vec::new(),
        cancel: zlogic_tools::CancellationToken::new(),
    }
}
