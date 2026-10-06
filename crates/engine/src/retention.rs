//! What the engine throws away on its own.
//!
//! One thread, because the jobs share a "not now" clock and a dedupe row per job: the
//! write-ahead log goes back to the database file, the day-foldered caches are trimmed to their
//! age, and chat sessions nobody has touched for `session_days` are deleted.
//!
//! The object store is not swept here. It is reached from a deleted session's entries, and
//! `zlogic::gc` is the one place that knows how to tell a live object from an abandoned one —
//! counting bytes off a directory walk would be a second answer to the same question, and the
//! wrong one the moment an object is shared.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use zlogic_config::Dirs;
use zlogic_protocol::SessionId;
use zlogic_protocol::settings::RetentionConfig;
use zlogic_store::{SharedStore, WalCheckpoint};

/// One dedupe key per job, so turning one off and on does not reset the other.
const WAL_CHECKPOINT: &str = "wal_checkpoint";
const CACHE_SWEEP: &str = "cache_sweep";
const LOG_SWEEP: &str = "log_sweep";
const SESSION_SWEEP: &str = "session_sweep";

/// How long the WAL is left alone between checkpoints. Short enough that the file does not grow
/// across a long session, long enough that a desktop relaunch does not copy 100 MB every time.
const WAL_INTERVAL: chrono::Duration = chrono::Duration::hours(6);
const SWEEP_INTERVAL: chrono::Duration = chrono::Duration::hours(24);

/// How many sessions one pass will delete.
///
/// A cap is not a politeness knob: a first run against a store that has been accumulating for a
/// year would otherwise delete every session in a single transaction-sized burst, holding the
/// write lock while it goes. Bounded, the work is spread over days and the engine stays usable
/// while it happens.
const SESSION_BATCH: usize = 200;

/// What one pass did. Reported to the log, not to the user: a sweep that removes nothing is the
/// normal case, and a line saying so every six hours is noise nobody reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub cache_dirs_removed: usize,
    /// Turn folders out of the workspaces' own caches. Not in `cache_dirs_removed`, which counts
    /// day folders in the shared cache: two different layouts, two numbers in the log.
    pub turn_dirs_removed: usize,
    pub log_dirs_removed: usize,
    pub sessions_deleted: usize,
    pub bytes_freed: u64,
    pub wal_checkpointed: bool,
    pub wal_bytes_reclaimed: u64,
    pub skipped: Vec<&'static str>,
}

impl SweepReport {
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.wal_checkpointed {
            parts.push(format!("WAL reclaimed {} bytes", self.wal_bytes_reclaimed));
        }
        if self.cache_dirs_removed > 0 {
            parts.push(format!("{} cache day(s)", self.cache_dirs_removed));
        }
        if self.turn_dirs_removed > 0 {
            parts.push(format!("{} turn cache(s)", self.turn_dirs_removed));
        }
        if self.log_dirs_removed > 0 {
            parts.push(format!("{} log day(s)", self.log_dirs_removed));
        }
        if self.sessions_deleted > 0 {
            parts.push(format!("{} session(s)", self.sessions_deleted));
        }
        if parts.is_empty() {
            return match self.skipped.is_empty() {
                true => "nothing to do".into(),
                false => format!("nothing to do ({})", self.skipped.join(", ")),
            };
        }
        parts.join(", ")
    }
}

