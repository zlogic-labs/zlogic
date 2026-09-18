//! UI ↔ engine contract for installed skills, plugins and MCP servers.
//! The contract is capability-oriented: every install is inspected first, and potentially
//! executable MCP declarations are returned as explicit capabilities. The caller must repeat the
//! install with `accept_capabilities = true`; there is no hidden "install and run" operation.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum ExtensionKind {
    Skill,
    Plugin,
    Mcp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum ExtensionSource {
    Github { repository: String },
    Local { path: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum ExtensionCapability {
    Skill {
        name: String,
    },
    Agent {
        name: String,
    },
    Process {
        server: String,
        command: String,
        args: Vec<String>,
    },
    Network {
        server: String,
        url: String,
    },
    Tool {
        name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ExtensionOrigin {
    pub repository: Option<String>,
    pub git_ref: Option<String>,
    pub subpath: Option<String>,
    pub local_path: Option<String>,
    pub installed_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ExtensionDescriptor {
    pub id: String,
    pub kind: ExtensionKind,
    pub name: String,
    pub description: Option<String>,
    pub version: Option<String>,
    pub location: String,
    pub enabled: bool,
    pub origin: Option<ExtensionOrigin>,
    pub capabilities: Vec<ExtensionCapability>,
    /// Only meaningful for MCP descriptors.
    pub authentication: Option<McpAuthMode>,
    /// Last observed MCP connection state. Live process state; never persisted.
    pub runtime_status: Option<McpRuntimeStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum McpRuntimeStatus {
    Connecting,
    Ready,
    AuthenticationRequired { message: String },
    Unavailable { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ExtensionCatalogReq {
    pub workspace_root: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ExtensionCatalog {
    pub installed: Vec<ExtensionDescriptor>,
    pub active_skills: Vec<ActiveSkill>,
    pub problems: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ActiveSkill {
    pub name: String,
    pub description: String,
    pub source_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ExtensionInspectReq {
    pub source: ExtensionSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ExtensionInstallPlan {
    pub kind: ExtensionKind,
    pub name: String,
    pub description: Option<String>,
    pub version: Option<String>,
    pub capabilities: Vec<ExtensionCapability>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ExtensionInstallReq {
    pub source: ExtensionSource,
    pub accept_capabilities: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ExtensionSetEnabledReq {
    pub kind: ExtensionKind,
    pub id: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ExtensionRemoveReq {
    pub kind: ExtensionKind,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum McpTransportInput {
    Stdio { command: String, args: Vec<String> },
    Http { url: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum McpAuthMode {
    None,
    Bearer,
    ApiKey {
        header: String,
    },
    #[serde(rename = "oauth")]
    #[cfg_attr(feature = "ts", ts(rename = "oauth"))]
    OAuth {
        /// Omit to use dynamic client registration.
        client_id: Option<String>,
        has_client_secret: bool,
        scopes: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct McpUpsertReq {
    pub id: String,
    pub label: Option<String>,
    pub transport: McpTransportInput,
    pub auth: McpAuthMode,
    /// Initial bearer token or API key. Accepted by the install transaction and stored only in the
    /// system keychain.
    pub credential: Option<String>,
    /// Initial pre-registered OAuth client secret. Stored only in the system keychain.
    pub oauth_client_secret: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct McpImportReq {
    pub path: String,
    pub accept_capabilities: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct McpImportResult {
    pub ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct McpSetTokenReq {
    pub id: String,
    /// Empty removes the keychain entry.
    pub token: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct McpSetKeyReq {
    pub id: String,
    /// Empty removes the keychain entry.
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct McpSetOAuthClientSecretReq {
    pub id: String,
    /// Empty removes the keychain entry.
    pub client_secret: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct McpOAuthBeginReq {
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct McpOAuthBeginResult {
    pub flow_id: String,
    pub authorization_url: String,
    pub redirect_uri: String,
    pub expires_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct McpOAuthStatusReq {
    pub flow_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum McpOAuthFlowStatus {
    Pending,
    Succeeded,
    Failed { message: String },
    Expired,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct McpOAuthStatusResult {
    pub flow_id: String,
    pub id: String,
    pub status: McpOAuthFlowStatus,
    pub expires_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct McpOAuthCancelReq {
    pub flow_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct McpLogoutReq {
    pub id: String,
}
