//! The `session_locks` table — cross-process mutual exclusion for a live turn.
//! A row exists exactly while a turn is running, so "does this session have a live turn?"
//! is a lookup here rather than a nullable column on `session`.
//! # Staleness is judged by the heartbeat, not by how long the lock is held
//! A turn can legitimately run for hours (a long build, a tool waiting on the user), and it
//! heartbeats the whole time. A crashed holder stops heartbeating and becomes reclaimable
//! within a couple of intervals. Using "held for longer than N" as the criterion would kill
//! healthy long turns, which is the worse failure — the user loses work that was fine.
//! # Taking over requires the value you saw
//! [`SessionLockStore::steal`] carries the `holder_id` the caller observed. Two processes
//! can both decide a lock is stale; only the one whose observation is still current wins.
//! Without that condition they would both take over and write the same session at once.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, OptionalExtension, named_params};
use zlogic_protocol::{HolderId, SessionId, TurnId, WorkspaceId};

use crate::{Result, now};

/// How often the holder is expected to heartbeat.
pub const HEARTBEAT_INTERVAL: Duration = Duration::seconds(20);

/// A lock is stale once its heartbeat is older than this. Three intervals, so one missed
/// tick (a GC pause, a busy machine) does not hand the session to someone else.
pub const STALE_AFTER: Duration = Duration::seconds(60);

#[derive(Debug, Clone, PartialEq)]
pub struct SessionLock {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub holder_id: HolderId,
    pub holder_kind: Option<String>,
    pub pid: u32,
    pub acquired_at: DateTime<Utc>,
    pub heartbeat_at: DateTime<Utc>,
}

impl SessionLock {
    /// Whether the heartbeat has stopped for long enough to reclaim.
    pub fn is_stale_at(&self, at: DateTime<Utc>) -> bool {
        at - self.heartbeat_at > STALE_AFTER
    }

    pub fn is_stale(&self) -> bool {
        self.is_stale_at(Utc::now())
    }
}

/// What happened when trying to claim a session.
#[derive(Debug, Clone, PartialEq)]
pub enum LockOutcome {
    Acquired(SessionLock),
    /// Somebody else holds it and is still alive. Carries their lock so the UI can say who.
    Busy(SessionLock),
}

pub struct SessionLockStore<'a> {
    conn: &'a Connection,
}

