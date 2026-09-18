//! Times the usage report — `aggregate` (all dimensions) and `aggregate_tools` — against a real
//! `state.db`, which is the only way to see what the report costs at a realistic table size.
//!
//! ```text
//! cp "$HOME/.local/share/zlogic/state.db"* /tmp/          # a copy: `Db::open` migrates and repairs
//! cargo run -p zlogic-store --example report_bench -- /tmp/state.db
//! ```
//!
//! Never point it at the live database: it opens with write access, and the open itself applies
//! migrations.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use chrono::{DateTime, FixedOffset};
use zlogic_store::{Db, UsageQuery};

/// The filter the report uses, repeated here so the alternative below selects the same rows.
const FILTER: &str = "
    (:since IS NULL OR u.created_at >= :since)
    AND (:until IS NULL OR u.created_at <= :until)
    AND (:workspace IS NULL OR s.workspace_id = :workspace)
    AND (:session IS NULL
         OR (:self_only = 1 AND u.session_id = :session)
         OR (:self_only = 0 AND s.root_session_id = :session))
    AND (:turn IS NULL OR u.turn_id = :turn)
    AND (:session_kind IS NULL OR s.kind = :session_kind)";

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: report_bench <state.db>");
    let opened = Instant::now();
    let db = Db::open(std::path::Path::new(&path)).expect("open");
    println!("open: {} ms", opened.elapsed().as_millis());

    let query = UsageQuery {
        utc_offset_minutes: 480,
        ..Default::default()
    };

    for round in 1..=3 {
        let started = Instant::now();
        let aggregate = db.usage().aggregate(&query).expect("aggregate");
        let aggregate_ms = started.elapsed().as_millis();

        let started = Instant::now();
        let tools = db.usage().aggregate_tools(&query).expect("aggregate_tools");
        let tools_ms = started.elapsed().as_millis();

        let parts = aggregate.total.len()
            + aggregate.by_model.len()
            + aggregate.by_provider.len()
            + aggregate.by_day.len()
            + aggregate.by_session.len()
            + aggregate.by_workspace.len()
            + aggregate.by_aux_purpose.len()
            + aggregate.by_cost_source.len();

        println!(
            "round {round}: aggregate {aggregate_ms} ms ({parts} parts, {} envelopes), \
             aggregate_tools {tools_ms} ms ({} tools)",
            aggregate.turn_envelopes.len(),
            tools.len()
        );

        println!(
            "  parts: total={} model={} provider={} day={} session={} workspace={} aux={} source={} latest_turn={}",
            aggregate.total.len(),
            aggregate.by_model.len(),
            aggregate.by_provider.len(),
            aggregate.by_day.len(),
            aggregate.by_session.len(),
            aggregate.by_workspace.len(),
            aggregate.by_aux_purpose.len(),
            aggregate.by_cost_source.len(),
            aggregate.latest_turn.len(),
        );

        // Totals for a cross-check against the pre-refactor SQL on the same database.
        let sum_u32 = |f: fn(&zlogic_store::UsageAggregatePart) -> u32| -> u32 {
            aggregate.total.iter().map(f).sum()
        };
        let sum_u64 = |f: fn(&zlogic_store::UsageAggregatePart) -> u64| -> u64 {
            aggregate.total.iter().map(f).sum()
        };
        println!(
            "  total calls={} sessions={} turns={} input={} output={} cost={:.6}",
            sum_u32(|part| part.calls),
            sum_u32(|part| part.sessions),
            sum_u32(|part| part.turns),
            sum_u64(|part| part.tokens.input),
            sum_u64(|part| part.tokens.output),
            aggregate
                .total
                .iter()
                .filter_map(|part| part.cost)
                .sum::<f64>(),
        );

        let (fetch_ms, fold_ms, rows) = if round == 1 {
            let started = Instant::now();
            let rows = raw_rows(&db, &query);
            let fetch_ms = started.elapsed().as_millis();
            let count = rows.len();
            let started = Instant::now();
            let totals = fold(&rows, query.utc_offset_minutes);
            let fold_ms = started.elapsed().as_millis();
            println!(
                "  pull-into-memory: fetch {fetch_ms} ms ({count} rows), fold in Rust {fold_ms} ms \
                 → calls={} sessions={} turns={} input={} output={} cost={:.6}",
                totals.calls,
                totals.sessions.len(),
                totals.turns.len(),
                totals.input,
                totals.output,
                totals.cost,
            );
            (fetch_ms, fold_ms, count)
        } else {
            (0, 0, 0)
        };
        let _ = (fetch_ms, fold_ms, rows);
    }

    // The report lane against the real file: this is where a read-only connection on a WAL database
    // either works or wastes the whole design.
    let store = zlogic_store::SharedStore::new(db);
    for round in 1..=2 {
        let started = Instant::now();
        let parts = store
            .with_report_named("usage.summary", |db| {
                db.usage()
                    .aggregate(&query)
                    .map(|aggregate| aggregate.total.len())
            })
            .expect("report");
        println!(
            "report lane round {round}: {} ms ({parts} total parts, read-only connection)",
            started.elapsed().as_millis()
        );
    }
    let refused = store.with_report_named("test.write", |db| {
        db.conn().execute("DELETE FROM usage_event WHERE 0", [])
    });
    println!("report lane write refused: {}", refused.is_err());
}

