//! The `session_env` table: variables that live as long as one conversation.
//!
//! Every method takes the caller's own session id and resolves the tree root itself. That is the
//! one thing this store insists on: a sub-agent's shell has to see the variables the user set for
//! the conversation it belongs to, and a caller that had to remember to pass the root would get
//! that wrong exactly where it matters most.

use rusqlite::{Connection, named_params};
use zlogic_protocol::SessionId;

use crate::{Result, StoreError, now};

/// One variable, as the session layer holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEnvVar {
    pub name: String,
    pub value: String,
    pub enabled: bool,
}

pub struct SessionEnvStore<'a> {
    conn: &'a Connection,
}

impl<'a> SessionEnvStore<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    /// The tree root for `session_id`. A session that has somehow lost its row is an error rather
    /// than a silent empty set: variables quietly disappearing is the failure mode here that would
    /// be hardest to notice.
    fn root_of(&self, session_id: SessionId) -> Result<String> {
        self.conn
            .query_row(
                "SELECT root_session_id FROM session WHERE session_id = :id",
                named_params! { ":id": session_id.to_string() },
                |row| row.get::<_, String>(0),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => StoreError::NoSuchSession(session_id),
                other => StoreError::Sqlite(other),
            })
    }

    pub fn list(&self, session_id: SessionId) -> Result<Vec<SessionEnvVar>> {
        let root = self.root_of(session_id)?;
        let mut stmt = self.conn.prepare(
            "SELECT name, value, enabled FROM session_env
             WHERE root_session_id = :root ORDER BY name",
        )?;
        let rows = stmt.query_map(named_params! { ":root": root }, |row| {
            Ok(SessionEnvVar {
                name: row.get(0)?,
                value: row.get(1)?,
                enabled: row.get::<_, i64>(2)? != 0,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Replaces the whole layer, so a table UI can delete a row and add another in one call
    /// without the intermediate state leaking into a command that happens to start meanwhile.
    pub fn replace(&self, session_id: SessionId, vars: &[SessionEnvVar]) -> Result<()> {
        let root = self.root_of(session_id)?;
        self.conn.execute(
            "DELETE FROM session_env WHERE root_session_id = :root",
            named_params! { ":root": root },
        )?;
        for var in vars {
            self.conn.execute(
                "INSERT INTO session_env (root_session_id, name, value, enabled)
                 VALUES (:root, :name, :value, :enabled)",
                named_params! {
                    ":root": root,
                    ":name": var.name,
                    ":value": var.value,
                    ":enabled": i64::from(var.enabled),
                    ":ts": now(),
                },
            )?;
        }
        Ok(())
    }
}
