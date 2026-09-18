//! # zlogic-store
//! SQLite persistence: sessions, locks, entries, usage, mailbox.
//! # Column conventions
//! Every table has `id INTEGER PRIMARY KEY` (a compact internal key, used for
//! table-to-table references) plus `xxx_id TEXT NOT NULL UNIQUE` — the external identity,
//! a UUID v7. Ids are typed newtypes (`SessionId`, `TurnId`, …) so passing a turn id where
//! a session id belongs does not compile; they still store and serialize as plain strings.
//! Timestamps are `DateTime<Utc>`, never integers. Enums store their explicit wire name,
//! never a discriminant. JSON columns go through [`Json`] rather than hand-rolled
//! `to_string()` calls, so a malformed value surfaces as a typed error at read time instead
//! of a `.unwrap()` deep in a row mapper.
//! # Named parameters everywhere
//! Positional `?1 ?2 ?3` breaks silently when a column is inserted in the middle of a
//! statement: everything still compiles, the values just land in the wrong columns. Named
//! parameters make that a runtime error naming the parameter.
//! # Multi-process
//! `state.db` is opened by several processes at once (the CLI and the GUI host, several
//! sessions). In `open()`, **`busy_timeout` must be set before `journal_mode=wal`** —
//! switching to WAL itself takes a write lock, and that is the classic source of
//! "database is locked" on first run.

pub mod agent_profile;
pub mod entry;
pub mod lock;
pub mod mailbox;
pub mod maintenance;
pub mod memory;
pub mod resource;
pub mod schema;
pub mod session;
pub mod usage;
pub mod workspace;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, PoisonError};

use chrono::{DateTime, Duration, Utc};
use rusqlite::Connection;
use rusqlite::OpenFlags;
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use serde::Serialize;
use serde::de::DeserializeOwned;

pub use agent_profile::{AgentProfileRecord, AgentProfileStore};
pub use entry::{ConversationStats, EntryKind, EntryRecord, EntryStore, NewEntry, TurnAsset};
pub use lock::{LockOutcome, SessionLock, SessionLockStore};
pub use mailbox::{Delivery, MailboxRecord, MailboxStore};
pub use memory::MemoryStore;
pub use resource::{NewResource, ResourceRecord, ResourceStore};
pub use session::{
    AgentPath, NewSession, SessionKind, SessionQuery, SessionRecord, SessionStore, TitleSource,
};
pub use usage::{
    NewUsage, ToolUsageAggregate, TurnEnvelope, UsageAggregate, UsageAggregatePart, UsageQuery,
    UsageRecord, UsageRow, UsageStore,
};
pub use workspace::{WorkspaceRecord, WorkspaceStore};
pub use zlogic_objects::{ObjectRef, ObjectRole};
pub use zlogic_paths::normalise;
pub use zlogic_protocol::usage::Purpose;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("no such session: {0}")]
    NoSuchSession(zlogic_protocol::SessionId),
    #[error("no such {kind}: {id}")]
    NotFound { kind: &'static str, id: String },
    #[error("corrupt row: {0}")]
    Corrupt(String),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Object(#[from] zlogic_objects::ObjectError),
}

pub type Result<T> = std::result::Result<T, StoreError>;

pub fn now() -> DateTime<Utc> {
    Utc::now()
}

/// Starts a transaction, or joins the caller's if one is already open.
/// SQLite has no nested transactions, and these stores compose: `MailboxStore::deliver` wraps
/// `EntryStore::append` so that delivery is one atomic move. Each of them wants atomicity on its
/// own too, so the inner one must join rather than fail.
/// `None` means "already inside someone else's transaction" — do not commit; their commit covers
/// these writes, and their rollback undoes them.
pub(crate) fn tx_or_join(conn: &Connection) -> Result<Option<rusqlite::Transaction<'_>>> {
    if conn.is_autocommit() {
        Ok(Some(conn.unchecked_transaction()?))
    } else {
        Ok(None)
    }
}

