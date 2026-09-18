use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, OptionalExtension, named_params};

use crate::Result;

pub const OBJECT_GC: &str = "object_gc";

pub const IS_FINAL_BACKFILL: &str = "is_final_backfill";

pub struct MaintenanceStore<'a> {
    conn: &'a Connection,
}

impl<'a> MaintenanceStore<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub fn last_run(&self, task: &str) -> Result<Option<DateTime<Utc>>> {
        let last: Option<String> = self
            .conn
            .query_row(
                "SELECT last_run_at FROM maintenance WHERE task = :task",
                named_params! { ":task": task },
                |r| r.get(0),
            )
            .optional()?;
        Ok(last
            .and_then(|t| DateTime::parse_from_rfc3339(&t).ok())
            .map(|t| t.with_timezone(&Utc)))
    }

    pub fn claim(&self, task: &str, min_interval: Duration, now: DateTime<Utc>) -> Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute_batch("ROLLBACK; BEGIN IMMEDIATE;")?;

        let last: Option<String> = tx
            .query_row(
                "SELECT last_run_at FROM maintenance WHERE task = :task",
                named_params! { ":task": task },
                |r| r.get(0),
            )
            .optional()?;

        if let Some(last) = last
            && let Ok(last) = DateTime::parse_from_rfc3339(&last)
            && now.signed_duration_since(last.with_timezone(&Utc)) < min_interval
        {
            tx.commit()?;
            return Ok(false);
        }

        tx.execute(
            "INSERT INTO maintenance (task, last_run_at) VALUES (:task, :now)
             ON CONFLICT(task) DO UPDATE SET last_run_at = :now",
            named_params! { ":task": task, ":now": now.to_rfc3339() },
        )?;
        tx.commit()?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;

    fn db() -> Db {
        Db::open_in_memory().unwrap()
    }

    #[test]
    fn the_first_claim_succeeds() {
        let db = db();
        assert!(
            db.maintenance()
                .claim(OBJECT_GC, Duration::hours(24), Utc::now())
                .unwrap()
        );
    }

    #[test]
    fn a_second_claim_within_the_interval_is_refused() {
        let db = db();
        let now = Utc::now();
        assert!(
            db.maintenance()
                .claim(OBJECT_GC, Duration::hours(24), now)
                .unwrap()
        );
        assert!(
            !db.maintenance()
                .claim(OBJECT_GC, Duration::hours(24), now)
                .unwrap()
        );
        assert!(
            !db.maintenance()
                .claim(OBJECT_GC, Duration::hours(24), now + Duration::minutes(1))
                .unwrap()
        );
    }

    #[test]
    fn a_claim_after_the_interval_succeeds() {
        let db = db();
        let now = Utc::now();
        assert!(
            db.maintenance()
                .claim(OBJECT_GC, Duration::hours(24), now)
                .unwrap()
        );
        assert!(
            db.maintenance()
                .claim(OBJECT_GC, Duration::hours(24), now + Duration::hours(25))
                .unwrap()
        );
    }

    #[test]
    fn tasks_are_independent() {
        let db = db();
        let now = Utc::now();
        assert!(
            db.maintenance()
                .claim(OBJECT_GC, Duration::hours(24), now)
                .unwrap()
        );
        assert!(
            db.maintenance()
                .claim("something_else", Duration::hours(24), now)
                .unwrap()
        );
    }

    #[test]
    fn an_unparseable_timestamp_is_treated_as_long_ago() {
        let db = db();
        db.conn()
            .execute(
                "INSERT INTO maintenance (task, last_run_at) VALUES (?1, 'not a timestamp')",
                [OBJECT_GC],
            )
            .unwrap();
        assert!(
            db.maintenance()
                .claim(OBJECT_GC, Duration::hours(24), Utc::now())
                .unwrap()
        );
    }

    #[test]
    fn a_zero_interval_always_claims() {
        let db = db();
        let now = Utc::now();
        assert!(
            db.maintenance()
                .claim(OBJECT_GC, Duration::zero(), now)
                .unwrap()
        );
        assert!(
            db.maintenance()
                .claim(OBJECT_GC, Duration::zero(), now)
                .unwrap()
        );
    }
}
