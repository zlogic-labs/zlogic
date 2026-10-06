//! UI ↔ engine contract for installed skills, plugins and MCP servers.
//! The contract is capability-oriented: every install is inspected first, and potentially
//! executable MCP declarations are returned as explicit capabilities. The caller must repeat the
//! install with `accept_capabilities = true`; there is no hidden "install and run" operation.

use std::collections::BTreeMap;

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
    /// `repository` is a link or `owner/repository@ref` and nothing else. Which folder inside it
    /// comes from `subpath`, chosen from the folders the app listed — not spelled out inside the
    /// link, where a URL cannot carry it and a user has to guess the shape of one that can.
    Github {
        repository: String,
        #[serde(default)]
        subpath: Option<String>,
    },
    Local {
        path: String,
    },
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
    /// Only meaningful for MCP descriptors.
    #[serde(default)]
    pub connection_scope: McpConnectionScope,
    /// Last observed MCP connection state. Live process state; never persisted.
    pub runtime_status: Option<McpRuntimeStatus>,
    /// The repository brought this one, so the switch can only be flipped per workspace.
    #[serde(default)]
    pub workspace_scoped: bool,
    /// Only set for a repository-provided server. What the workspace has already confirmed.
    #[serde(default)]
    pub trust: Option<McpTrust>,
    /// What turning it on would allow, in one sentence. Set while it still needs confirming.
    #[serde(default)]
    pub disclosure: Option<String>,
}

/// A repository-provided server's standing in one workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum McpTrust {
    /// Confirmed for the definition as it stands, or never needed confirming.
    Trusted,
    /// Never confirmed here: it does not start.
    Unconfirmed,
    /// Confirmed once, but the definition changed since. It does not start.
    DefinitionChanged,
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

/// How far one MCP connection is shared. The UI's name for the definition's `binding`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum McpConnectionScope {
    /// Whoever's launch parameters match. One workspace's server is usually its own.
    #[default]
    Auto,
    /// One connection per workspace.
    Workspace,
    /// One connection per conversation.
    Session,
    /// One connection for the whole app, launched in a directory of zlogic's own.
    Global,
}

impl McpConnectionScope {
    /// The `binding` value a definition file carries, or `None` for the default: a definition that
    /// says nothing about its scope is one the user wrote, and writing the default back out would
    /// be a change to it.
    pub fn as_binding(self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::Workspace => Some("workspace"),
            Self::Session => Some("session"),
            Self::Global => Some("global"),
        }
    }
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
    /// Locale (`zh-CN`, `en-US`, …) to description, for a UI that knows the reader's language.
    #[serde(default)]
    pub descriptions: BTreeMap<String, String>,
    pub source_path: String,
    /// Compiled into the binary rather than installed, so it has no folder whose disable marker
    /// could carry the switch: the UI's toggle writes to the state file instead.
    #[serde(default)]
    pub builtin: bool,
    /// Whether the model may load this skill. A switched-off skill stays in the list so the UI
    /// can still show the row and offer to turn it back on.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
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
    /// `owner/repository` when the source was GitHub. A confirmation box that cannot say where
    /// something comes from asks the user to approve a name they were shown out of context.
    #[serde(default)]
    pub repository: Option<String>,
    /// The subdirectory inside that repository. Set only when the install covers one folder and
    /// nothing else — which is exactly what the user needs confirmed before a 200-file repository
    /// turns into "you also installed everything else in it".
    #[serde(default)]
    pub subpath: Option<String>,
    /// The folders inside the downloaded repository this app would accept, repository root first.
    ///
    /// The UI offers a choice between them instead of making the subdirectory part of the link the
    /// user pastes: a repository of plugins has a dozen valid answers, and a text box can only
    /// ever hold the one the user guessed. Empty for a local folder, which the user already chose.
    #[serde(default)]
    pub subdirectories: Vec<String>,
    /// The repository holds several extensions and none of them has been chosen yet: its folders
    /// are a menu, and there is nothing here to install. A plan with this set is never installed.
    #[serde(default)]
    pub collection: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ExtensionInstallReq {
    pub source: ExtensionSource,
    pub accept_capabilities: bool,
    /// Folders of the source repository to install together, for a repository that holds several
    /// extensions and the user picked more than one. They are downloaded, validated and written in
    /// one pass, so a selection either lands whole or not at all. Empty — the common case —
    /// installs the one folder `source` names.
    #[serde(default)]
    pub folders: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ExtensionSetEnabledReq {
    pub kind: ExtensionKind,
    pub id: String,
    pub enabled: bool,
    /// The workspace this switch belongs to. Absent means the machine-wide switch.
    #[serde(default)]
    pub workspace_id: Option<String>,
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
    /// How far this server's connection is shared. Absent leaves it derived, which is what every
    /// server installed before this field existed does.
    #[serde(default)]
    pub connection_scope: McpConnectionScope,
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

/// Changes how far an installed server's connection is shared, leaving the rest of its definition
/// alone. A separate operation from [`McpUpsertReq`] on purpose: rewriting a definition to change one
/// setting would also rewrite everything the form does not know about, and re-enable a server the
/// user had switched off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct McpSetScopeReq {
    pub id: String,
    pub scope: McpConnectionScope,
}

/// Connect to one installed server and report what came back.
///
/// A separate operation from everything else in this file because it is the only one that *runs*
/// something: it opens a connection, asks for the tool list and closes it again, writing no state.
/// That is what makes it the honest way to answer "is this server any good?" — the runtime status
/// on a descriptor only says whether some turn has already reached it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct McpTestReq {
    pub id: String,
    /// The workspace whose paths a definition naming `${workspaceRoot}` should be resolved
    /// against. Absent when the management page has no workspace; the test then runs against a
    /// directory of our own and says so.
    #[serde(default)]
    pub workspace_root: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct McpToolSummary {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// The one dependency-install step a definition can imply on its own: a server checked out next to
/// the manifest that builds it. The command is read off files in the server's own directory, never
/// taken as free text from the caller, so this is a suggestion the definition made rather than a
/// shell the UI can point anywhere.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct McpSetupCommand {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    /// One sentence on what this step is for, in the same voice as the rest of the engine's
    /// messages to the user.
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct McpTestResult {
    pub ok: bool,
    pub tools: Vec<McpToolSummary>,
    /// `"connected"` on success, the reason it could not on failure — already readable, because
    /// this string is what the user is looking at when something does not work.
    pub message: String,
    /// The last lines the server wrote to stderr. A stdio server that exits on start says why
    /// there and nowhere else.
    #[serde(default)]
    pub stderr: String,
    pub duration_ms: u64,
    /// Present whenever the definition implies an install step, successful test or not.
    #[serde(default)]
    pub setup: Option<McpSetupCommand>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct McpSetupReq {
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct McpSetupResult {
    pub ok: bool,
    pub message: String,
    /// The tail of what the install printed. npm failing on a postinstall script is the whole
    /// diagnosis, and it is only ever written to a terminal nobody is watching.
    #[serde(default)]
    pub output: String,
    pub duration_ms: u64,
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