/// A JSON-encoded column.
/// Exists so that decoding failures become `FromSqlError` at the row boundary. Passing
/// `Value::to_string()` around instead means every read site needs its own `from_str` plus
/// its own error handling, and one of them will end up as `.unwrap()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Json<T>(pub T);

impl<T> Json<T> {
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T: Serialize> ToSql for Json<T> {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        let s = serde_json::to_string(&self.0)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        Ok(ToSqlOutput::from(s))
    }
}

impl<T: DeserializeOwned> FromSql for Json<T> {
    fn column_result(v: ValueRef<'_>) -> FromSqlResult<Self> {
        let s = v.as_str()?;
        serde_json::from_str(s)
            .map(Json)
            .map_err(|e| FromSqlError::Other(Box::new(e)))
    }
}

/// A `Db` pooled across tasks, in two lanes.
/// `rusqlite::Connection` is `Send` but **not `Sync`**, so a shared `Db` used to hide behind a
/// mutex that serialised every store access — the lock contention you saw as `wait_ms=100..350`
/// on long sessions. Instead there is a **connection pool**: each [`SharedStore::with`] borrows
/// its own connection and returns it afterwards, so concurrent reads run in parallel (WAL) and
/// writes are arbitrated by SQLite's own write lock + `busy_timeout`. The only way in is
/// [`SharedStore::with`], which takes a closure: a connection cannot outlive it, and no `.await`
/// can appear inside one. A panic in the closure still returns the connection (Drop guard), so
/// one bad tool call cannot deadlock or poison the pool.
///
/// Reports live on a **second lane** ([`SharedStore::with_report_named`]): their own small pool of
/// read-only connections. A report is allowed to be slow, but it must not be able to starve the
/// interactive lane — before this split, a two-second `usage.summary` was what turned every other
/// store call into a `wait_ms=3000, hold_ms=0` log line.
///
/// In-memory databases (tests) keep the old single-connection semantics: the pool never opens
/// another connection for them, and both lanes share the one connection.
#[derive(Clone)]
pub struct SharedStore {
    interactive: Arc<Pool>,
    report: Arc<Pool>,
}

const DB_POOL_CAP: usize = 8;
/// Reports are allowed to be slow, so this lane is sized to keep them *off* the interactive lane
/// rather than to run many at once. Concurrent identical reports are collapsed by the caller.
const DB_REPORT_POOL_CAP: usize = 2;

/// Which pool a store call borrows from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lane {
    Interactive,
    Report,
}

impl Lane {
    fn as_str(self) -> &'static str {
        match self {
            Lane::Interactive => "interactive",
            Lane::Report => "report",
        }
    }

    fn connect(self, path: &Path) -> Result<Db> {
        match self {
            Lane::Interactive => Db::connect(path),
            Lane::Report => Db::connect_report(path),
        }
    }
}

struct Pool {
    lane: Lane,
    cap: usize,
    path: Option<PathBuf>,
    inner: Mutex<PoolInner>,
    available: Condvar,
}

struct PoolInner {
    idle: Vec<Db>,
    open: usize,
    opening: usize,
}

impl SharedStore {
    pub fn new(db: Db) -> Self {
        let path = db.path.clone();
        let interactive = Arc::new(Pool {
            lane: Lane::Interactive,
            cap: DB_POOL_CAP,
            path: path.clone(),
            inner: Mutex::new(PoolInner {
                idle: vec![db],
                open: 1,
                opening: 0,
            }),
            available: Condvar::new(),
        });
        // An in-memory database has exactly one connection; sharing it is more honest than opening
        // a second, empty one.
        let report = match path {
            None => Arc::clone(&interactive),
            Some(path) => Arc::new(Pool {
                lane: Lane::Report,
                cap: DB_REPORT_POOL_CAP,
                path: Some(path),
                inner: Mutex::new(PoolInner {
                    idle: Vec::new(),
                    open: 0,
                    opening: 0,
                }),
                available: Condvar::new(),
            }),
        };
        Self {
            interactive,
            report,
        }
    }

