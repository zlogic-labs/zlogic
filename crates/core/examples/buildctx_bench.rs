//! ```text
//! cargo run -p zlogic-core --example buildctx_bench --release -- <state.db> <objects-root> [session_id...]
//! ```

use std::cell::Cell;
use std::time::Instant;

use rusqlite::{Connection, OpenFlags, named_params};

use serde_json::Value;
use zlogic_core::compact;
use zlogic_core::context::{self, ContextObject};
use zlogic_core::{CoreError, Result as CoreResult};
use zlogic_objects::fs::FileObjectStore;
use zlogic_objects::{ObjectId, ObjectStore};
use zlogic_protocol::SessionId;
use zlogic_protocol::message::{ContentPart, Source};
use zlogic_store::{EntryKind, EntryRecord, EntryStore};

fn main() -> CoreResult<()> {
    let mut args = std::env::args().skip(1);
    let db_path = args
        .next()
        .expect("usage: buildctx_bench <state.db> <objects-root> [session_id...]");
    let objects_root = args
        .next()
        .expect("usage: buildctx_bench <state.db> <objects-root> [session_id...]");
    let want: Vec<String> = args.collect();

    let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| CoreError::Invalid(format!("opening {db_path}: {e}")))?;
    let objects = FileObjectStore::open(&objects_root)
        .map_err(|e| CoreError::Invalid(format!("opening object store {objects_root}: {e}")))?;

    println!("state.db       = {db_path}");
    println!("objects root   = {objects_root}");
    println!("journal_mode   = {}", journal_mode(&conn));
    println!();

    let picks = if want.is_empty() {
        top_sessions(&conn, 6)?
    } else {
        want.iter()
            .map(|s| (s.clone(), String::new(), 0))
            .collect::<Vec<_>>()
    };

    for (sid, title, n) in &picks {
        if let Err(e) = bench_session(&conn, &objects, sid, title, *n) {
            eprintln!("session {sid} failed: {e}");
        }
        println!();
    }
    Ok(())
}

fn top_sessions(conn: &Connection, limit: usize) -> CoreResult<Vec<(String, String, i64)>> {
    let mut st = conn
        .prepare(
            "SELECT e.session_id, COALESCE(s.title, ''), COUNT(*) AS n
             FROM session_entry e
             LEFT JOIN session s ON s.session_id = e.session_id
             GROUP BY e.session_id
             ORDER BY n DESC
             LIMIT :limit",
        )
        .map_err(|e| CoreError::Corrupt(e.to_string()))?;
    let rows = st
        .query_map(named_params! { ":limit": limit as i64 }, |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })
        .map_err(|e| CoreError::Corrupt(e.to_string()))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| CoreError::Corrupt(e.to_string()))?);
    }
    Ok(out)
}

fn journal_mode(conn: &Connection) -> String {
    conn.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))
        .unwrap_or_else(|_| "?".into())
}

fn bench_session(
    conn: &Connection,
    objects: &FileObjectStore,
    sid: &str,
    title: &str,
    known_entries: i64,
) -> CoreResult<()> {
    let session: SessionId = sid
        .parse()
        .map_err(|e| CoreError::Invalid(format!("session id {sid}: {e}")))?;
    let entries = EntryStore::new(conn);

    let mut best = u128::MAX;
    let mut stage_lines: Vec<String> = Vec::new();
    let mut msg_count = 0usize;
    let mut msg_chars = 0usize;
    let mut total_entries = 0usize;
    let mut total_bytes = 0usize;
    let mut total_obj_bytes = 0u64;

    for run in 0..3 {
        let bytes = Cell::new(0usize);
        let obj_bytes = Cell::new(0u64);

        let t0 = Instant::now();
        let compact_entries = entries.list_kind(session, EntryKind::Compaction)?;
        let t1 = Instant::now();

        let loader = |rec: &EntryRecord| -> CoreResult<Value> {
            let v = entries.load_data(rec, objects)?;
            bytes.set(bytes.get() + serde_json::to_vec(&v)?.len());
            Ok(v)
        };
        let summaries = compact::effective_summaries(&compact_entries, &loader)?;
        let t2 = Instant::now();

        let list = match compact::covered_prefix_end(&summaries) {
            Some(end) => entries.list_for_context_after(session, end)?,
            None => entries.list_for_context(session)?,
        };
        let t3 = Instant::now();

        let obj_loader = |raw: &str| -> CoreResult<ContextObject> {
            let id: ObjectId = raw
                .parse()
                .map_err(|e| CoreError::Corrupt(format!("object id {raw}: {e}")))?;
            let size = objects.size(&id)?;
            obj_bytes.set(obj_bytes.get() + size);
            let reader = objects.open(&id)?;
            Ok(ContextObject {
                reader,
                bytes: size,
            })
        };
        let target = Source::new("bench", "bench");
        let prepared = context::build_context(&list, &target, &loader, &obj_loader)?;
        let t4 = Instant::now();

        let total = t4.duration_since(t0).as_micros();
        best = best.min(total);
        if run == 0 {
            stage_lines = vec![
                format!(
                    "    list_kind(compaction)      {:>8} µs",
                    t1.duration_since(t0).as_micros()
                ),
                format!(
                    "    effective_summaries       {:>8} µs",
                    t2.duration_since(t1).as_micros()
                ),
                format!(
                    "    list_for_context(_after)  {:>8} µs",
                    t3.duration_since(t2).as_micros()
                ),
                format!(
                    "    build_context (pure + object reads)  {:>8} µs",
                    t4.duration_since(t3).as_micros()
                ),
            ];
            msg_count = prepared.messages.len();
            msg_chars = prepared
                .messages
                .iter()
                .flat_map(|m| &m.content)
                .filter_map(|p| match p {
                    ContentPart::Text(t) => Some(t.text.chars().count()),
                    _ => None,
                })
                .sum();
            total_entries = list.len();
            total_bytes = bytes.get();
            total_obj_bytes = obj_bytes.get();
        }
        println!(
            "  run {run}: {total:>8} µs  (messages={}, entries={})",
            prepared.messages.len(),
            list.len()
        );
    }

    println!("── session {sid}  {title}");
    println!(
        "    entries={total_entries} (known {known_entries}) payload={}B object-reads={}B messages={msg_count} chars={msg_chars}",
        total_bytes, total_obj_bytes
    );
    for l in &stage_lines {
        println!("{l}");
    }
    println!(
        "    best (warm cache) {best} µs = {:.2} ms",
        best as f64 / 1000.0
    );
    Ok(())
}