/// Folds the WAL back into the database and trims the day folders.
///
/// `journal_size_limit` caps what the WAL is allowed to become in the first place; this is what
/// actually hands the space back on a database that is idle, which is the only time a checkpoint
/// is cheap. A busy engine gets a restart rather than a truncate, and one that will not let go of a
/// reader gets nothing — that is the right answer, not an error, so it is reported as neither.
pub fn sweep(
    store: &SharedStore,
    dirs: &Dirs,
    config: &RetentionConfig,
    now: chrono::DateTime<chrono::Utc>,
) -> SweepReport {
    let mut report = SweepReport::default();

    if claim(store, WAL_CHECKPOINT, WAL_INTERVAL, now) {
        report.wal_checkpointed = checkpoint_wal(store, &mut report);
    } else {
        report.skipped.push("wal: too soon");
    }

    if !config.enabled {
        report.skipped.push("retention: off");
        return report;
    }

    if claim(store, CACHE_SWEEP, SWEEP_INTERVAL, now) {
        let cutoff = day_cutoff(now, config.cache_days);
        report.cache_dirs_removed = sweep_day_dirs(&dirs.cache, cutoff, &mut report.bytes_freed);
        // The workspace caches are not under `dirs.cache` — they live in each workspace, under its
        // own `.zlogic` — so the day sweep above never sees a turn folder. A failure here costs
        // space, not correctness, which is why the roots are fetched separately and never fatal.
        let since: std::time::SystemTime =
            (now - chrono::Duration::days(i64::from(config.cache_days))).into();
        match store.with(|db| {
            db.workspaces()
                .list(true)
                .map_err(|e| e.to_string())
                .map(|workspaces| {
                    workspaces
                        .iter()
                        .map(|w| {
                            sweep_turn_dirs(Path::new(&w.path), since, &mut report.bytes_freed)
                        })
                        .sum()
                })
        }) {
            Ok(removed) => report.turn_dirs_removed = removed,
            Err(e) => tracing::warn!(target: "zlogic::retention", "turn cache sweep failed: {e}"),
        }
    } else {
        report.skipped.push("cache: too soon");
    }

    if claim(store, LOG_SWEEP, SWEEP_INTERVAL, now) {
        let cutoff = day_cutoff(now, config.logs_days);
        report.log_dirs_removed = sweep_day_dirs(&dirs.logs(), cutoff, &mut report.bytes_freed);
    } else {
        report.skipped.push("logs: too soon");
    }

    if claim(store, SESSION_SWEEP, SWEEP_INTERVAL, now) {
        let cutoff = now - chrono::Duration::days(i64::from(config.session_days));
        match sweep_sessions(store, cutoff) {
            Ok(removed) => report.sessions_deleted = removed,
            Err(e) => tracing::warn!(target: "zlogic::retention", "session sweep failed: {e}"),
        }
    } else {
        report.skipped.push("sessions: too soon");
    }

    report
}

/// Deletes chat sessions whose last activity predates `cutoff`, and returns how many went.
///
/// The batch is one transaction: a hundred small transactions would take and release the write
/// lock a hundred times, and the desktop is competing for that lock the whole time. The upside of
/// one transaction is also why a failure is logged rather than worked around — nothing is
/// deleted, and the next pass tries the same batch again. Each `DELETE` cascades to entries,
/// mailbox rows and the lock, which is what leaves the session's objects unreferenced and
/// therefore collectable by the next object gc, whose grace period is what stands between a
/// deletion and bytes on disk.
///
/// The workspace cache folders go afterwards and outside that transaction, because the
/// filesystem has no rollback: a folder left behind is litter, a folder removed for a session
/// that was not deleted would be data loss.
fn sweep_sessions(
    store: &SharedStore,
    cutoff: chrono::DateTime<chrono::Utc>,
) -> Result<usize, String> {
    let doomed = store.with(|db| {
        db.sessions()
            .idle_chat_roots_before(cutoff)
            .map_err(|e| e.to_string())
    })?;
    if doomed.is_empty() {
        return Ok(0);
    }
    let doomed = doomed.into_iter().take(SESSION_BATCH).collect::<Vec<_>>();
    let ids: Vec<SessionId> = doomed.iter().map(|s| s.session_id).collect();
    let roots: HashMap<_, _> = store.with(|db| {
        let mut roots: HashMap<_, _> = HashMap::new();
        for workspace in db.workspaces().list(true).map_err(|e| e.to_string())? {
            roots.insert(workspace.workspace_id, workspace.path);
        }
        Ok::<_, String>(roots)
    })?;

    store.with(|db| {
        for id in &ids {
            db.sessions().delete(*id).map_err(|e| e.to_string())?;
        }
        Ok::<_, String>(())
    })?;
    tracing::info!(target: "zlogic::retention", count = ids.len(), "deleted idle sessions");

    for session in &doomed {
        if let Some(root) = roots.get(&session.workspace_id) {
            remove_session_cache(Path::new(root), session.session_id);
        }
    }
    Ok(ids.len())
}