    /// Runs `f` with a pooled database connection.
    /// Keep the closure short and synchronous. The connection is returned to the pool when the
    /// closure finishes — including when it panics.
    /// Prefer [`SharedStore::with_named`] in production call sites so long holds can be attributed
    /// to a concrete store operation.
    #[track_caller]
    pub fn with<R>(&self, f: impl FnOnce(&Db) -> R) -> R {
        self.with_impl(&self.interactive, None, std::panic::Location::caller(), f)
    }

    /// Runs `f` with a pooled connection and records the operation name in contention logs.
    /// The closure should contain only database work plus the minimum row mapping required to
    /// produce its return value. CPU-heavy processing, JSON transformation, sorting and UI-model
    /// construction should happen after the closure returns so the connection is released early.
    #[track_caller]
    pub fn with_named<R>(&self, operation: &'static str, f: impl FnOnce(&Db) -> R) -> R {
        self.with_impl(
            &self.interactive,
            Some(operation),
            std::panic::Location::caller(),
            f,
        )
    }

    /// [`SharedStore::with_named`] with the caller's location passed in.
    /// For call sites that run the closure on another thread: `#[track_caller]` cannot see through
    /// a `spawn_blocking` closure, so without this every offloaded report would be logged against
    /// the offload helper instead of the engine call site that asked for it.
    pub fn with_named_at<R>(
        &self,
        operation: &'static str,
        caller: &'static std::panic::Location<'static>,
        f: impl FnOnce(&Db) -> R,
    ) -> R {
        self.with_impl(&self.interactive, Some(operation), caller, f)
    }

    /// Runs `f` on the report lane: its own pool of read-only connections.
    /// A report may be slow — that is the point of the lane — so it can at worst wait for another
    /// report, never for a chat query, and it can never write.
    #[track_caller]
    pub fn with_report_named<R>(&self, operation: &'static str, f: impl FnOnce(&Db) -> R) -> R {
        self.with_impl(
            &self.report,
            Some(operation),
            std::panic::Location::caller(),
            f,
        )
    }

    /// [`SharedStore::with_report_named`] with the caller's location passed in — see
    /// [`SharedStore::with_named_at`].
    pub fn with_report_named_at<R>(
        &self,
        operation: &'static str,
        caller: &'static std::panic::Location<'static>,
        f: impl FnOnce(&Db) -> R,
    ) -> R {
        self.with_impl(&self.report, Some(operation), caller, f)
    }

    fn with_impl<R>(
        &self,
        pool: &Arc<Pool>,
        operation: Option<&'static str>,
        caller: &'static std::panic::Location<'static>,
        f: impl FnOnce(&Db) -> R,
    ) -> R {
        let started = std::time::Instant::now();
        let guard = DbGuard {
            pool,
            db: Some(pool.acquire()),
        };

        let acquired = std::time::Instant::now();
        let out = f(guard.db.as_ref().expect("acquired connection"));

        // Explicitly release before logging/returning. This keeps any formatting or tracing work
        // out of the connection hold time.
        drop(guard);

        let finished = std::time::Instant::now();
        let wait_ms = acquired.duration_since(started).as_millis() as u64;
        let hold_ms = finished.duration_since(acquired).as_millis() as u64;
        let total_ms = finished.duration_since(started).as_millis() as u64;

        let tripped = slow_reasons(pool.lane, wait_ms, hold_ms, total_ms);
        if !tripped.is_empty() {
            tracing::warn!(
                target: "zlogic::store",
                operation = operation.unwrap_or("unknown"),
                lane = pool.lane.as_str(),
                caller_file = caller.file(),
                caller_line = caller.line(),
                caller_column = caller.column(),
                wait_ms,
                hold_ms,
                total_ms,
                reason = %tripped.join("+"),
                "shared store call slow"
            );
        }
        out
    }
}

