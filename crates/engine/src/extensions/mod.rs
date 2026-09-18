//! # MCP servers, plugins and skills
//!
//! Two halves live here, on opposite sides of the extension seam:
//!
//! * the **loading path** -- [`Extensions::tools_for`] assembles the tools of the enabled servers
//!   and plugins for one turn, with `state` (which switches are on) and `trust` (whether a
//!   definition that arrived inside a repository may run). This is what every turn needs, in every
//!   host, and it never calls the administrative half.
//! * the **administrative surface** -- what a management UI drives. It lives in the closed host
//!   and arrives through `BootstrapOptions::pro` as part of a `ProWiring`; the accessors below
//!   (`dirs`, `plugin_mcp_servers`, `loaded_catalog`, `after_extension_change`,
//!   `store_server_credential`, `forget_notices`) are all it needs from here.
pub mod state;
pub mod trust;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use zlogic_config::Dirs;
use zlogic_core::PlanNotice;
use zlogic_credential::{CredentialStore, SystemCredentialStore};
use zlogic_mcp::{Catalog, CatalogDirs, McpPool, PoolConfig, ServerDef};
use zlogic_plugins::PluginDirs;
use zlogic_protocol::WorkspaceId;
use zlogic_tools::ToolRegistry;

pub const TOKEN_BUDGET_SHARE: f32 = 0.10;

pub const TOKEN_BUDGET_ABSOLUTE: usize = 15_000;

pub use state::{Decided, Kind, Verdict};
pub use trust::Trust;
pub use zlogic_mcp::Origin;
pub use zlogic_mcp::def::Capabilities as ServerCapabilities;

pub struct Extensions {
    dirs: Dirs,
    catalog: Arc<Catalog>,
    plugins: PluginDirs,
    pool: Arc<McpPool>,
    told: Mutex<HashSet<(PathBuf, String)>>,
    reaper: AtomicBool,
}

#[derive(Debug, thiserror::Error)]
pub enum ExtensionError {
    #[error("no MCP server named `{0}`")]
    Unknown(String),
    #[error("`{0}` is one you installed yourself; it does not need confirmation")]
    NotGated(String),
    #[error("cannot write the state: {0}")]
    Io(#[from] std::io::Error),
    #[error("cannot write the credential to the system keychain: {0}")]
    Credential(String),
}

fn budget_warning(
    loaded: &zlogic_mcp::Loaded,
    context_window: Option<u64>,
) -> Option<(
    String,
    std::collections::BTreeMap<String, serde_json::Value>,
)> {
    let tokens = loaded.estimated_tokens();
    let (over, how_much) = match context_window {
        Some(window) if window > 0 => {
            let share = tokens as f32 / window as f32;
            (
                share >= TOKEN_BUDGET_SHARE,
                format!(
                    "approximately {tokens} tokens, {:.0}% of the context window",
                    share * 100.0
                ),
            )
        }
        _ => (
            tokens >= TOKEN_BUDGET_ABSOLUTE,
            format!("approximately {tokens} tokens"),
        ),
    };
    if !over {
        return None;
    }
    let biggest: Vec<String> = loaded
        .token_estimates()
        .into_iter()
        .take(3)
        .map(|(id, t)| format!("{id} (~{t} tokens)"))
        .collect();
    let message = format!(
        "MCP tool definitions total {how_much}, and you pay that cost **every round**. The biggest consumers: {}. \
         To tighten: restrict which tools ship in the definition (`\"tools\": [\"only these\"]`), or turn off \
         servers you do not need right now with `/mcp off <id>`.",
        biggest.join(", ")
    );
    let mut args = std::collections::BTreeMap::new();
    args.insert("how_much".to_string(), serde_json::json!(how_much));
    args.insert("biggest".to_string(), serde_json::json!(biggest.join(", ")));
    Some((message, args))
}

fn plugin_origin(plugin: &zlogic_plugins::PluginDef) -> zlogic_mcp::Origin {
    if plugin.from_workspace {
        zlogic_mcp::Origin::Workspace
    } else {
        zlogic_mcp::Origin::Global
    }
}

pub struct Assembled {
    pub tools: ToolRegistry,
    pub notices: Vec<PlanNotice>,
    pub extension_tools: usize,
    pub extension_tokens: usize,
    pub unavailable: Vec<String>,
}

impl Extensions {
    pub fn new(dirs: &Dirs) -> Arc<Self> {
        Self::with_pool_config(dirs, PoolConfig::default())
    }

    pub fn with_pool_config(dirs: &Dirs, config: PoolConfig) -> Arc<Self> {
        Self::with_limits(dirs, config, zlogic_mcp::catalog::RefreshLimits::default())
    }