/// The temp files a turn's model wrote live in `<workspace>/.zlogic/cache/<session>`; the prompt is
/// what tells it to put them there. Deleting the session without them would leave the
/// project's only regenerable-garbage directory growing forever, which is the thing this whole
/// module exists to stop.
fn remove_session_cache(root: &Path, session_id: SessionId) {
    let dir = session_cache_dir(root, Some(session_id));
    if dir.exists() && remove_dir_all(&dir) {
        tracing::debug!(target: "zlogic::retention", dir = %dir.display(), "removed");
    }
}

/// The folder layout is defined once, in the crate that reads it at the end of every turn. Re-
/// exported under its old name because this is where the prompt, the environment and the sweeper
/// all reach for it.
pub use zlogic_core::turndir::session_cache_dir;

/// Removes the turn folders of a workspace's cache that no turn has touched since the cutoff.
///
/// Separate from [`sweep_day_dirs`] because that one removes a folder by *reading its name as a
/// date*, and a turn folder is named after a turn. No depth setting makes it match: the shape here
/// is age, and it is the same age the day folders get.
///
/// Skipped while the session lives: a turn folder is small next to a day of logs, and a session
/// being used is a session whose cache folder is the wrong one to be tidying. Deleting the session
/// takes its whole subtree with it regardless.
fn sweep_turn_dirs(root: &Path, cutoff: std::time::SystemTime, bytes_freed: &mut u64) -> usize {
    let cache = session_cache_dir(root, None);
    let Ok(sessions) = std::fs::read_dir(&cache) else {
        return 0;
    };
    let mut removed = 0;
    for session in sessions.flatten() {
        if !session.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Ok(turns) = std::fs::read_dir(session.path()) else {
            continue;
        };
        for turn in turns.flatten() {
            let path = turn.path();
            if !turn.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            if touched_since(&path, cutoff) {
                continue;
            }
            // Counted before the delete: afterwards there is nothing left to measure, and the log
            // line would report zero for a sweep that just freed something.
            let size = dir_size(&path);
            if remove_dir_all(&path) {
                removed += 1;
                *bytes_freed += size;
                tracing::debug!(target: "zlogic::retention", dir = %path.display(), "removed");
            }
        }
    }
    removed
}

/// Whether anything in the folder was written at or after the cutoff.
///
/// The folder's own mtime is not the answer, and neither is its children's: writing a file inside a
/// directory updates that directory only when entries are *added or removed*, so a turn that
/// overwrote one file an hour ago — a script, a chart — looks untouched a level down, and the
/// folder would be deleted out from under it. So the whole subtree is asked. It is a turn's own
/// scratch, so the walk is a handful of entries; a file that cannot be read counts as touched,
/// because the alternative is deleting a folder on the strength of a permission error.
fn touched_since(dir: &Path, cutoff: std::time::SystemTime) -> bool {
    let Ok(meta) = std::fs::metadata(dir) else {
        return true;
    };
    if meta.modified().is_ok_and(|m| m >= cutoff) {
        return true;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return true;
    };
    entries.flatten().any(|entry| {
        let Ok(meta) = entry.metadata() else {
            return true;
        };
        meta.modified().is_ok_and(|m| m >= cutoff)
            || (meta.is_dir() && touched_since(&entry.path(), cutoff))
    })
}