/// Why a store call is worth a warning, or an empty list when it is not.
///
/// Waiting is a signal on **both** lanes: someone was blocked behind a holder, which is the one
/// thing that used to turn a slow report into a slow everything. Holding is only a signal beyond
/// what the lane is *for* — a report is expected to cost a second or more, so on the report lane a
/// hold is logged only when it is long even by report standards. Warning on every report would
/// train the log's only reader to ignore it.
fn slow_reasons(lane: Lane, wait_ms: u64, hold_ms: u64, total_ms: u64) -> Vec<&'static str> {
    let (hold_limit, total_limit) = match lane {
        Lane::Interactive => (200, 500),
        Lane::Report => (2_000, 2_000),
    };
    let mut tripped = Vec::new();
    if wait_ms > 100 {
        tripped.push("wait");
    }
    if hold_ms > hold_limit {
        tripped.push("hold");
    }
    if total_ms > total_limit {
        tripped.push("total");
    }
    tripped
}

struct DbGuard<'a> {
    pool: &'a Pool,
    db: Option<Db>,
}

impl Drop for DbGuard<'_> {
    fn drop(&mut self) {
        if let Some(db) = self.db.take() {
            self.pool.release(db);
        }
    }
}

impl Pool {
    fn acquire(&self) -> Db {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);

        loop {
            if let Some(db) = inner.idle.pop() {
                return db;
            }

            // Reserve capacity while still holding the mutex, then do the expensive open outside
            // the mutex. `opening` makes the pool cap a real cap under concurrent acquisition.
            if let Some(path) = self.path.as_ref() {
                if inner.open + inner.opening < self.cap {
                    inner.opening += 1;
                    let lane = self.lane;
                    let path = path.clone();

                    drop(inner);

                    // Pool expansion must only configure a connection. Migration/WAL setup is done
                    // by the initial connection, not by every lazily-created pool connection.
                    let result = lane.connect(&path);

                    inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
                    inner.opening -= 1;

                    match result {
                        Ok(db) => {
                            inner.open += 1;
                            return db;
                        }
                        Err(error) => {
                            tracing::warn!(
                                target: "zlogic::store",
                                lane = lane.as_str(),
                                path = %path.display(),
                                %error,
                                "opening a pooled connection failed; waiting for a returned one"
                            );
                            // Opening failed, so another waiter may now be able to reserve the
                            // released slot.
                            self.available.notify_one();
                            // A lane that has never opened a connection has nothing to wait for:
                            // without this pause, a failing open would spin at full speed.
                            if inner.open == 0 {
                                std::thread::sleep(std::time::Duration::from_millis(20));
                            }
                            continue;
                        }
                    }
                }
            }

            inner = self
                .available
                .wait(inner)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn release(&self, db: Db) {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        inner.idle.push(db);
        self.available.notify_one();
    }
}

pub struct Db {
    conn: Connection,
    path: Option<PathBuf>,
}

impl Db {
    /// Opens and initializes a database.
    /// This is the expensive path: directory creation, connection setup, WAL verification and
    /// schema migration happen here. Pool expansion uses [`Db::connect`] instead.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let db = Self::connect(path)?;
        db.ensure_wal()?;
        db.migrate()?;
        db.repair_missing_finals();
        Ok(db)
    }

    /// Opens an in-memory database and applies the schema.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::configure_connection(&conn)?;