    pub fn with_limits(
        dirs: &Dirs,
        config: PoolConfig,
        limits: zlogic_mcp::catalog::RefreshLimits,
    ) -> Arc<Self> {
        let pool = Arc::new(McpPool::new(config));
        Arc::new(Self {
            dirs: dirs.clone(),
            catalog: Arc::new(
                Catalog::new(CatalogDirs::under(dirs), pool.clone()).with_limits(limits),
            ),
            plugins: PluginDirs::under(dirs),
            pool,
            told: Mutex::new(HashSet::new()),
            reaper: AtomicBool::new(false),
        })
    }

    pub fn pool(&self) -> &Arc<McpPool> {
        &self.pool
    }

    pub fn catalog(&self) -> &Arc<Catalog> {
        &self.catalog
    }

    pub fn dirs(&self) -> &Dirs {
        &self.dirs
    }

    pub fn plugin_mcp_servers(&self, workspace_root: &Path) -> Vec<ServerDef> {
        zlogic_plugins::load(&self.plugins, Some(workspace_root)).mcp_servers()
    }

    pub fn loaded_catalog(
        &self,
        workspace_root: &Path,
        contributed: Vec<ServerDef>,
    ) -> zlogic_mcp::Loaded {
        self.catalog.load(workspace_root, contributed)
    }

    /// Everything the engine cached about an extension whose switches or definition just changed:
    /// the live connection is dropped, the runtime status is cleared and the notices are reset so
    /// the next turn reports the new state instead of remembering the old one.
    pub fn after_extension_change(&self, id: &str) {
        self.pool.evict_server(id);
        self.catalog.clear_runtime_status(id);
        self.forget_notices();
    }

    /// Put a credential in the OS keychain under `entry`, or delete that entry when the value is
    /// blank. The live connection of `server_id` is dropped so the next call reads the new value.
    pub fn store_server_credential(
        &self,
        server_id: &str,
        entry: &str,
        value: &str,
    ) -> Result<String, ExtensionError> {
        if value.trim().is_empty() {
            SystemCredentialStore
                .delete_keyring(entry)
                .map_err(|e| ExtensionError::Credential(e.to_string()))?;
        } else {
            SystemCredentialStore
                .set_keyring(entry, value.trim())
                .map_err(|e| ExtensionError::Credential(e.to_string()))?;
        }
        self.pool.evict_server(server_id);
        self.catalog.clear_runtime_status(server_id);
        Ok(format!("keyring:{entry}"))
    }

