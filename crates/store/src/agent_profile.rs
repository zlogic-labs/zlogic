use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, named_params};

use crate::{Json, StoreError, now};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentProfileRecord {
    pub profile_name: String,
    pub system_prompt: String,
    pub tools: Vec<String>,
    pub model_ref: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub struct AgentProfileStore<'a> {
    conn: &'a Connection,
}

impl<'a> AgentProfileStore<'a> {
    pub(crate) fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub fn list(&self) -> Result<Vec<AgentProfileRecord>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT profile_name, system_prompt, tools, model_ref, created_at, updated_at
               FROM agent_profile
              ORDER BY profile_name COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn get(&self, profile_name: &str) -> Result<Option<AgentProfileRecord>, StoreError> {
        self.conn
            .query_row(
                "SELECT profile_name, system_prompt, tools, model_ref, created_at, updated_at
                   FROM agent_profile
                  WHERE profile_name = :profile_name",
                named_params! { ":profile_name": profile_name },
                row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn create(
        &self,
        profile_name: &str,
        system_prompt: &str,
        tools: &[String],
        model_ref: Option<&str>,
    ) -> Result<AgentProfileRecord, StoreError> {
        let created_at = now();
        self.conn.execute(
            "INSERT INTO agent_profile (
               profile_name, system_prompt, tools, model_ref, created_at, updated_at
             ) VALUES (
               :profile_name, :system_prompt, :tools, :model_ref, :created_at, :updated_at
             )",
            named_params! {
                ":profile_name": profile_name,
                ":system_prompt": system_prompt,
                ":tools": Json(tools),
                ":model_ref": model_ref,
                ":created_at": created_at,
                ":updated_at": created_at,
            },
        )?;
        Ok(AgentProfileRecord {
            profile_name: profile_name.to_owned(),
            system_prompt: system_prompt.to_owned(),
            tools: tools.to_vec(),
            model_ref: model_ref.map(str::to_owned),
            created_at,
            updated_at: created_at,
        })
    }

    pub fn update(
        &self,
        profile_name: &str,
        system_prompt: &str,
        tools: &[String],
        model_ref: Option<&str>,
    ) -> Result<AgentProfileRecord, StoreError> {
        let updated_at = now();
        let changed = self.conn.execute(
            "UPDATE agent_profile
                SET system_prompt = :system_prompt, tools = :tools, model_ref = :model_ref,
                    updated_at = :updated_at
              WHERE profile_name = :profile_name",
            named_params! {
                ":profile_name": profile_name,
                ":system_prompt": system_prompt,
                ":tools": Json(tools),
                ":model_ref": model_ref,
                ":updated_at": updated_at,
            },
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound {
                kind: "agent_profile",
                id: profile_name.to_owned(),
            });
        }
        let record = self
            .get(profile_name)?
            .ok_or_else(|| StoreError::NotFound {
                kind: "agent_profile",
                id: profile_name.to_owned(),
            })?;
        Ok(record)
    }

    pub fn delete(&self, profile_name: &str) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            "DELETE FROM agent_profile WHERE profile_name = :profile_name",
            named_params! { ":profile_name": profile_name },
        )?;
        ensure_changed(changed, profile_name)
    }
}

fn ensure_changed(changed: usize, profile_name: &str) -> Result<(), StoreError> {
    if changed == 0 {
        Err(StoreError::NotFound {
            kind: "agent_profile",
            id: profile_name.to_owned(),
        })
    } else {
        Ok(())
    }
}

fn row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AgentProfileRecord> {
    let tools: Json<Vec<String>> = row.get("tools")?;
    Ok(AgentProfileRecord {
        profile_name: row.get("profile_name")?,
        system_prompt: row.get("system_prompt")?,
        tools: tools.into_inner(),
        model_ref: row.get("model_ref")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

#[cfg(test)]
mod tests {
    use crate::{AgentProfileRecord, Db, StoreError};

    #[test]
    fn create_list_update_delete_round_trips() {
        let db = Db::open_in_memory().unwrap();
        let store = db.agent_profiles();
        assert!(store.list().unwrap().is_empty());

        let created = store
            .create(
                "qa-lead",
                "You check test quality.",
                &["shell".to_string(), "grep".to_string()],
                Some("gpt-5:mini"),
            )
            .unwrap();
        assert_eq!(created.profile_name, "qa-lead");
        assert_eq!(created.tools, vec!["shell", "grep"]);
        assert_eq!(created.model_ref.as_deref(), Some("gpt-5:mini"));

        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0], created);

        // update keeps identity, changes the content
        let updated = store
            .update(
                "qa-lead",
                "You check test quality thoroughly.",
                &["shell".to_string()],
                None,
            )
            .unwrap();
        assert_eq!(updated.profile_name, "qa-lead");
        assert_eq!(updated.system_prompt, "You check test quality thoroughly.");
        assert!(updated.model_ref.is_none());
        assert_eq!(updated.created_at, created.created_at);

        // create with the same name is rejected by the UNIQUE constraint
        assert!(
            store
                .create("qa-lead", "dup", &[], None)
                .unwrap_err()
                .to_string()
                .contains("UNIQUE")
        );

        // delete removes the row; double delete is NotFound
        store.delete("qa-lead").unwrap();
        assert!(store.list().unwrap().is_empty());
        match store.delete("qa-lead") {
            Err(StoreError::NotFound { .. }) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn empty_tools_and_no_model_round_trip_as_none() {
        let db = Db::open_in_memory().unwrap();
        let created = db
            .agent_profiles()
            .create("plain", "Be helpful.", &[], None)
            .unwrap();
        assert!(created.tools.is_empty());
        assert!(created.model_ref.is_none());
        let fetched: Option<AgentProfileRecord> = db.agent_profiles().get("plain").unwrap();
        assert_eq!(fetched, Some(created));
    }
}