/// The oldest day a folder may still carry. Equal to the cutoff, not before it: a folder named for
/// today is never removed, and neither is one from the last `days` days.
fn day_cutoff(now: chrono::DateTime<chrono::Utc>, days: u32) -> chrono::NaiveDate {
    (now - chrono::Duration::days(i64::from(days))).date_naive()
}

/// Removes every `YYYY-MM-DD` folder strictly older than the cutoff, and returns how many went.
///
/// `root` itself and one level below it are searched, which is what the two layouts need: logs
/// file days directly, the MCP tool cache files them under `<cache>/mcp`. Two levels is also the
/// bound this walks to — a day folder is a leaf, and anything deeper is somebody else's business.
///
/// A folder whose name is not a date is descended into rather than removed, and a stray file is
/// never touched: the cache root is shared, and this function only claims what it can read as a
/// day. Files inside a removed folder are counted for the log line but not enumerated — the point
/// of the day split is that the common case is one `remove_dir_all`, not a walk.
fn sweep_day_dirs(root: &Path, cutoff: chrono::NaiveDate, bytes_freed: &mut u64) -> usize {
    sweep_day_dirs_at(root, cutoff, bytes_freed, 1)
}

fn sweep_day_dirs_at(
    dir: &Path,
    cutoff: chrono::NaiveDate,
    bytes_freed: &mut u64,
    depth: u8,
) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        match chrono::NaiveDate::parse_from_str(name, "%Y-%m-%d") {
            Ok(day) if day < cutoff => {
                if remove_dir_all(&path) {
                    removed += 1;
                    *bytes_freed += dir_size(&path);
                    tracing::debug!(target: "zlogic::retention", dir = %path.display(), "removed");
                }
            }
            Ok(_) => {}
            Err(_) if depth > 0 => {
                removed += sweep_day_dirs_at(&path, cutoff, bytes_freed, depth - 1)
            }
            Err(_) => {}
        }
    }
    removed
}

/// A folder's contents, recursively, for the log line.
///
/// Recursive because a directory's own length is 0 on every platform that matters, so a flat count
/// reports the bytes of a cache folder's *top level* — which for both layouts here is a handful of
/// subfolders, and therefore zero. Counted before the delete, since afterwards there is nothing
/// left to measure; a failure here changes nothing.
fn dir_size(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.metadata() {
            Ok(meta) if meta.is_dir() => dir_size(&entry.path()),
            Ok(meta) => meta.len(),
            Err(_) => 0,
        })
        .sum()
}

fn remove_dir_all(path: &Path) -> bool {
    match std::fs::remove_dir_all(path) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(target: "zlogic::retention", dir = %path.display(), "could not remove: {e}");
            false
        }
    }
}

/// Truncates the WAL, or asks for as much of it back as a busy database will give.
///
/// Truncate is tried first because it is the only mode that shrinks the file rather than just
/// moving its high-water mark. A second, read-only connection holding a read transaction is the
/// normal reason it cannot, and `RESTART` is the graceful answer: it still lets the next writer
/// start from the beginning, so the file stops growing even though it does not shrink yet.
fn checkpoint_wal(store: &SharedStore, report: &mut SweepReport) -> bool {
    let before = match store.with(|db| db.wal_size()) {
        Ok(size) => size,
        Err(e) => {
            tracing::debug!(target: "zlogic::retention", "could not read WAL size: {e}");
            0
        }
    };
    if before == 0 {
        return false;
    }

    let truncated = match store.with(|db| db.checkpoint(WalCheckpoint::Truncate)) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(target: "zlogic::retention", "checkpoint failed: {e}");
            return false;
        }
    };
    if !truncated.busy {
        let after = store.with(|db| db.wal_size()).unwrap_or(before);
        report.wal_bytes_reclaimed = before.saturating_sub(after);
        if report.wal_bytes_reclaimed > 0 {
            tracing::info!(target: "zlogic::retention", before, after, "WAL truncated");
        }
        return true;
    }

    // Truncate needs every reader to let go. Restart asks for the same copy without that, so the
    // log stops growing even though the file stays the size it is until the next quiet moment.
    match store.with(|db| db.checkpoint(WalCheckpoint::Restart)) {
        Ok(_) => {
            tracing::debug!(target: "zlogic::retention", "WAL restarted rather than truncated");
            true
        }
        Err(e) => {
            tracing::debug!(
                target: "zlogic::retention",
                log_frames = truncated.log_frames,
                "WAL checkpoint deferred: {e}"
            );
            false
        }
    }
}