    pub fn forget_notices(&self) {
        self.told
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    pub async fn tools_for(
        &self,
        workspace: WorkspaceId,
        workspace_root: &Path,
        base: &ToolRegistry,
        context_window: Option<u64>,
    ) -> Assembled {
        if !self.reaper.swap(true, Ordering::SeqCst) {
            McpPool::spawn_reaper(&self.pool);
        }
        let mut notices = Vec::new();
        let states = state::States::load(&self.dirs, workspace);

        let plugins = zlogic_plugins::load(&self.plugins, Some(workspace_root));
        for problem in &plugins.problems {
            self.note(
                workspace_root,
                &mut notices,
                "plugin_unusable",
                problem.to_string(),
                std::collections::BTreeMap::new(),
            );
        }

        let mut contributed = Vec::new();
        for plugin in &plugins.plugins {
            let verdict = states.verdict(
                Kind::Plugin,
                &plugin.id,
                &plugin_origin(plugin),
                plugin.enabled,
            );
            if !verdict.enabled {
                continue;
            }
            let unused = plugin.unused_directories();
            if !unused.is_empty() {
                tracing::debug!(
                    target: "zlogic::engine",
                    plugin = %plugin.id,
                    "plugin ships directories zlogic does not currently consume: {}",
                    unused.join(", ")
                );
            }
            contributed.extend(plugin.servers.iter().cloned());
        }

        let mut loaded = self.catalog.load(workspace_root, contributed);

        let mut withheld: Vec<(ServerDef, Trust)> = Vec::new();
        loaded.retain(|def| {
            if !states
                .verdict(Kind::Mcp, &def.id, &def.origin, def.enabled)
                .enabled
            {
                return false;
            }
            let trust = trust::state_of(def, &states);
            if trust.allows_launch() {
                return true;
            }
            withheld.push((def.clone(), trust));
            false
        });

        let mut unavailable: Vec<String> = withheld
            .iter()
            .map(|(def, trust)| {
                let why = match trust {
                    Trust::Changed => "its definition changed and needs confirming again",
                    _ => "it has not been confirmed yet",
                };
                format!(
                    "{} ({why}; the user can run `/mcp trust {}`)",
                    def.id, def.id
                )
            })
            .collect();

        if !withheld.is_empty() {
            let borrowed: Vec<(&ServerDef, Trust)> =
                withheld.iter().map(|(def, trust)| (def, *trust)).collect();
            let (message, args) = trust::withheld_message(&borrowed);
            self.note(
                workspace_root,
                &mut notices,
                "mcp_server_untrusted",
                message,
                args,
            );
        }

        self.catalog.refresh(&mut loaded, workspace_root).await;

        unavailable.extend(loaded.problems.iter().map(ToString::to_string));
        for problem in &loaded.problems {
            // Runtime health belongs in the MCP management surface. It is still logged and supplied
            // to the model below, but it must not become a persisted chat notice.
            tracing::warn!(
                target: "zlogic::engine",
                code = "mcp_server_unavailable",
                "{}",
                problem
            );
        }

        unavailable.extend(
            loaded
                .pending
                .iter()
                .map(|id| format!("{id} (still starting; it should be available next turn)")),
        );

        if !loaded.pending.is_empty() {
            let servers = loaded.pending.join(", ");
            let message = format!(
                "Still fetching the tool manifest for {servers}; they are not included this round (they will be next round)"
            );
            tracing::debug!(target: "zlogic::engine", "{message}");
            let mut args = std::collections::BTreeMap::new();
            args.insert("servers".to_string(), serde_json::json!(servers));
            notices.push(PlanNotice::info_with_args(
                "mcp_tools_pending",
                message,
                args,
            ));
        }

        let mut tools = base.clone();
        tools.add_source(&self.catalog.source(&loaded));
        let extension_tools = tools.len().saturating_sub(base.len());
        let extension_tokens = loaded.estimated_tokens();

        if let Some((message, args)) = budget_warning(&loaded, context_window) {
            self.note(
                workspace_root,
                &mut notices,
                "mcp_tools_expensive",
                message,
                args,
            );
        }

        for server in loaded.silent_servers() {
            self.note(
                workspace_root,
                &mut notices,
                "mcp_server_has_no_tools",
                format!("MCP server `{server}` connected but did not provide any tools"),
                std::collections::BTreeMap::from([(
                    "server".to_string(),
                    serde_json::json!(server),
                )]),
            );
        }

        Assembled {
            tools,
            notices,
            extension_tools,
            extension_tokens,
            unavailable,
        }
    }

    pub fn forget_workspace(&self, workspace_root: &Path) {
        let dropped = self.pool.evict_workspace(workspace_root);
        if dropped > 0 {
            tracing::debug!(target: "zlogic::engine", "closing workspace, dropped {dropped} MCP connections");
        }
        self.told
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(root, _)| root != workspace_root);
    }

    fn note(
        &self,
        workspace_root: &Path,
        notices: &mut Vec<PlanNotice>,
        code: &str,
        message: String,
        args: std::collections::BTreeMap<String, serde_json::Value>,
    ) {
        let first_time = self
            .told
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert((workspace_root.to_path_buf(), message.clone()));
        tracing::warn!(target: "zlogic::engine", code, "{message}");
        if first_time {
            notices.push(PlanNotice::warn_with_args(code, message, args));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use zlogic_tools::ToolRegistry;

    fn ws() -> WorkspaceId {
        WorkspaceId::new()
    }

    fn write(path: &Path, value: serde_json::Value) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_string_pretty(&value).unwrap()).unwrap();
    }

    #[tokio::test]
    async fn nothing_configured_leaves_the_builtins_untouched() {
        let ws = ws();
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        let ext = Extensions::new(&dirs);
        let base = ToolRegistry::with_builtins();

        let out = ext.tools_for(ws, tmp.path(), &base, None).await;
        assert_eq!(out.tools.len(), base.len());
        assert_eq!(out.extension_tools, 0);
        assert!(
            out.notices.is_empty(),
            "having no extensions installed is the normal case, not a problem"
        );
    }

