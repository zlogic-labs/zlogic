//! User-managed external resources.
//! Secrets are write-only at this boundary. [`ManagedResource`] exposes only whether a credential
//! is available; the actual value is stored in the OS keychain or an environment variable.

use serde::{Deserialize, Serialize};

use crate::{ResourceId, WorkspaceId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum ManagedResourceKind {
    Database,
    ObjectStorage,
    CloudAccount,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum ManagedResourceEnvironment {
    Development,
    Test,
    Staging,
    Production,
}

impl ManagedResourceEnvironment {
    pub fn is_production(self) -> bool {
        matches!(self, Self::Production)
    }
}

/// Safe resource metadata returned to clients and tools.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ManagedResource {
    pub resource_id: ResourceId,
    pub label: String,
    pub kind: ManagedResourceKind,
    pub provider: String,
    pub environment: ManagedResourceEnvironment,
    /// Provider-specific, non-secret settings only.
    pub config: serde_json::Value,
    pub capabilities: Vec<String>,
    pub workspace_ids: Vec<WorkspaceId>,
    pub enabled: bool,
    pub credential_present: bool,
    /// Changes whenever target/config/environment changes. Grants must bind to this value.
    pub fingerprint: String,
    pub updated_at: String,
}

/// Creates or replaces one managed resource.
/// Deliberately has no `Debug`: `secret` must never reach tracing through `?req`.
#[derive(Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ManagedResourceUpsertReq {
    pub resource_id: Option<ResourceId>,
    pub label: String,
    pub kind: ManagedResourceKind,
    pub provider: String,
    pub environment: ManagedResourceEnvironment,
    pub config: serde_json::Value,
    pub capabilities: Vec<String>,
    pub workspace_ids: Vec<WorkspaceId>,
    pub enabled: bool,
    /// A write-only secret. Non-empty values replace the keychain entry.
    pub secret: Option<String>,
    /// Optional `env:NAME`. Mutually exclusive with a submitted secret.
    pub credential_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ManagedResourceDeleteReq {
    pub resource_id: ResourceId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ManagedResourceListReq {
    pub workspace_id: Option<WorkspaceId>,
    pub kind: Option<ManagedResourceKind>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ManagedResourceTestReq {
    pub resource_id: ResourceId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ManagedResourceTestResult {
    pub ok: bool,
    pub message: String,
    pub duration_ms: u64,
}