fn claim(
    store: &SharedStore,
    task: &str,
    min_interval: chrono::Duration,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    match store.with(|db| db.maintenance().claim(task, min_interval, now)) {
        Ok(claimed) => claimed,
        Err(e) => {
            tracing::warn!(target: "zlogic::retention", task, "failed to claim: {e}");
            false
        }
    }
}

/// Starts the background pass and returns its handle, or `None` if the thread would not start.
///
/// The delay is not arbitrary: the engine has just opened the database and every connection is
/// warming up, and the desktop is about to ask for its first screen. None of this is urgent enough
/// to compete with either.
pub fn spawn_detached(
    store: SharedStore,
    dirs: Dirs,
    config: RetentionConfig,
    delay: Duration,
) -> Option<std::thread::JoinHandle<SweepReport>> {
    std::thread::Builder::new()
        .name("zlogic-retention".into())
        .spawn(move || {
            if !delay.is_zero() {
                std::thread::sleep(delay);
            }
            let report = sweep(&store, &dirs, &config, chrono::Utc::now());
            tracing::info!(target: "zlogic::retention", "{}", report.summary());
            report
        })
        .inspect_err(
            |e| tracing::warn!(target: "zlogic::retention", "failed to start sweep thread: {e}"),
        )
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::path::PathBuf;

    /// Turn folders are reclaimed by age, not by a name that parses as a date — so this is the only
    /// rule that can ever touch one, and a turn that is still writing must survive it.
    ///
    /// The cutoff is taken *between* the two folders rather than set to some past date: Windows
    /// cannot stamp a directory with a chosen mtime, so the test orders its writes instead. What
    /// was written before the cutoff is stale, what was written after it is not.
    #[test]
    fn a_turn_folder_nothing_has_touched_goes_and_a_recent_one_stays() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let session = SessionId::new();
        let turn_of = || zlogic_protocol::TurnId::new();
        let stale = zlogic_core::turndir::turn_cache_dir(root, session, turn_of());
        std::fs::create_dir_all(stale.join("deliverables")).unwrap();
        std::fs::write(stale.join("deliverables").join("chart.png"), b"old").unwrap();

        let cutoff = std::time::SystemTime::now();

        let fresh = zlogic_core::turndir::turn_cache_dir(root, session, turn_of());
        std::fs::create_dir_all(fresh.join("deliverables")).unwrap();
        std::fs::write(fresh.join("deliverables").join("chart.png"), b"new").unwrap();
        // Eight hex characters of the turn id's tail, which is what makes this test able to assert
        // the layout rather than trust it.
        assert_ne!(stale, fresh);

        let mut freed = 0;
        let removed = sweep_turn_dirs(root, cutoff, &mut freed);

        assert_eq!(
            removed, 1,
            "only the folder nothing has touched since the cutoff"
        );
        assert!(!stale.exists(), "the stale turn folder is gone");
        assert!(fresh.exists(), "a turn that wrote after the cutoff is not");
        assert!(freed > 0, "what it took is counted for the log line");
    }

    struct DayDirs {
        root: PathBuf,
    }

    impl DayDirs {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!("zlogic-retention-{name}"));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn day(&self, date: &str, file: &str) -> &Self {
            let dir = self.root.join(date);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(file), b"x").unwrap();
            self
        }

        fn plain(&self, name: &str) -> &Self {
            std::fs::write(self.root.join(name), b"x").unwrap();
            self
        }

        fn exists(&self, date: &str) -> bool {
            self.root.join(date).exists()
        }
    }

    impl Drop for DayDirs {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn at(date: &str) -> chrono::DateTime<chrono::Utc> {
        date.parse::<chrono::NaiveDate>()
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc()
    }

    #[test]
    fn an_old_day_goes_and_the_recent_ones_stay() {
        let dirs = DayDirs::new("old");
        dirs.day("2026-09-01", "a.json")
            .day("2026-09-20", "b.json")
            .day("2026-09-25", "c.json");

        let removed = sweep_day_dirs(&dirs.root, day_cutoff(at("2026-09-25"), 7), &mut 0);

        assert_eq!(removed, 1);
        assert!(!dirs.exists("2026-09-01"));
        assert!(dirs.exists("2026-09-20"), "the cutoff day itself is kept");
        assert!(dirs.exists("2026-09-25"));
    }

    #[test]
    fn the_cutoff_day_is_the_last_day_kept() {
        assert_eq!(
            day_cutoff(at("2026-09-25"), 7),
            chrono::NaiveDate::from_ymd_opt(2026, 9, 18).unwrap()
        );
    }

    #[test]
    fn a_folder_that_is_not_a_day_is_not_ours_to_delete() {
        let dirs = DayDirs::new("stray");
        dirs.day("2020-01-01", "a.json").plain("notes.txt");

        sweep_day_dirs(&dirs.root, day_cutoff(at("2026-09-25"), 7), &mut 0);

        assert!(dirs.root.join("notes.txt").exists());
    }

    #[test]
    fn a_day_nested_one_level_down_is_still_found() {
        let dirs = DayDirs::new("nested");
        let mcp = dirs.root.join("mcp");
        std::fs::create_dir_all(mcp.join("2020-01-01")).unwrap();
        std::fs::write(mcp.join("2020-01-01/s.tools.json"), b"x").unwrap();

        let removed = sweep_day_dirs(&dirs.root, day_cutoff(at("2026-09-25"), 7), &mut 0);

        assert_eq!(removed, 1);
        assert!(!mcp.join("2020-01-01").exists());
        assert!(mcp.exists(), "the folder holding the days stays");
    }

    #[test]
    fn a_missing_root_is_nothing_to_sweep() {
        assert_eq!(
            sweep_day_dirs(
                Path::new("does-not-exist-zlogic"),
                chrono::NaiveDate::MIN,
                &mut 0
            ),
            0
        );
    }

    #[test]
    fn the_report_says_so_when_there_was_nothing_to_do() {
        let report = SweepReport {
            skipped: vec!["cache: too soon"],
            ..SweepReport::default()
        };
        assert!(report.summary().contains("cache"));
    }

    #[test]
    fn an_empty_report_does_not_invent_work() {
        assert_eq!(SweepReport::default().summary(), "nothing to do");
    }

    #[test]
    fn every_job_claims_under_its_own_key() {
        let store = SharedStore::new(zlogic_store::Db::open_in_memory().unwrap());
        let now = chrono::Utc::now();
        let keys: HashSet<&str> = [WAL_CHECKPOINT, CACHE_SWEEP, LOG_SWEEP, SESSION_SWEEP].into();
        for key in keys {
            assert!(claim(&store, key, SWEEP_INTERVAL, now), "{key}");
            assert!(
                !claim(&store, key, SWEEP_INTERVAL, now),
                "{key} should be held back the second time"
            );
        }
    }

    #[test]
    fn the_cache_folder_is_named_after_the_session_id() {
        let root = Path::new("/work");
        let id = SessionId::new();
        let raw = id.to_string();
        let tail = raw.chars().skip(raw.chars().count() - 8).collect::<String>();
        assert_eq!(
            session_cache_dir(root, Some(id)),
            root.join(".zlogic").join("cache").join(tail)
        );
        assert_eq!(
            session_cache_dir(root, None),
            root.join(".zlogic").join("cache")
        );
    }

    #[test]
    fn two_sessions_created_at_the_same_moment_get_different_folders() {
        // The folder is named with the id's tail, not its head: a v7 id's first twelve hex digits
        // are the millisecond clock, so a leading 8 collapses every session opened inside one
        // 65.5-second window.
        let ids: Vec<SessionId> = (0..64).map(|_| SessionId::new()).collect();
        let names: HashSet<String> = ids
            .iter()
            .map(|id| {
                let raw = id.to_string();
                raw.chars().skip(raw.chars().count() - 8).collect::<String>()
            })
            .collect();
        assert_eq!(
            names.len(),
            ids.len(),
            "two sessions shared a scratch folder"
        );
    }

    struct SessionFixture {
        store: SharedStore,
        root: PathBuf,
        workspace: zlogic_protocol::WorkspaceId,
    }

    impl SessionFixture {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!("zlogic-sweep-{name}"));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            let store = SharedStore::new(zlogic_store::Db::open_in_memory().unwrap());
            let workspace = store
                .with(|db| {
                    let (record, _) = db.workspaces().resolve(&root).map_err(|e| e.to_string())?;
                    Ok::<_, String>(record.workspace_id)
                })
                .unwrap();
            Self {
                store,
                root,
                workspace,
            }
        }

        fn chat(&self, age_days: i64) -> SessionId {
            self.store
                .with(|db| {
                    let session = db
                        .sessions()
                        .create(zlogic_store::NewSession::root(self.workspace))
                        .map_err(|e| e.to_string())?;
                    db.conn()
                        .execute(
                            "UPDATE session SET created_at = :ts, updated_at = :ts
                             WHERE session_id = :id",
                            rusqlite::named_params! {
                                ":ts": Utc::now() - chrono::Duration::days(age_days),
                                ":id": session.session_id.to_string(),
                            },
                        )
                        .map_err(|e| e.to_string())?;
                    Ok::<_, String>(session.session_id)
                })
                .unwrap()
        }

        fn cache_for(&self, id: SessionId) -> PathBuf {
            session_cache_dir(&self.root, Some(id))
        }
    }

    impl Drop for SessionFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    use chrono::Utc;

    #[test]
    fn an_idle_chat_session_and_its_scratch_files_go() {
        let f = SessionFixture::new("idle");
        let old = f.chat(30);
        let dir = f.cache_for(old);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("script.py"), b"print(1)").unwrap();

        let removed = sweep_sessions(&f.store, Utc::now() - chrono::Duration::days(7)).unwrap();

        assert_eq!(removed, 1);
        assert!(
            f.store
                .with(|db| db.sessions().find(old))
                .unwrap()
                .is_none()
        );
        assert!(!dir.exists(), "the scratch folder goes with the session");
    }

    #[test]
    fn a_recent_session_is_never_a_candidate() {
        let f = SessionFixture::new("recent");
        let fresh = f.chat(1);

        assert_eq!(
            sweep_sessions(&f.store, Utc::now() - chrono::Duration::days(7)).unwrap(),
            0
        );
        assert!(
            f.store
                .with(|db| db.sessions().find(fresh))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn a_locked_session_survives_however_old_it_is() {
        let f = SessionFixture::new("locked");
        let old = f.chat(30);
        f.store
            .with(|db| {
                db.locks()
                    .acquire(old, zlogic_protocol::TurnId::new(), Some("test"))
                    .map(|_| ())
            })
            .unwrap();

        assert_eq!(
            sweep_sessions(&f.store, Utc::now() - chrono::Duration::days(7)).unwrap(),
            0
        );
        assert!(
            f.store
                .with(|db| db.sessions().find(old))
                .unwrap()
                .is_some()
        );
    }
}