impl<'a> SessionLockStore<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    /// Claims the session for `turn_id`, reclaiming a stale lock if there is one.
    pub fn acquire(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        holder_kind: Option<&str>,
    ) -> Result<LockOutcome> {
        let holder_id = HolderId::new();
        let ts = now();
        let pid = std::process::id();

        // `INSERT … ON CONFLICT DO NOTHING` inside SQLite's write transaction is what makes
        // this exclusive across processes: only one INSERT can win.
        let inserted = self.conn.execute(
            "INSERT INTO session_locks
                (session_id, turn_id, holder_id, holder_kind, pid, acquired_at, heartbeat_at)
             VALUES (:session_id, :turn_id, :holder_id, :holder_kind, :pid, :ts, :ts)
             ON CONFLICT(session_id) DO NOTHING",
            named_params! {
                ":session_id": session_id,
                ":turn_id": turn_id,
                ":holder_id": holder_id,
                ":holder_kind": holder_kind,
                ":pid": pid,
                ":ts": ts,
            },
        )?;
        if inserted == 1 {
            return Ok(LockOutcome::Acquired(self.expect(session_id)?));
        }

        let existing = self.expect(session_id)?;
        if !existing.is_stale_at(ts) {
            return Ok(LockOutcome::Busy(existing));
        }
        // Stale: take it over, but only if nobody moved it since we looked.
        if self.steal(session_id, existing.holder_id, turn_id, holder_kind)? {
            Ok(LockOutcome::Acquired(self.expect(session_id)?))
        } else {
            Ok(LockOutcome::Busy(self.expect(session_id)?))
        }
    }

    /// Takes over a lock whose holder is presumed dead.
    /// `observed_holder` is the `holder_id` the caller saw when it judged the lock stale.
    /// Passing it makes the update conditional, so a second process that reached the same
    /// conclusion fails instead of also taking over.
    /// A steal flips which turn is live, so `session.last_message_at` is recomputed in the
    /// same commit: the rescued turn's in-flight entries start counting (it is now a completed
    /// turn), the new turn's stop being counted.
    pub fn steal(
        &self,
        session_id: SessionId,
        observed_holder: HolderId,
        turn_id: TurnId,
        holder_kind: Option<&str>,
    ) -> Result<bool> {
        let ts = now();
        let tx = crate::tx_or_join(self.conn)?;
        let n = self.conn.execute(
            "UPDATE session_locks
             SET turn_id = :turn_id, holder_id = :holder_id, holder_kind = :holder_kind,
                 pid = :pid, acquired_at = :ts, heartbeat_at = :ts
             WHERE session_id = :session_id AND holder_id = :observed_holder",
            named_params! {
                ":turn_id": turn_id,
                ":holder_id": HolderId::new(),
                ":holder_kind": holder_kind,
                ":pid": std::process::id(),
                ":ts": ts,
                ":session_id": session_id,
                ":observed_holder": observed_holder,
            },
        )?;
        if n == 1 {
            crate::entry::EntryStore::new(self.conn).recompute_stats(session_id)?;
        }
        if let Some(tx) = tx {
            tx.commit()?;
        }
        Ok(n == 1)
    }

    /// Refreshes the heartbeat. Returns `false` when this holder no longer owns the lock —
    /// a revived process must notice it was taken over and stop writing.
    pub fn heartbeat(&self, session_id: SessionId, holder_id: HolderId) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE session_locks SET heartbeat_at = :ts
             WHERE session_id = :session_id AND holder_id = :holder_id",
            named_params! { ":ts": now(), ":session_id": session_id, ":holder_id": holder_id },
        )?;
        Ok(n == 1)
    }

    /// Releases. Conditional on the holder, so a late release cannot free somebody else's
    /// turn.
    /// Releasing ends the turn, so `session.last_message_at` is recomputed in the same commit:
    /// the turn's final entries stop being excluded from `last_message_at` the moment it stops
    /// being live.
    pub fn release(&self, session_id: SessionId, holder_id: HolderId) -> Result<bool> {
        let tx = crate::tx_or_join(self.conn)?;
        let n = self.conn.execute(
            "DELETE FROM session_locks
             WHERE session_id = :session_id AND holder_id = :holder_id",
            named_params! { ":session_id": session_id, ":holder_id": holder_id },
        )?;
        if n == 1 {
            crate::entry::EntryStore::new(self.conn).recompute_stats(session_id)?;
        }
        if let Some(tx) = tx {
            tx.commit()?;
        }
        Ok(n == 1)
    }

    pub fn get(&self, session_id: SessionId) -> Result<Option<SessionLock>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {COLS} FROM session_locks WHERE session_id = :session_id"),
                named_params! { ":session_id": session_id },
                map_row,
            )
            .optional()?)
    }

    /// The live turn, if any. This is where "is a turn running" is answered now.
    pub fn live_turn(&self, session_id: SessionId) -> Result<Option<TurnId>> {
        Ok(self.get(session_id)?.map(|l| l.turn_id))
    }

    pub fn live_turns_in(&self, workspace_id: WorkspaceId) -> Result<HashMap<SessionId, TurnId>> {
        let mut st = self.conn.prepare(
            "SELECT l.session_id, l.turn_id
             FROM session_locks l
             JOIN session s ON s.session_id = l.session_id
             WHERE s.workspace_id = :workspace_id",
        )?;
        let rows = st.query_map(named_params! { ":workspace_id": workspace_id }, |r| {
            Ok((r.get::<_, SessionId>(0)?, r.get::<_, TurnId>(1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<HashMap<SessionId, TurnId>>>()?)
    }

    pub fn all(&self) -> Result<Vec<SessionLock>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {COLS} FROM session_locks ORDER BY acquired_at"
        ))?;
        Ok(st
            .query_map([], map_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Locks whose heartbeat stopped. Startup housekeeping uses this to reclaim after a
    /// crash; it does **not** resume those turns, only frees the sessions.
    pub fn stale(&self, at: DateTime<Utc>) -> Result<Vec<SessionLock>> {
        Ok(self
            .all()?
            .into_iter()
            .filter(|l| l.is_stale_at(at))
            .collect())
    }

    fn expect(&self, session_id: SessionId) -> Result<SessionLock> {
        self.get(session_id)?.ok_or(crate::StoreError::NotFound {
            kind: "session_lock",
            id: session_id.to_string(),
        })
    }
}

const COLS: &str = "session_id, turn_id, holder_id, holder_kind, pid, acquired_at, heartbeat_at";

fn map_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SessionLock> {
    Ok(SessionLock {
        session_id: r.get("session_id")?,
        turn_id: r.get("turn_id")?,
        holder_id: r.get("holder_id")?,
        holder_kind: r.get("holder_kind")?,
        pid: r.get::<_, i64>("pid")? as u32,
        acquired_at: r.get("acquired_at")?,
        heartbeat_at: r.get("heartbeat_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Db, NewSession};
    use zlogic_protocol::WorkspaceId;

    fn setup() -> (Db, SessionId) {
        let db = Db::open_in_memory().unwrap();
        let s = db
            .sessions()
            .create(NewSession::root(WorkspaceId::new()))
            .unwrap();
        (db, s.session_id)
    }

    fn backdate(db: &Db, session_id: SessionId, ago: Duration) {
        db.conn()
            .execute(
                "UPDATE session_locks SET heartbeat_at = :ts WHERE session_id = :session_id",
                named_params! { ":ts": Utc::now() - ago, ":session_id": session_id },
            )
            .unwrap();
    }

    #[test]
    fn acquire_is_exclusive() {
        let (db, sid) = setup();
        let l = db.locks();
        let t1 = TurnId::new();

        let LockOutcome::Acquired(lock) = l.acquire(sid, t1, Some("cli")).unwrap() else {
            panic!("first acquire must win");
        };
        assert_eq!(lock.turn_id, t1);

        match l.acquire(sid, TurnId::new(), Some("desktop")).unwrap() {
            LockOutcome::Busy(held) => {
                assert_eq!(held.turn_id, t1);
                assert_eq!(
                    held.holder_kind.as_deref(),
                    Some("cli"),
                    "the UI can say who holds it"
                );
            }
            LockOutcome::Acquired(_) => panic!("second acquire must not win"),
        }
    }

    #[test]
    fn live_turn_comes_from_the_lock_table() {
        let (db, sid) = setup();
        assert_eq!(db.locks().live_turn(sid).unwrap(), None);

        let t = TurnId::new();
        db.locks().acquire(sid, t, None).unwrap();
        assert_eq!(db.locks().live_turn(sid).unwrap(), Some(t));

        let holder = db.locks().get(sid).unwrap().unwrap().holder_id;
        db.locks().release(sid, holder).unwrap();
        assert_eq!(db.locks().live_turn(sid).unwrap(), None);
    }

    #[test]
    fn live_turns_in_covers_only_its_workspace() {
        let db = Db::open_in_memory().unwrap();
        let ws_a = WorkspaceId::new();
        let ws_b = WorkspaceId::new();
        let a1 = db
            .sessions()
            .create(NewSession::root(ws_a))
            .unwrap()
            .session_id;
        let a2 = db
            .sessions()
            .create(NewSession::root(ws_a))
            .unwrap()
            .session_id;
        let b1 = db
            .sessions()
            .create(NewSession::root(ws_b))
            .unwrap()
            .session_id;

        db.locks().acquire(a1, TurnId::new(), None).unwrap();
        let t2 = TurnId::new();
        db.locks().acquire(a2, t2, None).unwrap();
        db.locks().acquire(b1, TurnId::new(), None).unwrap();

        let live = db.locks().live_turns_in(ws_a).unwrap();
        assert_eq!(live.len(), 2);
        assert_eq!(live[&a2], t2);
        assert!(!live.contains_key(&b1));
    }

    /// A long-running turn keeps heartbeating and must never be considered stale.
    #[test]
    fn a_long_turn_that_heartbeats_stays_valid() {
        let (db, sid) = setup();
        let l = db.locks();
        let LockOutcome::Acquired(lock) = l.acquire(sid, TurnId::new(), None).unwrap() else {
            panic!()
        };

        // Held for an hour, but the heartbeat is current.
        db.conn()
            .execute(
                "UPDATE session_locks SET acquired_at = :ts",
                named_params! { ":ts": Utc::now() - Duration::hours(1) },
            )
            .unwrap();
        assert!(
            !l.get(sid).unwrap().unwrap().is_stale(),
            "duration held is not the criterion"
        );
        assert!(l.heartbeat(sid, lock.holder_id).unwrap());
    }

    #[test]
    fn a_stale_lock_is_reclaimed_on_the_next_acquire() {
        let (db, sid) = setup();
        let l = db.locks();
        l.acquire(sid, TurnId::new(), Some("cli")).unwrap();
        backdate(&db, sid, STALE_AFTER + Duration::seconds(5));

        let t2 = TurnId::new();
        let LockOutcome::Acquired(lock) = l.acquire(sid, t2, Some("desktop")).unwrap() else {
            panic!("a dead holder must not block the session forever");
        };
        assert_eq!(lock.turn_id, t2);
        assert_eq!(lock.holder_kind.as_deref(), Some("desktop"));
    }

    /// Two processes both see the same stale lock. Only one may take over.
    #[test]
    fn concurrent_steals_cannot_both_win() {
        let (db, sid) = setup();
        let l = db.locks();
        l.acquire(sid, TurnId::new(), None).unwrap();
        let observed = l.get(sid).unwrap().unwrap().holder_id;
        backdate(&db, sid, STALE_AFTER * 2);

        assert!(l.steal(sid, observed, TurnId::new(), Some("a")).unwrap());
        assert!(
            !l.steal(sid, observed, TurnId::new(), Some("b")).unwrap(),
            "the second process observed a holder that is no longer current"
        );
        assert_eq!(
            l.get(sid).unwrap().unwrap().holder_kind.as_deref(),
            Some("a")
        );
    }

    /// A revived process must learn that it lost the lock and stop writing.
    #[test]
    fn heartbeat_fails_after_being_taken_over() {
        let (db, sid) = setup();
        let l = db.locks();
        let LockOutcome::Acquired(old) = l.acquire(sid, TurnId::new(), None).unwrap() else {
            panic!()
        };
        backdate(&db, sid, STALE_AFTER * 2);
        l.steal(sid, old.holder_id, TurnId::new(), None).unwrap();

        assert!(
            !l.heartbeat(sid, old.holder_id).unwrap(),
            "the old holder must find out"
        );
        assert!(
            !l.release(sid, old.holder_id).unwrap(),
            "and must not free the new turn"
        );
    }

    #[test]
    fn release_is_idempotent_and_holder_scoped() {
        let (db, sid) = setup();
        let l = db.locks();
        let LockOutcome::Acquired(lock) = l.acquire(sid, TurnId::new(), None).unwrap() else {
            panic!()
        };
        assert!(l.release(sid, lock.holder_id).unwrap());
        assert!(
            !l.release(sid, lock.holder_id).unwrap(),
            "releasing twice is a no-op"
        );
    }

    #[test]
    fn stale_lists_only_what_stopped_heartbeating() {
        let db = Db::open_in_memory().unwrap();
        let ws = WorkspaceId::new();
        let a = db
            .sessions()
            .create(NewSession::root(ws))
            .unwrap()
            .session_id;
        let b = db
            .sessions()
            .create(NewSession::root(ws))
            .unwrap()
            .session_id;
        db.locks().acquire(a, TurnId::new(), None).unwrap();
        db.locks().acquire(b, TurnId::new(), None).unwrap();
        backdate(&db, a, STALE_AFTER * 2);

        let stale = db.locks().stale(Utc::now()).unwrap();
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].session_id, a);
    }

    #[test]
    fn deleting_a_session_drops_its_lock() {
        let (db, sid) = setup();
        db.locks().acquire(sid, TurnId::new(), None).unwrap();
        db.sessions().delete(sid).unwrap();
        assert!(db.locks().get(sid).unwrap().is_none());
    }
}
