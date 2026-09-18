//! Durable non-secret metadata for external resources.

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, named_params};
use zlogic_protocol::{ManagedResourceEnvironment, ManagedResourceKind, ResourceId, WorkspaceId};

use crate::{Json, Result, StoreError, now};

const COLS: &str = "resource_id, label, kind, provider, environment, config, capabilities, \
                    credential_ref, enabled, fingerprint, updated_at";

#[derive(Debug, Clone, PartialEq)]
pub struct ResourceRecord {
    pub resource_id: ResourceId,
    pub label: String,
    pub kind: ManagedResourceKind,
    pub provider: String,
    pub environment: ManagedResourceEnvironment,
    pub config: serde_json::Value,
    pub capabilities: Vec<String>,
    pub credential_ref: Option<String>,
    pub enabled: bool,
    pub fingerprint: String,
    pub updated_at: DateTime<Utc>,
    pub workspace_ids: Vec<WorkspaceId>,
}

pub struct NewResource<'a> {
    pub resource_id: ResourceId,
    pub label: &'a str,
    pub kind: ManagedResourceKind,
    pub provider: &'a str,
    pub environment: ManagedResourceEnvironment,
    pub config: &'a serde_json::Value,
    pub capabilities: &'a [String],
    pub credential_ref: Option<&'a str>,
    pub enabled: bool,
    pub fingerprint: &'a str,
    pub workspace_ids: &'a [WorkspaceId],
}

pub struct ResourceStore<'a> {
    conn: &'a Connection,
}

impl<'a> ResourceStore<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub fn list(
        &self,
        workspace_id: Option<WorkspaceId>,
        kind: Option<ManagedResourceKind>,
    ) -> Result<Vec<ResourceRecord>> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {COLS} FROM managed_resource r
             WHERE (:workspace_id IS NULL OR EXISTS (
               SELECT 1 FROM managed_resource_workspace rw
               WHERE rw.resource_id = r.resource_id AND rw.workspace_id = :workspace_id
             ))
             AND (:kind IS NULL OR r.kind = :kind)
             ORDER BY lower(r.label), r.resource_id"
        ))?;
        let workspace = workspace_id.map(|id| id.to_string());
        let kind = kind.map(kind_wire);
        let rows = statement.query_map(
            named_params! {":workspace_id": workspace, ":kind": kind},
            map_record,
        )?;
        rows.map(|row| {
            let mut record = row?;
            record.workspace_ids = self.workspace_ids(record.resource_id)?;
            Ok(record)
        })
        .collect()
    }

    pub fn find(&self, resource_id: ResourceId) -> Result<Option<ResourceRecord>> {
        let mut record = self
            .conn
            .query_row(
                &format!("SELECT {COLS} FROM managed_resource WHERE resource_id = :resource_id"),
                named_params! {":resource_id": resource_id},
                map_record,
            )
            .optional()?;
        if let Some(record) = &mut record {
            record.workspace_ids = self.workspace_ids(resource_id)?;
        }
        Ok(record)
    }

    pub fn get(&self, resource_id: ResourceId) -> Result<ResourceRecord> {
        self.find(resource_id)?.ok_or_else(|| StoreError::NotFound {
            kind: "managed_resource",
            id: resource_id.to_string(),
        })
    }

    pub fn upsert(&self, resource: NewResource<'_>) -> Result<ResourceRecord> {
        let transaction = self.conn.unchecked_transaction()?;
        let updated_at = now();
        transaction.execute(
            "INSERT INTO managed_resource (
               resource_id, label, kind, provider, environment, config, capabilities,
               credential_ref, enabled, fingerprint, updated_at
             ) VALUES (
               :resource_id, :label, :kind, :provider, :environment, :config, :capabilities,
               :credential_ref, :enabled, :fingerprint, :updated_at
             )
             ON CONFLICT(resource_id) DO UPDATE SET
               label = excluded.label,
               kind = excluded.kind,
               provider = excluded.provider,
               environment = excluded.environment,
               config = excluded.config,
               capabilities = excluded.capabilities,
               credential_ref = excluded.credential_ref,
               enabled = excluded.enabled,
               fingerprint = excluded.fingerprint,
               updated_at = excluded.updated_at",
            named_params! {
                ":resource_id": resource.resource_id,
                ":label": resource.label,
                ":kind": kind_wire(resource.kind),
                ":provider": resource.provider,
                ":environment": environment_wire(resource.environment),
                ":config": Json(resource.config),
                ":capabilities": Json(resource.capabilities),
                ":credential_ref": resource.credential_ref,
                ":enabled": resource.enabled,
                ":fingerprint": resource.fingerprint,
                ":updated_at": updated_at,
            },
        )?;
        transaction.execute(
            "DELETE FROM managed_resource_workspace WHERE resource_id = :resource_id",
            named_params! {":resource_id": resource.resource_id},
        )?;
        for workspace_id in resource.workspace_ids {
            transaction.execute(
                "INSERT INTO managed_resource_workspace (resource_id, workspace_id)
                 VALUES (:resource_id, :workspace_id)",
                named_params! {
                    ":resource_id": resource.resource_id,
                    ":workspace_id": workspace_id,
                },
            )?;
        }
        transaction.commit()?;
        self.get(resource.resource_id)
    }

    pub fn delete(&self, resource_id: ResourceId) -> Result<bool> {
        Ok(self.conn.execute(
            "DELETE FROM managed_resource WHERE resource_id = :resource_id",
            named_params! {":resource_id": resource_id},
        )? > 0)
    }

    fn workspace_ids(&self, resource_id: ResourceId) -> Result<Vec<WorkspaceId>> {
        let mut statement = self.conn.prepare(
            "SELECT workspace_id FROM managed_resource_workspace
             WHERE resource_id = :resource_id ORDER BY workspace_id",
        )?;
        statement
            .query_map(named_params! {":resource_id": resource_id}, |row| {
                row.get(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }
}

fn map_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<ResourceRecord> {
    let kind: String = row.get(2)?;
    let environment: String = row.get(4)?;
    Ok(ResourceRecord {
        resource_id: row.get(0)?,
        label: row.get(1)?,
        kind: parse_kind(&kind).map_err(to_sql_error)?,
        provider: row.get(3)?,
        environment: parse_environment(&environment).map_err(to_sql_error)?,
        config: row.get::<_, Json<serde_json::Value>>(5)?.0,
        capabilities: row.get::<_, Json<Vec<String>>>(6)?.0,
        credential_ref: row.get(7)?,
        enabled: row.get(8)?,
        fingerprint: row.get(9)?,
        updated_at: row.get(10)?,
        workspace_ids: Vec::new(),
    })
}

fn kind_wire(kind: ManagedResourceKind) -> &'static str {
    match kind {
        ManagedResourceKind::Database => "database",
        ManagedResourceKind::ObjectStorage => "object_storage",
        ManagedResourceKind::CloudAccount => "cloud_account",
    }
}

fn environment_wire(environment: ManagedResourceEnvironment) -> &'static str {
    match environment {
        ManagedResourceEnvironment::Development => "development",
        ManagedResourceEnvironment::Test => "test",
        ManagedResourceEnvironment::Staging => "staging",
        ManagedResourceEnvironment::Production => "production",
    }
}

