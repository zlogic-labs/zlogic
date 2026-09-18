//! The database schema.
//! Kept in its own module so the DDL can be read end to end. When the tables were inlined in
//! `lib.rs` the shape of the database was interleaved with connection setup, and answering
//! "what columns does an entry have" meant scrolling past pragmas.
//! # Adding a migration
//! Append a `&str` to [`MIGRATIONS`] and bump nothing else — the index in that slice *is* the
//! version. [`crate::Db::migrate`] applies every statement past the recorded `user_version`, so
//! an existing database moves forward one step at a time.
//! Each migration must be idempotent within itself (`IF NOT EXISTS`) so a half-applied batch can
//! be re-run, and must never rewrite an earlier one: editing history in place would leave two
//! databases with the same version and different shapes.

/// Every migration, in order. The index is the schema version.
pub const MIGRATIONS: &[&str] = &[
    V1, V2, V3, V4, V5, V6, V7, V8, V9, V10, V11, V12, V13, V14, V15, V16, V17, V18, V19, V20, V21,
];

/// The current version — what a freshly created database reports.
pub const CURRENT_VERSION: i64 = MIGRATIONS.len() as i64;

const V1: &str = r#"
CREATE TABLE IF NOT EXISTS session (
  id                INTEGER PRIMARY KEY,
  session_id        TEXT NOT NULL UNIQUE,
  -- Which workspace this belongs to. A pointer, never a path, so it cannot go stale when the
  -- workspace is later bound to a different directory.
  workspace_id      TEXT NOT NULL,
  kind              TEXT NOT NULL DEFAULT 'chat' CHECK (kind IN ('chat', 'task')),
  -- Where tools actually run, **only when it deviates from the workspace root**.
  --
  -- NULL means "wherever the workspace points now", which is the normal case. The root itself is
  -- deliberately not stored: duplicating it here would mean migrating every session row whenever
  -- the workspace moves, and a row that was missed would silently run in the old directory.
  --
  -- A value appears when the agent enters a worktree — a genuine, persistent deviation that has
  -- to survive a restart.
  exec_cwd          TEXT,
  -- This session's own agent profile name ('main' for a root session).
  agent             TEXT NOT NULL DEFAULT 'main',
  -- Materialised path of agent names from the root, '/'-joined: 'main/researcher'. Lets a whole
  -- sub-tree be selected with a LIKE prefix, and gives depth without a recursive query.
  agent_paths       TEXT NOT NULL DEFAULT 'main',
  parent_session_id TEXT,
  -- Shared by an entire sub-agent tree; a root session points at itself, so "the whole tree" is
  -- one flat query rather than a recursive CTE.
  root_session_id   TEXT NOT NULL,
  title             TEXT,
  -- 'draft' may be replaced by a higher layer; 'model' and 'user' are protected.
  title_source      TEXT,
  model_ref         TEXT CHECK (
    model_ref IS NULL OR (
      instr(model_ref, ':') > 1
      AND length(substr(model_ref, instr(model_ref, ':') + 1)) > 0
    )
  ),
  created_at        TEXT NOT NULL,
  updated_at        TEXT NOT NULL,
  archived_at       TEXT
);
CREATE INDEX IF NOT EXISTS idx_session_workspace ON session(workspace_id, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_session_parent ON session(parent_session_id);
CREATE INDEX IF NOT EXISTS idx_session_root ON session(root_session_id);
CREATE INDEX IF NOT EXISTS idx_session_agent_paths ON session(agent_paths);

-- The live-turn lock. A separate table rather than columns on `session`, because it has its own
-- lifecycle (acquire, heartbeat, release, steal) and because every session read would otherwise
-- drag lock bookkeeping along. "Is a turn running?" is a lookup here.
CREATE TABLE IF NOT EXISTS session_locks (
  id           INTEGER PRIMARY KEY,
  session_id   TEXT NOT NULL UNIQUE REFERENCES session(session_id) ON DELETE CASCADE,
  turn_id      TEXT NOT NULL,
  -- Fresh per acquisition. Guards against pid reuse: a revived process must not be able to pass
  -- itself off as the still-live holder.
  holder_id    TEXT NOT NULL,
  -- 'cli' | 'desktop' | … so the UI can say who is holding it.
  holder_kind  TEXT,
  pid          INTEGER NOT NULL,
  acquired_at  TEXT NOT NULL,
  -- Staleness is judged by the heartbeat stopping, never by how long the lock has been held: a
  -- turn that legitimately runs for days heartbeats for days.
  heartbeat_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_locks_heartbeat ON session_locks(heartbeat_at);

CREATE TABLE IF NOT EXISTS session_entry (
  id           INTEGER PRIMARY KEY,
  entry_id     TEXT NOT NULL UNIQUE,
  session_id   TEXT NOT NULL REFERENCES session(session_id) ON DELETE CASCADE,
  -- Global ordering, and how round boundaries are derived.
  seq          INTEGER NOT NULL,
  -- Ordering / rewind / fork / compaction boundaries. Many entries share one turn_seq, so this
  -- must NOT be unique.
  turn_seq     INTEGER NOT NULL,
  turn_id      TEXT NOT NULL,
  round_id     TEXT,
  kind         TEXT NOT NULL,
  -- Normalised fields: display text, tool name and args, an interaction body. Object ids appear
  -- here too, in whatever structure the kind defines — a tool result knows which of its objects
  -- is stdout and which is a diff. Enumerating them is `entry_object`'s job, not this column's.
  data         TEXT NOT NULL,
  -- Provider-native replayable payload, byte for byte.
  native       TEXT,
  -- {provider_id, model_id}: the raw-replay gate's criterion.
  source       TEXT,
  created_at   TEXT NOT NULL,
  UNIQUE(session_id, seq)
);
CREATE INDEX IF NOT EXISTS idx_entry_turn ON session_entry(session_id, turn_seq, seq);
CREATE INDEX IF NOT EXISTS idx_entry_kind ON session_entry(session_id, kind, seq);

-- Which objects an entry references.
--
-- A join table rather than a column, for two reasons:
--
-- 1. One entry can reference several objects — three image attachments, a tool result with both
--    a large stdout and a captured diff. A single column cannot say that.
-- 2. Garbage collection has to enumerate live references cheaply. If the ids lived only inside
--    `data`, and `data` itself can be offloaded to an object, then finding an entry's references
--    would require fetching that object first — GC would have to read every offloaded payload to
--    learn what it points at. This table breaks that circularity: one index scan, no JSON.
--
-- The index on object_id answers the reverse question, which is the one GC actually asks:
-- "does anything still reference this object?"
CREATE TABLE IF NOT EXISTS entry_object (
  id        INTEGER PRIMARY KEY,
  entry_id  TEXT NOT NULL REFERENCES session_entry(entry_id) ON DELETE CASCADE,
  object_id TEXT NOT NULL,
  -- 'payload' (this entry's own data was offloaded; at most one per entry) | 'attachment' |
  -- 'output' | 'diff' | 'skill'
  role      TEXT NOT NULL,
  -- Ties the row back to a specific place in `data` — an attachment index, a file path.
  ref_key   TEXT,
  UNIQUE(entry_id, role, object_id, ref_key)
);
CREATE INDEX IF NOT EXISTS idx_entry_object_entry ON entry_object(entry_id);
CREATE INDEX IF NOT EXISTS idx_entry_object_object ON entry_object(object_id);

CREATE TABLE IF NOT EXISTS usage_event (
  id            INTEGER PRIMARY KEY,
  usage_id      TEXT NOT NULL UNIQUE,
  session_id    TEXT NOT NULL,
  turn_id       TEXT,
  round_id      TEXT,
  -- Required. Compaction only looks at 'main', which excludes sub-agents, title refinement and
  -- approval calls without maintaining a list of what counts as the main conversation.
  purpose       TEXT NOT NULL,
  model_ref     TEXT CHECK (
    model_ref IS NULL OR (
      instr(model_ref, ':') > 1
      AND length(substr(model_ref, instr(model_ref, ':') + 1)) > 0
    )
  ),
  input_tokens  INTEGER NOT NULL DEFAULT 0,
  output_tokens INTEGER NOT NULL DEFAULT 0,
  cache_read    INTEGER,
  cache_write   INTEGER,
  reasoning     INTEGER,
  cost          REAL,
  currency      TEXT,
  cost_source   TEXT,
  -- Display-only and diagnostic material (the raw usage blob). No columns for these.
  metadata      TEXT,
  created_at    TEXT NOT NULL
);
-- Deliberately no session foreign key: usage must outlive the session it came from, or
-- "how much did this month cost" shrinks whenever someone tidies up their session list.
CREATE INDEX IF NOT EXISTS idx_usage_session ON usage_event(session_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_usage_main ON usage_event(session_id, purpose, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_usage_turn ON usage_event(turn_id);

-- Undelivered user input only. Delivery moves the row into session_entry and deletes it here,
-- inside one transaction — hence no `delivered` column.
CREATE TABLE IF NOT EXISTS mailbox (
  id                INTEGER PRIMARY KEY,
  submission_id     TEXT NOT NULL UNIQUE,
  session_id        TEXT NOT NULL REFERENCES session(session_id) ON DELETE CASCADE,
  -- Client-side idempotency key.
  client_request_id TEXT NOT NULL,
  parts             TEXT NOT NULL,
  -- The provider-qualified model selected when this input was submitted. It stays attached while
  -- queued so changing the session model cannot silently reroute already-written input.
  model_ref         TEXT CHECK (
    model_ref IS NULL OR (
      instr(model_ref, ':') > 1
      AND length(substr(model_ref, instr(model_ref, ':') + 1)) > 0
    )
  ),
  delivery          TEXT NOT NULL,
  created_at        TEXT NOT NULL,
  UNIQUE(session_id, client_request_id)
);
CREATE INDEX IF NOT EXISTS idx_mailbox_session ON mailbox(session_id, id);
"#;

const V2: &str = "";

const V3: &str = r#"
CREATE TABLE IF NOT EXISTS workspaces (
  id           INTEGER PRIMARY KEY,
  workspace_id TEXT NOT NULL UNIQUE,
  -- Display name. Defaults to the last segment of the path; the user can change it, and renaming
  -- does not affect identity.
  name         TEXT NOT NULL,
  -- The normalised (realpath) directory. UNIQUE makes "one directory, one workspace" a guarantee of
  -- the database rather than something every call site checks for itself first -- that kind of
  -- check always leaks under concurrency.
  path         TEXT NOT NULL UNIQUE,
  created_at   TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_workspaces_path ON workspaces(path);
"#;

const V4: &str = r#"
ALTER TABLE session_entry ADD COLUMN display TEXT;
"#;

const V5: &str = r#"
ALTER TABLE workspaces ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0;
ALTER TABLE workspaces ADD COLUMN sort_order INTEGER NOT NULL DEFAULT 0;
ALTER TABLE workspaces ADD COLUMN last_opened_at TEXT;
-- "Removed from the sidebar" = set to 1. **Not a delete** -- the row stays, the id stays, the
-- session history stays, and when the user opens the same directory again they get back the same id
-- and all of its history.
ALTER TABLE workspaces ADD COLUMN hidden INTEGER NOT NULL DEFAULT 0;
"#;

const V6: &str = r#"
-- When the background maintenance task last ran.
--
-- It exists for exactly one reason: **cross-process deduplication**. `state.db` and the object store
-- are shared by several processes (CLI, desktop, each session) that start at arbitrary times; without
-- this table every process start would scan the whole object store, and a user opening the app
-- twenty times a day would mean twenty full scans.
--
-- Deciding "long enough" and writing must happen in **one and the same transaction** (see
-- MaintenanceStore::claim), or two processes starting at once both read "it has been ages" and both
-- start scanning -- which is exactly what this table exists to prevent.
CREATE TABLE IF NOT EXISTS maintenance (
  task        TEXT PRIMARY KEY,
  -- RFC3339. The time it last **started**, not finished -- a task that crashed halfway must not make
  -- the next run retry immediately, which would turn a consistently failing task into a startup loop.
  last_run_at TEXT NOT NULL
) STRICT;
"#;

const V7: &str = r#"
ALTER TABLE workspaces ADD COLUMN tools TEXT;
"#;

const V8: &str = r#"
CREATE TABLE IF NOT EXISTS memory (
  id                INTEGER PRIMARY KEY,
  memory_id         TEXT NOT NULL UNIQUE,
  scope             TEXT NOT NULL CHECK(scope IN ('global', 'workspace')),
  workspace_id      TEXT,
  category          TEXT NOT NULL CHECK(category IN ('preference', 'correction', 'goal', 'reference')),
  fact              TEXT NOT NULL,
  source_quote      TEXT NOT NULL,
  source_session_id TEXT,
  source_turn_id    TEXT,
  status            TEXT NOT NULL DEFAULT 'active' CHECK(status IN ('active', 'removed')),
  created_at        TEXT NOT NULL,
  updated_at        TEXT NOT NULL,
  CHECK(
    (scope = 'global' AND workspace_id IS NULL) OR
    (scope = 'workspace' AND workspace_id IS NOT NULL)
  )
);
CREATE INDEX IF NOT EXISTS idx_memory_scope
  ON memory(scope, workspace_id, status, updated_at DESC);

CREATE TABLE IF NOT EXISTS memory_events (
  id                INTEGER PRIMARY KEY,
  event_id          TEXT NOT NULL UNIQUE,
  memory_id         TEXT NOT NULL REFERENCES memory(memory_id),
  action            TEXT NOT NULL CHECK(action IN ('add', 'update', 'remove', 'undo')),
  before_json       TEXT,
  after_json        TEXT,
  source_session_id TEXT,
  source_turn_id    TEXT,
  reverts_event_id  TEXT,
  created_at        TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_memory_events_memory
  ON memory_events(memory_id, id);
"#;

/// The extension tables are no longer created. A database that already built them keeps them:
/// this slot stays empty instead of dropping anything, because migrations are positional and the
/// `MIGRATIONS` array must not change length. Nothing reads those tables any more.
const V9: &str = "";

/// External resources are configuration, not extensions. Secrets never enter this database:
/// credential_ref points at the OS keychain or an environment variable.
/// The extension-group tables that used to be created here are gone with the software catalogue;
/// existing databases keep them and nothing reads them.
const V10: &str = r#"
CREATE TABLE IF NOT EXISTS managed_resource (
  id             INTEGER PRIMARY KEY,
  resource_id    TEXT NOT NULL UNIQUE,
  label          TEXT NOT NULL,
  kind           TEXT NOT NULL CHECK(kind IN ('database', 'object_storage', 'cloud_account')),
  provider       TEXT NOT NULL,
  environment    TEXT NOT NULL CHECK(environment IN ('development', 'test', 'staging', 'production')),
  config         TEXT NOT NULL DEFAULT '{}',
  capabilities   TEXT NOT NULL DEFAULT '[]',
  credential_ref TEXT,
  enabled        INTEGER NOT NULL DEFAULT 1 CHECK(enabled IN (0, 1)),
  fingerprint    TEXT NOT NULL,
  updated_at     TEXT NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS idx_managed_resource_kind
  ON managed_resource(kind, enabled, label);

CREATE TABLE IF NOT EXISTS managed_resource_workspace (
  resource_id  TEXT NOT NULL REFERENCES managed_resource(resource_id) ON DELETE CASCADE,
  workspace_id TEXT NOT NULL REFERENCES workspaces(workspace_id) ON DELETE CASCADE,
  PRIMARY KEY(resource_id, workspace_id)
) STRICT;
CREATE INDEX IF NOT EXISTS idx_managed_resource_workspace_workspace
  ON managed_resource_workspace(workspace_id, resource_id);
"#;

const V11: &str = r#"
UPDATE session SET exec_cwd = replace(exec_cwd, '\\?\UNC\', '\\')
  WHERE exec_cwd LIKE '\\?\UNC\%';
UPDATE session SET exec_cwd = replace(exec_cwd, '\\?\', '')
  WHERE exec_cwd LIKE '\\?\%';
"#;

const V12: &str = r#"
ALTER TABLE session_entry ADD COLUMN round_seq INTEGER;
"#;

const V13: &str = "";

const V14: &str = r#"
ALTER TABLE usage_event ADD COLUMN request_started_at TEXT;
ALTER TABLE usage_event ADD COLUMN first_token_at TEXT;
ALTER TABLE usage_event ADD COLUMN completed_at TEXT;
"#;

const V15: &str = r#"
CREATE TABLE IF NOT EXISTS agent_profile (
  id            INTEGER PRIMARY KEY,
  profile_name  TEXT NOT NULL UNIQUE,
  system_prompt TEXT NOT NULL,
  tools         TEXT NOT NULL DEFAULT '[]',
  model_ref     TEXT,
  created_at    TEXT NOT NULL,
  updated_at    TEXT NOT NULL
) STRICT;
"#;

const V16: &str = r#"
ALTER TABLE session ADD COLUMN turn_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE session ADD COLUMN last_message_at TEXT;
UPDATE session SET
  turn_count = (
    SELECT COUNT(DISTINCT e.turn_seq) FROM session_entry e
    WHERE e.session_id = session.session_id AND e.kind != 'event'
  ),
  last_message_at = (
    SELECT MAX(e.created_at) FROM session_entry e
    WHERE e.session_id = session.session_id AND e.kind != 'event'
      AND NOT EXISTS (
        SELECT 1 FROM session_locks l
        WHERE l.session_id = session.session_id AND l.turn_id = e.turn_id
      )
  ) where id>0;
CREATE INDEX IF NOT EXISTS idx_session_locks_session_turn
ON session_locks(session_id, turn_id);
"#;

const V17: &str = r#"
ALTER TABLE session ADD COLUMN effort TEXT CHECK (
  effort IS NULL OR effort IN ('minimal', 'low', 'medium', 'high', 'xhigh', 'max')
);
ALTER TABLE mailbox ADD COLUMN thinking TEXT;
"#;

const V18: &str = r#"
ALTER TABLE session_entry ADD COLUMN is_final INTEGER NOT NULL DEFAULT 0;
UPDATE session_entry SET is_final = 1 WHERE entry_id IN (
  SELECT entry_id FROM (
    SELECT entry_id,
           ROW_NUMBER() OVER (PARTITION BY session_id, turn_seq
                              ORDER BY seq DESC) AS rn
      FROM session_entry
     WHERE kind IN ('assistant_text', 'thinking')
  ) WHERE rn = 1
);
CREATE INDEX IF NOT EXISTS idx_entry_final
ON session_entry(session_id) WHERE is_final = 1;
"#;

const V19: &str = r#"
DROP INDEX IF EXISTS idx_entry_final;
CREATE INDEX IF NOT EXISTS idx_entry_final
ON session_entry(session_id, seq) WHERE is_final = 1;
CREATE INDEX IF NOT EXISTS idx_entry_input
ON session_entry(session_id, seq) WHERE kind IN ('user', 'steering');
DROP INDEX IF EXISTS idx_session_entry_session_kind_seq;
"#;

const V20: &str = r#"
ALTER TABLE entry_object ADD COLUMN kind TEXT;
ALTER TABLE entry_object ADD COLUMN label TEXT;
ALTER TABLE entry_object ADD COLUMN meta TEXT;
UPDATE entry_object SET kind = CASE
  WHEN role = 'diff' THEN 'diff'
  WHEN ref_key = 'widget' THEN 'widget'
  WHEN ref_key IS NOT NULL THEN 'file'
  WHEN role = 'output' THEN 'output'
  ELSE role
END;
CREATE INDEX IF NOT EXISTS idx_entry_object_kind ON entry_object(kind, entry_id);
"#;

// Startup reconciliation asks for every interaction request/response in the database
// (`entry::unanswered_interactions`). Without an index on the kinds that filter cannot use
// `idx_entry_kind`, whose leading column is `session_id`, so it scanned all of `session_entry`:
// 153 ms warm on a 102k-row table, up to 1.5 s cold, straight off the launch path.
// Partial rather than a full `(kind, session_id, seq)`: 384 rows instead of 102k, so 24 KB instead
// of 6 MB, one third of the build time, and it already orders by `(session_id, seq)`.
const V21: &str = r#"
CREATE INDEX IF NOT EXISTS idx_entry_interaction
ON session_entry(session_id, seq) WHERE kind IN ('interaction_request', 'interaction_response');
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn the_version_matches_the_migration_count() {
        assert_eq!(CURRENT_VERSION, MIGRATIONS.len() as i64);
        assert!(!MIGRATIONS.is_empty());
    }

    /// Every migration must be re-runnable, so a batch interrupted part way can be retried.
    #[test]
    fn every_migration_is_idempotent_on_its_own() {
        for (i, m) in MIGRATIONS.iter().enumerate() {
            for stmt in m.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                let upper = stmt.to_ascii_uppercase();
                if upper.starts_with("CREATE TABLE") || upper.starts_with("CREATE INDEX") {
                    assert!(
                        upper.contains("IF NOT EXISTS"),
                        "migration {i} has a CREATE without IF NOT EXISTS: {stmt}"
                    );
                }
            }
        }
    }

    #[test]
    fn v11_strips_legacy_windows_verbatim_prefixes() {
        let conn = Connection::open_in_memory().unwrap();
        const V11_IDX: usize = 10;
        for m in &MIGRATIONS[..V11_IDX] {
            conn.execute_batch(m).unwrap();
        }
        conn.execute_batch(
            "INSERT INTO session (session_id, workspace_id, kind, agent, agent_paths, root_session_id, exec_cwd, created_at, updated_at)
             VALUES ('s1', 'ws1', 'chat', 'main', 'main', 's1', '\\\\?\\D:\\proj', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z');",
        )
        .unwrap();
        conn.execute_batch(MIGRATIONS[V11_IDX]).unwrap();

        let one = |sql: &str| -> String { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
        assert_eq!(
            one("SELECT exec_cwd FROM session WHERE session_id = 's1'"),
            r"D:\proj"
        );
    }
}