/// One selected `usage_event` row, read the way an in-memory grouping would need it.
/// The timing columns are read but not folded below: keeping them in the fetch makes the measured
/// fetch realistic, while the fold that uses them would only be slower than the one timed here.
#[allow(dead_code)]
struct RawRow {
    session_id: String,
    turn_id: Option<String>,
    purpose: String,
    model_ref: Option<String>,
    cost_source: Option<String>,
    currency: Option<String>,
    created_at: String,
    workspace_id: Option<String>,
    session_title: Option<String>,
    workspace_name: Option<String>,
    input: u64,
    output: u64,
    cost: Option<f64>,
    request_started_at: Option<String>,
    first_token_at: Option<String>,
    completed_at: Option<String>,
}

fn raw_rows(db: &Db, q: &UsageQuery) -> Vec<RawRow> {
    let sql = format!(
        "SELECT u.session_id, u.turn_id, u.purpose, u.model_ref, u.cost_source, u.currency,
                u.created_at, s.workspace_id, NULLIF(TRIM(s.title), ''), w.name,
                u.input_tokens, u.output_tokens, u.cost,
                u.request_started_at, u.first_token_at, u.completed_at
         FROM usage_event u
         LEFT JOIN session s ON s.session_id = u.session_id
         LEFT JOIN workspaces w ON w.workspace_id = s.workspace_id
         WHERE {FILTER}"
    );
    let mut statement = db.conn().prepare(&sql).expect("prepare");
    let rows = statement
        .query_map(
            rusqlite::named_params! {
                ":since": q.since,
                ":until": q.until,
                ":workspace": q.workspace_id,
                ":session": q.session_id,
                ":self_only": q.self_only,
                ":turn": q.turn_id,
                ":session_kind": q.session_kind,
            },
            |row| {
                Ok(RawRow {
                    session_id: row.get(0)?,
                    turn_id: row.get(1)?,
                    purpose: row.get(2)?,
                    model_ref: row.get(3)?,
                    cost_source: row.get(4)?,
                    currency: row.get(5)?,
                    created_at: row.get(6)?,
                    workspace_id: row.get(7)?,
                    session_title: row.get(8)?,
                    workspace_name: row.get(9)?,
                    input: row.get::<_, i64>(10)? as u64,
                    output: row.get::<_, i64>(11)? as u64,
                    cost: row.get(12)?,
                    request_started_at: row.get(13)?,
                    first_token_at: row.get(14)?,
                    completed_at: row.get(15)?,
                })
            },
        )
        .expect("query");
    rows.collect::<rusqlite::Result<Vec<_>>>().expect("rows")
}

#[derive(Default)]
struct Totals {
    calls: u32,
    input: u64,
    output: u64,
    cost: f64,
    sessions: HashSet<String>,
    turns: HashSet<String>,
}

/// Groups every dimension in memory, the way an in-memory design would: one pass per dimension over
/// the full row set, keyed by the same expressions the SQL version groups by.
fn fold(rows: &[RawRow], utc_offset_minutes: i32) -> Totals {
    let offset = FixedOffset::east_opt(utc_offset_minutes * 60).expect("offset");
    let local_day = |text: &str| -> Option<String> {
        DateTime::parse_from_rfc3339(text)
            .ok()
            .map(|at| at.with_timezone(&offset).format("%Y-%m-%d").to_string())
    };
    let provider = |model_ref: &str| -> String {
        model_ref
            .split_once(':')
            .map_or(model_ref, |(provider, _)| provider)
            .to_owned()
    };

    // Each dimension is keyed differently; the accumulator shape is the same.
    type Key =
        fn(&RawRow, &dyn Fn(&str) -> Option<String>, &dyn Fn(&str) -> String) -> Option<String>;
    let dimensions: [Key; 8] = [
        |_, _, _| Some(String::new()),
        |row, _, _| row.model_ref.clone(),
        |row, _, provider| row.model_ref.as_deref().map(provider),
        |row, day, _| day(&row.created_at),
        |row, _, _| Some(row.session_id.clone()),
        |row, _, _| row.workspace_id.clone(),
        |row, _, _| {
            (row.purpose != "main" && !row.purpose.starts_with("agent:"))
                .then(|| row.purpose.clone())
        },
        |row, _, _| {
            Some(
                row.cost_source
                    .clone()
                    .unwrap_or_else(|| "unknown".to_owned()),
            )
        },
    ];

    let mut total = Totals::default();
    for key_of in dimensions {
        let mut groups: HashMap<String, Totals> = HashMap::new();
        for row in rows {
            let Some(key) = key_of(row, &local_day, &provider) else {
                continue;
            };
            let group = groups.entry(key).or_default();
            group.calls += 1;
            group.input += row.input;
            group.output += row.output;
            group.cost += row.cost.unwrap_or(0.0);
            group.sessions.insert(row.session_id.clone());
            if let Some(turn_id) = &row.turn_id {
                group.turns.insert(turn_id.clone());
            }
        }
        if total.calls == 0 {
            // The total dimension is the first one; keep its numbers for the caller's cross-check.
            for group in groups.into_values() {
                total.calls += group.calls;
                total.input += group.input;
                total.output += group.output;
                total.cost += group.cost;
                total.sessions.extend(group.sessions);
                total.turns.extend(group.turns);
            }
        }
    }
    total
}