    #[tokio::test]
    async fn a_server_that_cannot_start_is_status_not_a_chat_notice() {
        let ws = ws();
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        write(
            &dirs.data.join("extensions/mcp/gh.json"),
            json!({ "command": "zlogic-mcp-definitely-not-installed" }),
        );
        let ext = Extensions::with_pool_config(
            &dirs,
            PoolConfig {
                connect_timeout: std::time::Duration::from_millis(200),
                ..Default::default()
            },
        );
        let base = ToolRegistry::with_builtins();

        let out = ext.tools_for(ws, tmp.path(), &base, None).await;
        assert_eq!(
            out.tools.len(),
            base.len(),
            "not one builtin tool is missing"
        );
        assert!(out.notices.is_empty(), "{:?}", out.notices);
        assert!(matches!(
            ext.catalog().runtime_status("gh"),
            Some(zlogic_mcp::RuntimeStatus::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn a_broken_plugin_manifest_is_reported_as_a_plugin_problem() {
        let ws = ws();
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        let manifest = dirs.data.join("extensions/plugins/broken/plugin.json");
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        std::fs::write(&manifest, "{ not json").unwrap();

        let ext = Extensions::new(&dirs);
        let out = ext
            .tools_for(ws, tmp.path(), &ToolRegistry::with_builtins(), None)
            .await;
        assert_eq!(out.notices.len(), 1);
        assert_eq!(out.notices[0].code, "plugin_unusable");
    }

    #[tokio::test]
    async fn definitions_in_the_repository_are_read() {
        let ws = ws();
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        let workspace = tmp.path().join("work");
        std::fs::create_dir_all(&workspace).unwrap();
        write(
            &workspace.join(".mcp.json"),
            json!({ "mcpServers": { "repo": { "command": "not-installed" } } }),
        );

        let ext = Extensions::with_pool_config(
            &dirs,
            PoolConfig {
                connect_timeout: std::time::Duration::from_millis(200),
                ..Default::default()
            },
        );
        let out = ext
            .tools_for(ws, &workspace, &ToolRegistry::with_builtins(), None)
            .await;
        assert!(
            out.notices.iter().any(|n| n.message.contains("repo")),
            "the .mcp.json in the repository was not read: {:?}",
            out.notices
        );
    }
    fn seed(dirs: &Dirs, workspace_root: &Path, id: &str, tools: usize, description_chars: usize) {
        let path = dirs.data.join("extensions/mcp").join(format!("{id}.json"));
        write(
            &path,
            json!({ "command": "never-started", "cwd": "/fixed" }),
        );
        let parsed = zlogic_mcp::def::parse_file(&path, zlogic_mcp::Origin::Global).unwrap();
        let def = parsed
            .servers
            .iter()
            .find(|s| s.id == id)
            .expect("could not read back the definition just written");

        let specs: Vec<zlogic_mcp::ToolSpec> = (0..tools)
            .map(|i| zlogic_mcp::ToolSpec {
                name: format!("t{i}"),
                description: Some("d".repeat(description_chars)),
                input_schema: json!({ "type": "object", "properties": {} }),
                hints: zlogic_mcp::Hints::default(),
            })
            .collect();
        zlogic_mcp::write_tool_cache(
            &CatalogDirs::under(dirs),
            def,
            workspace_root,
            &specs,
            chrono::Utc::now(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn the_same_servers_are_not_reported_when_the_window_is_large() {
        let ws = ws();
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        seed(&dirs, tmp.path(), "modest", 3, 200);

        let ext = Extensions::new(&dirs);
        let out = ext
            .tools_for(
                ws,
                tmp.path(),
                &ToolRegistry::with_builtins(),
                Some(1_000_000),
            )
            .await;
        assert!(
            !out.notices.iter().any(|n| n.code == "mcp_tools_expensive"),
            "{:?}",
            out.notices
        );
    }

    #[tokio::test]
    async fn an_unknown_window_falls_back_to_an_absolute_threshold() {
        let ws = ws();
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        seed(&dirs, tmp.path(), "huge", 60, 2_000);

        let ext = Extensions::new(&dirs);
        let out = ext
            .tools_for(ws, tmp.path(), &ToolRegistry::with_builtins(), None)
            .await;
        assert!(out.extension_tokens >= TOKEN_BUDGET_ABSOLUTE);
        assert!(
            out.notices.iter().any(|n| n.code == "mcp_tools_expensive"),
            "{:?}",
            out.notices
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_slow_server_is_reported_as_pending_not_as_a_problem() {
        let ws = ws();
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        write(
            &dirs.data.join("extensions/mcp/slow.json"),
            json!({ "command": "sleep", "args": ["30"], "cwd": "/tmp" }),
        );

        let ext = Extensions::with_limits(
            &dirs,
            PoolConfig {
                connect_timeout: std::time::Duration::from_secs(30),
                ..Default::default()
            },
            zlogic_mcp::catalog::RefreshLimits {
                deadline: std::time::Duration::from_millis(80),
                ..Default::default()
            },
        );

        let started = std::time::Instant::now();
        let out = ext
            .tools_for(ws, tmp.path(), &ToolRegistry::with_builtins(), None)
            .await;
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "must not wait for its handshake"
        );

        let notice = out
            .notices
            .iter()
            .find(|n| n.code == "mcp_tools_pending")
            .unwrap_or_else(|| panic!("{:?}", out.notices));
        assert_eq!(
            notice.level,
            zlogic_protocol::stream::NoticeLevel::Info,
            "still fetching is not a warning"
        );
        assert!(notice.message.contains("slow"), "{}", notice.message);
    }
}
