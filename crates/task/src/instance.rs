//! Which engine process owns a run, and whether that process is still there to own it.
//!
//! A run's future lives in the process that claimed it, so a row left behind by a process that
//! died can never finish — it has to be interrupted. Telling those rows apart from the ones a
//! **live** process is working on is what this module is for: several hosts share one database
//! (the desktop app and a local daemon both bootstrap an engine over the same `state.db`), and a
//! host that starts up must not take work away from the hosts already running.
//!
//! # Liveness is a heartbeat, not a pid
//! A pid says a process existed. It cannot say it still does — a crashed engine's pid is recycled,
//! and a recycled pid would make an orphaned row look owned forever. So each engine writes a row
//! here and refreshes it while it works; a row that stopped being refreshed is what "gone" means.
//! The interval and the staleness threshold match the session lock table's, and for the same
//! reason: one missed tick must not make a working engine look dead, because that is exactly how a
//! healthy run gets interrupted.

use std::collections::HashSet;

use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, named_params};
use uuid::Uuid;

use crate::store::Result;

/// How often a live engine refreshes its row.
pub const HEARTBEAT_INTERVAL: Duration = Duration::seconds(15);

/// An instance counts as gone once its heartbeat is older than this. Three intervals, so one
/// missed tick — a GC pause, a busy machine, a store call that held the lane — does not hand a
/// working engine's runs to somebody else.
pub const STALE_AFTER: Duration = Duration::seconds(60);

/// Identity of this engine process. A fresh value per process: two engines must never agree on
/// who owns a run, and a reused id would let a new process inherit the old one's rows.
pub fn new_instance_id() -> String {
    Uuid::now_v7().to_string()
}

/// The moment before which an instance is treated as gone. Callers pass the value they observed
/// into the writes that depend on it, so a decision and the write that acts on it are judged
/// against one clock reading instead of two.
pub fn live_cutoff() -> DateTime<Utc> {
    Utc::now() - STALE_AFTER
}

/// The engine processes sharing this database.
pub struct EngineInstance<'a> {
    conn: &'a Connection,
}

impl<'a> EngineInstance<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    /// Records this process as alive. Idempotent, so a host that bootstraps twice does not need
    /// to know whether it already did.
    pub fn register(&self, instance_id: &str, host_kind: &str, pid: u32) -> Result<()> {
        let now = Utc::now();
        self.conn.execute(
            "INSERT INTO engine_instance (instance_id, host_kind, pid, started_at, heartbeat_at)
             VALUES (:instance_id, :host_kind, :pid, :now, :now)
             ON CONFLICT(instance_id) DO UPDATE SET
                 host_kind = excluded.host_kind,
                 pid = excluded.pid,
                 heartbeat_at = excluded.heartbeat_at",
            named_params! {
                ":instance_id": instance_id,
                ":host_kind": host_kind,
                ":pid": pid,
                ":now": now,
            },
        )?;
        Ok(())
    }

    /// Refreshes the heartbeat. False means the row is gone — this id was never registered, or
    /// something removed it — which the caller should treat as "do not claim anything".
    pub fn heartbeat(&self, instance_id: &str) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE engine_instance SET heartbeat_at = :now WHERE instance_id = :instance_id",
            named_params! { ":now": Utc::now(), ":instance_id": instance_id },
        )?;
        Ok(changed == 1)
    }

    /// The instances still refreshing themselves. Rows are never deleted: an instance that comes
    /// back with the same id is impossible, so a leftover row only ever means "gone", and the
    /// heartbeat is what tells the two apart.
    pub fn live(&self, cutoff: DateTime<Utc>) -> Result<HashSet<String>> {
        let mut statement = self
            .conn
            .prepare("SELECT instance_id FROM engine_instance WHERE heartbeat_at >= :cutoff")?;
        let rows = statement.query_map(named_params! { ":cutoff": cutoff }, |row| {
            row.get::<_, String>(0)
        })?;
        Ok(rows.collect::<rusqlite::Result<HashSet<String>>>()?)
    }
}