        let db = Self { conn, path: None };
        db.migrate()?;
        db.repair_missing_finals();
        Ok(db)
    }

    /// Opens a file-backed connection suitable for pool expansion.
    /// This deliberately does not run migrations or change `journal_mode`: both are database-wide
    /// initialization concerns and doing them for every connection creates unnecessary lock and
    /// CPU contention under bursty workloads.
    fn connect(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::configure_connection(&conn)?;

        Ok(Self {
            conn,
            path: Some(path.to_path_buf()),
        })
    }

    /// Opens the report lane's connection: read-only, and additionally `query_only` so that a
    /// report cannot write even if it were handed a read-write connection.
    /// `SQLITE_OPEN_READ_ONLY` is refused for a WAL database whose `-shm` cannot be written, which
    /// cannot happen here (this process owns the directory and keeps connections open), but the
    /// fallback keeps a report lane from being a hard failure if it ever does.
    fn connect_report(path: &Path) -> Result<Self> {
        let conn = match Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
            Ok(conn) => conn,
            Err(error) => {
                tracing::warn!(
                    target: "zlogic::store",
                    path = %path.display(),
                    %error,
                    "could not open the report connection read-only; opening it read-write behind query_only"
                );
                Connection::open(path)?
            }
        };
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "query_only", true)?;

        Ok(Self {
            conn,
            path: Some(path.to_path_buf()),
        })
    }

    fn configure_connection(conn: &Connection) -> Result<()> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "synchronous", "normal")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        Ok(())
    }

    fn ensure_wal(&self) -> Result<()> {
        let mode: String = self
            .conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))?;

        if !mode.eq_ignore_ascii_case("wal") {
            self.conn.pragma_update(None, "journal_mode", "wal")?;
        }

        Ok(())
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn workspaces(&self) -> WorkspaceStore<'_> {
        WorkspaceStore::new(&self.conn)
    }

    pub fn sessions(&self) -> SessionStore<'_> {
        SessionStore::new(&self.conn)
    }

    pub fn locks(&self) -> SessionLockStore<'_> {
        SessionLockStore::new(&self.conn)
    }

    pub fn entries(&self) -> EntryStore<'_> {
        EntryStore::new(&self.conn)
    }

    pub fn agent_profiles(&self) -> AgentProfileStore<'_> {
        AgentProfileStore::new(&self.conn)
    }

    pub fn resources(&self) -> ResourceStore<'_> {
        ResourceStore::new(&self.conn)
    }

    pub fn maintenance(&self) -> maintenance::MaintenanceStore<'_> {
        maintenance::MaintenanceStore::new(&self.conn)
    }

    pub fn memories(&self) -> MemoryStore<'_> {
        MemoryStore::new(&self.conn)
    }

    pub fn usage(&self) -> UsageStore<'_> {
        UsageStore::new(&self.conn)
    }

    pub fn mailbox(&self) -> MailboxStore<'_> {
        MailboxStore::new(&self.conn)
    }

    /// Applies every migration past the recorded version, one at a time.
    /// Stepping rather than "apply everything if behind" is what lets an existing database move
    /// forward: version 1 gets only migration 2, not the whole history replayed.
    fn migrate(&self) -> Result<()> {
        let current: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap_or(0);
        for (i, m) in schema::MIGRATIONS.iter().enumerate() {
            let version = i as i64 + 1;
            if version <= current {
                continue;
            }
            // Each migration is one transaction: a failure leaves the version untouched, so the
            // next start retries it instead of running on a half-built schema.
            let tx = self.conn.unchecked_transaction()?;
            self.conn.execute_batch(m)?;
            self.conn.pragma_update(None, "user_version", version)?;
            tx.commit()?;
        }
        Ok(())
    }

    fn repair_missing_finals(&self) {
        let since = self
            .maintenance()
            .last_run(maintenance::IS_FINAL_BACKFILL)
            .ok()
            .flatten();
        match self.maintenance().claim(
            maintenance::IS_FINAL_BACKFILL,
            Duration::hours(1),
            Utc::now(),
        ) {
            Ok(true) => {}
            Ok(false) => return,
            Err(e) => {
                tracing::warn!(target: "zlogic::store", "could not claim the is_final backfill: {e}");
                return;
            }
        }

        match self.clear_duplicate_final_marks() {
            Ok(0) => {}
            Ok(n) => tracing::info!(
                target: "zlogic::store",
                cleared = n,
                "cleared duplicate is_final marks (a live turn had been stamped prematurely)"
            ),
            Err(e) => {
                tracing::warn!(target: "zlogic::store", "could not clear duplicate is_final marks: {e}")
            }
        }
        match self.stamp_missing_final_marks(since) {
            Ok(0) => {}
            Ok(n) => tracing::info!(
                target: "zlogic::store",
                repaired = n,
                "backfilled is_final on turns written before the marker existed"
            ),
            Err(e) => tracing::warn!(target: "zlogic::store", "could not backfill is_final: {e}"),
        }
    }

    fn clear_duplicate_final_marks(&self) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE session_entry SET is_final = 0
              WHERE is_final = 1
                AND seq <> (SELECT MAX(e.seq) FROM session_entry e
                             WHERE e.session_id = session_entry.session_id
                               AND e.turn_seq = session_entry.turn_seq
                               AND e.kind IN ('assistant_text', 'thinking'))",
            [],
        )?)
    }

    fn stamp_missing_final_marks(&self, since: Option<DateTime<Utc>>) -> Result<usize> {
        let mut sql = String::from(
            "UPDATE session_entry SET is_final = 1
              WHERE kind IN ('assistant_text', 'thinking')
                AND is_final = 0
                AND seq = (SELECT MAX(e2.seq) FROM session_entry e2
                            WHERE e2.session_id = session_entry.session_id
                              AND e2.turn_seq = session_entry.turn_seq
                              AND e2.kind IN ('assistant_text', 'thinking'))
                AND NOT EXISTS (SELECT 1 FROM session_entry f
                                 WHERE f.session_id = session_entry.session_id
                                   AND f.turn_seq = session_entry.turn_seq
                                   AND f.is_final = 1)
                AND NOT EXISTS (SELECT 1 FROM session_locks l
                                 WHERE l.session_id = session_entry.session_id
                                   AND l.turn_id = session_entry.turn_id)",
        );
        let params: Vec<(&str, Box<dyn rusqlite::ToSql>)> = match since {
            Some(since) => {
                sql.push_str(" AND created_at > :since");
                vec![(":since", Box::new(since.to_rfc3339()))]
            }
            None => Vec::new(),
        };
        let borrowed: Vec<(&str, &dyn rusqlite::ToSql)> =
            params.iter().map(|(k, v)| (*k, v.as_ref())).collect();
        Ok(self.conn.execute(&sql, borrowed.as_slice())?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A file-backed store, so the report lane really opens its own connection.
    fn file_store(tmp: &tempfile::TempDir) -> SharedStore {
        SharedStore::new(Db::open(&tmp.path().join("state.db")).unwrap())
    }

    #[test]
    fn the_report_lane_connection_is_read_only() {
        let tmp = tempfile::tempdir().unwrap();
        let store = file_store(&tmp);

        let write = store.with_report_named("test.report", |db| {
            db.conn()
                .execute("INSERT INTO usage_event (id, session_id, purpose, created_at) VALUES ('x', 'y', 'main', 'z')", [])
        });

        assert!(
            write.is_err(),
            "a report must not be able to write, even by mistake"
        );
    }

    #[test]
    fn reports_cannot_starve_the_interactive_lane() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let tmp = tempfile::tempdir().unwrap();
        let store = file_store(&tmp);

        // More reports than either lane can hold at once. With one shared pool these would occupy
        // every connection and the interactive call below would have to wait for one of them.
        let released = Arc::new(AtomicBool::new(false));
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let mut reports = Vec::new();
        for _ in 0..DB_POOL_CAP {
            let store = store.clone();
            let entered_tx = entered_tx.clone();
            let released = Arc::clone(&released);
            reports.push(std::thread::spawn(move || {
                store.with_report_named("test.report", |db| {
                    let one: i64 = db
                        .conn()
                        .query_row("SELECT 1", [], |row| row.get(0))
                        .unwrap();
                    assert_eq!(one, 1);
                    entered_tx.send(()).unwrap();
                    while !released.load(Ordering::Relaxed) {
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                });
            }));
        }
        drop(entered_tx);

        // Wait until the report lane is saturated, then the interactive lane must still answer —
        // from another thread, so a regression fails this test instead of hanging it.
        for _ in 0..DB_REPORT_POOL_CAP {
            entered_rx.recv().unwrap();
        }
        let (answered_tx, answered_rx) = std::sync::mpsc::channel();
        let interactive = store.clone();
        std::thread::spawn(move || {
            let one: i64 = interactive
                .with(|db| db.conn().query_row("SELECT 1", [], |row| row.get(0)))
                .unwrap();
            answered_tx.send(one).unwrap();
        });
        let answered = answered_rx.recv_timeout(std::time::Duration::from_secs(5));
        released.store(true, Ordering::Relaxed);
        for report in reports {
            report.join().unwrap();
        }
        assert_eq!(
            answered.expect("the interactive lane waited behind reports"),
            1
        );
    }

    #[test]
    fn the_slow_log_has_one_threshold_for_waiting_and_one_per_lane_for_holding() {
        // Waiting is a signal everywhere: something was blocked behind a holder.
        assert_eq!(slow_reasons(Lane::Interactive, 150, 10, 160), ["wait"]);
        assert_eq!(slow_reasons(Lane::Report, 150, 10, 160), ["wait"]);

        // Holding is judged against what the lane is for. A 400 ms error inside a report is not
        // worth a warning; the same hold on an interactive call is.
        assert!(slow_reasons(Lane::Report, 0, 400, 400).is_empty());
        assert_eq!(slow_reasons(Lane::Interactive, 0, 400, 400), ["hold"]);

        // A report that takes seconds still shows up.
        assert_eq!(
            slow_reasons(Lane::Report, 0, 2_500, 2_500),
            ["hold", "total"]
        );
    }

    #[test]
    fn opens_and_migrates() {
        let db = Db::open_in_memory().unwrap();
        let v: i64 = db
            .conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, schema::CURRENT_VERSION);
    }

    #[test]
    fn a_database_from_before_the_interaction_index_gains_one_that_is_used() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.db");
        {
            let db = Db::open(&path).unwrap();
            db.conn()
                .execute_batch(
                    "DROP INDEX IF EXISTS idx_entry_interaction;
                     PRAGMA user_version = 20;",
                )
                .unwrap();
        }

        let db = Db::open(&path).unwrap();
        let version: i64 = db
            .conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, schema::CURRENT_VERSION);

        // The plan is the point of the migration: startup reconciliation must not scan the table.
        let mut statement = db
            .conn()
            .prepare(
                "EXPLAIN QUERY PLAN
                 SELECT entry_id, session_id, seq, kind FROM session_entry
                 WHERE kind IN ('interaction_request', 'interaction_response')
                 ORDER BY session_id, seq",
            )
            .unwrap();
        let plan: Vec<String> = statement
            .query_map([], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            plan.iter()
                .any(|line| line.contains("idx_entry_interaction")),
            "the interaction index should serve the query, plan was {plan:?}"
        );
    }

    #[test]
    fn migration_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.db");
        drop(Db::open(&path).unwrap());
        let b = Db::open(&path).unwrap();
        assert!(
            b.sessions()
                .list(zlogic_protocol::WorkspaceId::new())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn an_existing_database_migrates_forward_over_an_alter_table() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.db");

        let session = {
            let db = Db::open(&path).unwrap();
            let s = db
                .sessions()
                .create(session::NewSession::root(
                    zlogic_protocol::WorkspaceId::new(),
                ))
                .unwrap();
            db.entries()
                .append(entry::NewEntry::new(
                    s.session_id,
                    zlogic_protocol::TurnId::new(),
                    1,
                    entry::EntryKind::AssistantText,
                    json!("old data"),
                ))
                .unwrap();
            s.session_id
        };

        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "DROP INDEX IF EXISTS idx_entry_final;
                 DROP INDEX IF EXISTS idx_entry_input;
                 ALTER TABLE session_entry DROP COLUMN display;
                 ALTER TABLE session_entry DROP COLUMN round_seq;
                 ALTER TABLE session_entry DROP COLUMN is_final;
                 ALTER TABLE workspaces DROP COLUMN pinned;
                 ALTER TABLE workspaces DROP COLUMN sort_order;
                 ALTER TABLE workspaces DROP COLUMN last_opened_at;
                 ALTER TABLE workspaces DROP COLUMN hidden;
                 ALTER TABLE workspaces DROP COLUMN tools;
                 ALTER TABLE usage_event DROP COLUMN request_started_at;
                 ALTER TABLE usage_event DROP COLUMN first_token_at;
                 ALTER TABLE usage_event DROP COLUMN completed_at;
                 ALTER TABLE session DROP COLUMN turn_count;
                 ALTER TABLE session DROP COLUMN last_message_at;
                 ALTER TABLE session DROP COLUMN effort;
                 ALTER TABLE mailbox DROP COLUMN thinking;
                 DROP INDEX IF EXISTS idx_entry_object_kind;
                 ALTER TABLE entry_object DROP COLUMN kind;
                 ALTER TABLE entry_object DROP COLUMN label;
                 ALTER TABLE entry_object DROP COLUMN meta;",
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 3i64).unwrap();
        }

        let db = Db::open(&path).unwrap();
        let v: i64 = db
            .conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, schema::CURRENT_VERSION);

        let rows = db.entries().list(session).unwrap();
        assert_eq!(rows.len(), 1, "a migration must not touch existing rows");
        assert_eq!(
            rows[0].display, None,
            "an old row has no projection and reads as None"
        );
    }

    #[test]
    fn creates_parent_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("deep").join("nested").join("state.db");
        assert!(Db::open(&path).is_ok());
        assert!(path.exists());
    }

    /// There is no `interaction` table: interactions live in `session_entry` so the
    /// timeline has one source of truth.
    #[test]
    fn there_is_no_separate_interaction_table() {
        let db = Db::open_in_memory().unwrap();
        let names: Vec<String> = db
            .conn()
            .prepare("SELECT name FROM sqlite_master WHERE type='table'")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(!names.iter().any(|n| n == "interaction"));
        assert!(names.iter().any(|n| n == "session_locks"));
    }

    /// The live turn is not a session column any more.
    #[test]
    fn session_table_has_no_lock_columns() {
        let db = Db::open_in_memory().unwrap();
        let cols: Vec<String> = db
            .conn()
            .prepare("SELECT name FROM pragma_table_info('session')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        for gone in [
            "live_turn_id",
            "owner_pid",
            "owner_pid_started_at",
            "claimed_at",
            "anchor_entry_id",
        ] {
            assert!(!cols.iter().any(|c| c == gone), "{gone} should be gone");
        }
        assert!(cols.iter().any(|c| c == "agent_paths"));
    }

    #[test]
    fn json_columns_round_trip_and_report_corruption() {
        let db = Db::open_in_memory().unwrap();
        db.conn().execute_batch("CREATE TABLE t (v TEXT)").unwrap();
        db.conn()
            .execute(
                "INSERT INTO t (v) VALUES (:v)",
                rusqlite::named_params! {
                    ":v": Json(json!({ "a": [1, 2, 3] })),
                },
            )
            .unwrap();

        let got: Json<serde_json::Value> = db
            .conn()
            .query_row("SELECT v FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(got.0, json!({ "a": [1, 2, 3] }));

        // A hand-corrupted value must surface as an error, not a panic.
        db.conn()
            .execute("UPDATE t SET v = '{not json'", [])
            .unwrap();
        let bad: rusqlite::Result<Json<serde_json::Value>> =
            db.conn().query_row("SELECT v FROM t", [], |r| r.get(0));
        assert!(bad.is_err());
    }
}