fn parse_kind(value: &str) -> Result<ManagedResourceKind> {
    match value {
        "database" => Ok(ManagedResourceKind::Database),
        "object_storage" => Ok(ManagedResourceKind::ObjectStorage),
        "cloud_account" => Ok(ManagedResourceKind::CloudAccount),
        other => Err(StoreError::Corrupt(format!(
            "unknown resource kind {other:?}"
        ))),
    }
}

fn parse_environment(value: &str) -> Result<ManagedResourceEnvironment> {
    match value {
        "development" => Ok(ManagedResourceEnvironment::Development),
        "test" => Ok(ManagedResourceEnvironment::Test),
        "staging" => Ok(ManagedResourceEnvironment::Staging),
        "production" => Ok(ManagedResourceEnvironment::Production),
        other => Err(StoreError::Corrupt(format!(
            "unknown resource environment {other:?}"
        ))),
    }
}

fn to_sql_error(error: StoreError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;

    #[test]
    fn resource_metadata_and_workspace_scope_round_trip_without_a_secret() {
        let db = Db::open_in_memory().unwrap();
        let workspace = db
            .workspaces()
            .resolve(tempfile::tempdir().unwrap().path())
            .unwrap()
            .0;
        let id = ResourceId::new();
        let record = db
            .resources()
            .upsert(NewResource {
                resource_id: id,
                label: "Development DB",
                kind: ManagedResourceKind::Database,
                provider: "postgresql",
                environment: ManagedResourceEnvironment::Development,
                config: &serde_json::json!({"host":"localhost"}),
                capabilities: &["schema".into(), "sample".into()],
                credential_ref: Some("keyring:resource-test"),
                enabled: true,
                fingerprint: "abc",
                workspace_ids: &[workspace.workspace_id],
            })
            .unwrap();
        assert_eq!(record.resource_id, id);
        assert_eq!(record.workspace_ids, [workspace.workspace_id]);
        assert_eq!(
            db.resources()
                .list(Some(workspace.workspace_id), None)
                .unwrap()
                .len(),
            1
        );
        assert!(db.resources().delete(id).unwrap());
    }
}
