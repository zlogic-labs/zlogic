use async_trait::async_trait;
use serde_json::{Map, Value};
use zlogic_protocol::{ManagedResourceEnvironment, ManagedResourceKind, ResourceId, SessionId};

use crate::Result;

#[derive(Debug, Clone)]
pub struct ManagedResourceConnection {
    pub resource_id: ResourceId,
    pub label: String,
    pub kind: ManagedResourceKind,
    pub provider: String,
    pub environment: ManagedResourceEnvironment,
    pub config: Value,
    pub capabilities: Vec<String>,
    pub enabled: bool,
    pub fingerprint: String,
    pub credential_present: bool,
    pub credential: Option<String>,
}

/// The host side of managed resources: where the connections live.
/// `available()` is asked before the tool advertises itself, so a build with no resource store
/// shows no resource tool at all rather than a tool that always fails.
pub trait ManagedResourceHost: Send + Sync {
    fn available(&self) -> bool;
    fn list(
        &self,
        session_id: SessionId,
        kind: ManagedResourceKind,
    ) -> Result<Vec<ManagedResourceConnection>>;
    fn get(
        &self,
        session_id: SessionId,
        resource_id: ResourceId,
    ) -> Result<ManagedResourceConnection>;
}

#[async_trait]
pub trait ManagedResourceProvider: Send + Sync {
    fn kind(&self) -> ManagedResourceKind;
    fn providers(&self) -> &'static [&'static str];
    fn parse_url(
        &self,
        provider: &str,
        url: &str,
        existing: Option<&Map<String, Value>>,
    ) -> Result<Map<String, Value>>;
    async fn probe(&self, connection: ManagedResourceConnection) -> Result<String>;
}
