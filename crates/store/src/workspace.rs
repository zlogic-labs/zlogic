use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, named_params};
use zlogic_protocol::WorkspaceId;

use crate::{Result, StoreError, now};

const COLS: &str =
    "workspace_id, name, path, created_at, pinned, sort_order, last_opened_at, hidden, tools";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRecord {
    pub workspace_id: WorkspaceId,
    pub name: String,
    pub path: String,
    pub created_at: DateTime<Utc>,
    pub pinned: bool,
    pub sort_order: i32,
    pub last_opened_at: Option<DateTime<Utc>>,
    pub hidden: bool,
    pub tools: Option<Vec<String>>,
}

impl WorkspaceRecord {
    pub fn as_path(&self) -> &std::path::Path {
        std::path::Path::new(&self.path)
    }

    pub fn exists(&self) -> bool {
        self.as_path().is_dir()
    }
}

pub struct WorkspaceStore<'a> {
    conn: &'a Connection,
}

impl<'a> WorkspaceStore<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub fn resolve(&self, path: impl AsRef<std::path::Path>) -> Result<(WorkspaceRecord, bool)> {
        self.resolve_named(path, None)
    }

    pub fn resolve_named(
        &self,
        path: impl AsRef<std::path::Path>,
        name: Option<&str>,
    ) -> Result<(WorkspaceRecord, bool)> {
        self.resolve_with(path, name, None)
    }

    pub fn resolve_with(
        &self,
        path: impl AsRef<std::path::Path>,
        name: Option<&str>,
        tools: Option<&[String]>,
    ) -> Result<(WorkspaceRecord, bool)> {
        let path = normalise(path.as_ref());
        if let Some(existing) = self.find_by_path(&path)? {
            return Ok((existing, false));
        }

        let id = WorkspaceId::new();
        let name = name
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| default_name(&path));
        let affected = self.conn.execute(
            "INSERT OR IGNORE INTO workspaces (workspace_id, name, path, created_at, tools)
             VALUES (:workspace_id, :name, :path, :ts, :tools)",
            named_params! {
                ":workspace_id": id,
                ":name": name,
                ":path": path,
                ":ts": now(),
                ":tools": encode_tools(tools)?,
            },
        )?;

        let record = self.find_by_path(&path)?.ok_or_else(|| {
            StoreError::Corrupt(format!("registered {path} but could not read the row back"))
        })?;
        Ok((record, affected == 1))
    }

    pub fn get(&self, workspace_id: WorkspaceId) -> Result<WorkspaceRecord> {
        self.find(workspace_id)?
            .ok_or_else(|| StoreError::NotFound {
                kind: "workspace",
                id: workspace_id.to_string(),
            })
    }

    pub fn find(&self, workspace_id: WorkspaceId) -> Result<Option<WorkspaceRecord>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {COLS} FROM workspaces WHERE workspace_id = :id"),
                named_params! { ":id": workspace_id },
                map_row,
            )
            .optional()?)
    }

    pub fn find_by_path(
        &self,
        path: impl AsRef<std::path::Path>,
    ) -> Result<Option<WorkspaceRecord>> {
        let path = normalise(path.as_ref());
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {COLS} FROM workspaces WHERE path = :path"),
                named_params! { ":path": path },
                map_row,
            )
            .optional()?)
    }

    pub fn list(&self, include_hidden: bool) -> Result<Vec<WorkspaceRecord>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM workspaces
             WHERE (:include_hidden OR hidden = 0)
             ORDER BY pinned DESC, sort_order ASC, last_opened_at DESC, id DESC"
        ))?;
        let rows = stmt.query_map(named_params! { ":include_hidden": include_hidden }, map_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn touch(&self, workspace_id: WorkspaceId) -> Result<()> {
        self.conn.execute(
            "UPDATE workspaces SET last_opened_at = :ts, hidden = 0 WHERE workspace_id = :id",
            named_params! { ":ts": now(), ":id": workspace_id },
        )?;
        Ok(())
    }

    pub fn set_preferences(
        &self,
        workspace_id: WorkspaceId,
        pinned: Option<bool>,
        sort_order: Option<i32>,
        hidden: Option<bool>,
    ) -> Result<WorkspaceRecord> {
        self.conn.execute(
            "UPDATE workspaces
                SET pinned     = COALESCE(:pinned, pinned),
                    sort_order = COALESCE(:sort_order, sort_order),
                    hidden     = COALESCE(:hidden, hidden)
              WHERE workspace_id = :id",
            named_params! {
                ":pinned": pinned,
                ":sort_order": sort_order,
                ":hidden": hidden,
                ":id": workspace_id,
            },
        )?;
        self.get(workspace_id)
    }

    pub fn session_count(&self, workspace_id: WorkspaceId) -> Result<u32> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM session
             WHERE workspace_id = :id
               AND parent_session_id IS NULL
               AND archived_at IS NULL
               AND kind = 'chat'",
            named_params! {
                ":id": workspace_id,
            },
            |r| r.get(0),
        )?;
        Ok(n as u32)
    }

    pub fn set_tools(
        &self,
        workspace_id: WorkspaceId,
        tools: Option<&[String]>,
    ) -> Result<WorkspaceRecord> {
        self.conn.execute(
            "UPDATE workspaces SET tools = :tools WHERE workspace_id = :id",
            named_params! { ":tools": encode_tools(tools)?, ":id": workspace_id },
        )?;
        self.get(workspace_id)
    }

    pub fn rename(&self, workspace_id: WorkspaceId, name: &str) -> Result<WorkspaceRecord> {
        let name = name.trim();
        if name.is_empty() {
            return Err(StoreError::Corrupt(
                "workspace name must not be empty".into(),
            ));
        }
        self.conn.execute(
            "UPDATE workspaces SET name = :name WHERE workspace_id = :id",
            named_params! { ":name": name, ":id": workspace_id },
        )?;
        self.get(workspace_id)
    }

    pub fn rebind(
        &self,
        workspace_id: WorkspaceId,
        path: impl AsRef<std::path::Path>,
    ) -> Result<WorkspaceRecord> {
        let path = normalise(path.as_ref());
        if let Some(other) = self.find_by_path(&path)?
            && other.workspace_id != workspace_id
        {
            return Err(StoreError::Corrupt(format!(
                "{path} already belongs to another workspace"
            )));
        }
        self.conn.execute(
            "UPDATE workspaces SET path = :path WHERE workspace_id = :id",
            named_params! { ":path": path, ":id": workspace_id },
        )?;
        self.get(workspace_id)
    }

    pub fn delete(&self, workspace_id: WorkspaceId) -> Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM workspaces WHERE workspace_id = :id",
            named_params! { ":id": workspace_id },
        )?;
        Ok(n > 0)
    }
}

pub use zlogic_paths::normalise;

fn default_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| path.to_string())
}

fn encode_tools(tools: Option<&[String]>) -> Result<Option<String>> {
    tools
        .map(|list| {
            serde_json::to_string(list).map_err(|e| {
                StoreError::Corrupt(format!("failed to serialize tool whitelist: {e}"))
            })
        })
        .transpose()
}

fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkspaceRecord> {
    let tools: Option<String> = row.get("tools")?;
    let tools = tools
        .map(|text| {
            serde_json::from_str::<Vec<String>>(&text).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    row.as_ref().column_index("tools").unwrap_or_default(),
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })
        })
        .transpose()?;
    Ok(WorkspaceRecord {
        workspace_id: row.get("workspace_id")?,
        name: row.get("name")?,
        path: row.get("path")?,
        created_at: row.get("created_at")?,
        pinned: row.get("pinned")?,
        sort_order: row.get("sort_order")?,
        last_opened_at: row.get("last_opened_at")?,
        hidden: row.get("hidden")?,
        tools,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;

    #[test]
    fn an_explicit_name_only_applies_on_create() {
        let db = db();
        let dir = tempfile::tempdir().unwrap();

        let (created, is_new) = db
            .workspaces()
            .resolve_named(dir.path(), Some("my project"))
            .unwrap();
        assert!(is_new);
        assert_eq!(created.name, "my project");

        let (again, is_new) = db
            .workspaces()
            .resolve_named(dir.path(), Some("another name"))
            .unwrap();
        assert!(!is_new);
        assert_eq!(again.name, "my project");
    }

    #[test]
    fn a_blank_name_falls_back_to_the_directory() {
        let db = db();
        let dir = tempfile::tempdir().unwrap();
        let (created, _) = db
            .workspaces()
            .resolve_named(dir.path(), Some("   "))
            .unwrap();
        assert_eq!(
            created.name,
            dir.path().file_name().unwrap().to_string_lossy()
        );
    }

    fn db() -> Db {
        Db::open_in_memory().unwrap()
    }

    #[test]
    fn a_new_workspace_offers_every_tool_by_default() {
        let db = db();
        let dir = tempfile::tempdir().unwrap();
        let (ws, _) = db.workspaces().resolve(dir.path()).unwrap();
        assert_eq!(ws.tools, None);
    }

    #[test]
    fn an_initial_tool_list_only_applies_on_create() {
        let db = db();
        let dir = tempfile::tempdir().unwrap();
        let chat = zlogic_protocol::chat_workspace_tools();

        let (created, is_new) = db
            .workspaces()
            .resolve_with(dir.path(), None, Some(&chat))
            .unwrap();
        assert!(is_new);
        assert_eq!(created.tools.as_deref(), Some(chat.as_slice()));

        let (again, is_new) = db
            .workspaces()
            .resolve_with(dir.path(), None, Some(&["shell".to_string()]))
            .unwrap();
        assert!(!is_new);
        assert_eq!(
            again.tools.as_deref(),
            Some(chat.as_slice()),
            "reopening does not overwrite"
        );
    }

    #[test]
    fn set_tools_round_trips_every_shape() {
        let db = db();
        let dir = tempfile::tempdir().unwrap();
        let (ws, _) = db.workspaces().resolve(dir.path()).unwrap();
        let id = ws.workspace_id;

        let only = db
            .workspaces()
            .set_tools(id, Some(&["web_fetch".to_string()]))
            .unwrap();
        assert_eq!(only.tools.as_deref(), Some(&["web_fetch".to_string()][..]));

        let none_at_all = db.workspaces().set_tools(id, Some(&[])).unwrap();
        assert_eq!(none_at_all.tools.as_deref(), Some(&[][..]));

        let unrestricted = db.workspaces().set_tools(id, None).unwrap();
        assert_eq!(unrestricted.tools, None);
    }

    #[test]
    fn resolving_a_new_directory_registers_it() {
        let db = db();
        let dir = tempfile::tempdir().unwrap();

        let (ws, created) = db.workspaces().resolve(dir.path()).unwrap();
        assert!(created, "the first time creates it");
        assert_eq!(
            ws.path,
            normalise(dir.path()),
            "what is stored is the normalised key (realpath + verbatim prefix stripped), not something comparable to canonicalize directly"
        );
        assert!(ws.exists());
        assert_eq!(ws.name, dir.path().file_name().unwrap().to_string_lossy());
    }

    #[test]
    fn resolving_the_same_directory_twice_returns_one_id() {
        let db = db();
        let dir = tempfile::tempdir().unwrap();

        let (first, created_a) = db.workspaces().resolve(dir.path()).unwrap();
        let (second, created_b) = db.workspaces().resolve(dir.path()).unwrap();

        assert!(
            created_a && !created_b,
            "the second time does not create it"
        );
        assert_eq!(first.workspace_id, second.workspace_id);
        assert_eq!(db.workspaces().list(false).unwrap().len(), 1);
    }

    #[test]
    fn equivalent_spellings_of_a_path_resolve_to_one_workspace() {
        let db = db();
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();

        let (a, _) = db.workspaces().resolve(base).unwrap();
        for spelling in [
            base.join("."),
            base.join("sub").join(".."),
            std::path::PathBuf::from(format!("{}/", base.display())),
        ] {
            std::fs::create_dir_all(base.join("sub")).unwrap();
            let (b, created) = db.workspaces().resolve(&spelling).unwrap();
            assert!(
                !created,
                "{} must not create a new workspace",
                spelling.display()
            );
            assert_eq!(a.workspace_id, b.workspace_id, "{}", spelling.display());
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_to_the_same_directory_is_the_same_workspace() {
        let db = db();
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        let link = dir.path().join("link");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let (a, _) = db.workspaces().resolve(&real).unwrap();
        let (b, created) = db.workspaces().resolve(&link).unwrap();
        assert!(!created);
        assert_eq!(a.workspace_id, b.workspace_id);
    }

    #[test]
    fn a_missing_directory_still_normalises_consistently() {
        let db = db();
        let missing = "/definitely/not/here/proj";

        let (a, created) = db.workspaces().resolve(missing).unwrap();
        assert!(created);
        assert!(
            !a.exists(),
            "it must be marked as missing rather than pretending to be fine"
        );

        let (b, created) = db.workspaces().resolve(format!("{missing}/")).unwrap();
        assert!(!created);
        assert_eq!(a.workspace_id, b.workspace_id);
        assert_eq!(a.name, "proj");
    }

    #[test]
    fn renaming_changes_the_label_but_not_the_identity() {
        let db = db();
        let dir = tempfile::tempdir().unwrap();
        let (ws, _) = db.workspaces().resolve(dir.path()).unwrap();

        let renamed = db
            .workspaces()
            .rename(ws.workspace_id, "  my project  ")
            .unwrap();
        assert_eq!(
            renamed.name, "my project",
            "surrounding whitespace must be trimmed"
        );
        assert_eq!(renamed.workspace_id, ws.workspace_id);
        assert_eq!(renamed.path, ws.path);

        assert!(
            db.workspaces().rename(ws.workspace_id, "   ").is_err(),
            "an empty name would become an invisible row"
        );
    }

    #[test]
    fn rebinding_moves_the_directory_and_keeps_the_id() {
        let db = db();
        let old = tempfile::tempdir().unwrap();
        let new = tempfile::tempdir().unwrap();
        let (ws, _) = db.workspaces().resolve(old.path()).unwrap();

        let moved = db.workspaces().rebind(ws.workspace_id, new.path()).unwrap();
        assert_eq!(
            moved.workspace_id, ws.workspace_id,
            "identity is the id, not the path"
        );
        assert_eq!(moved.path, normalise(new.path()));

        let (again, created) = db.workspaces().resolve(new.path()).unwrap();
        assert!(!created);
        assert_eq!(again.workspace_id, ws.workspace_id);
        assert_eq!(db.workspaces().list(false).unwrap().len(), 1);
    }

    #[test]
    fn rebinding_onto_another_workspaces_directory_is_refused() {
        let db = db();
        let a_dir = tempfile::tempdir().unwrap();
        let b_dir = tempfile::tempdir().unwrap();
        let (a, _) = db.workspaces().resolve(a_dir.path()).unwrap();
        let (b, _) = db.workspaces().resolve(b_dir.path()).unwrap();

        assert!(
            db.workspaces()
                .rebind(a.workspace_id, b_dir.path())
                .is_err()
        );
        assert!(db.workspaces().rebind(b.workspace_id, b_dir.path()).is_ok());
    }

    #[test]
    fn listing_puts_the_newest_first() {
        let db = db();
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let (a, _) = db.workspaces().resolve(first.path()).unwrap();
        let (b, _) = db.workspaces().resolve(second.path()).unwrap();

        let ids: Vec<_> = db
            .workspaces()
            .list(false)
            .unwrap()
            .into_iter()
            .map(|w| w.workspace_id)
            .collect();
        assert_eq!(ids, [b.workspace_id, a.workspace_id]);
    }

    #[test]
    fn a_new_workspace_starts_with_neutral_preferences() {
        let db = db();
        let dir = tempfile::tempdir().unwrap();
        let (ws, _) = db.workspaces().resolve(dir.path()).unwrap();

        assert!(!ws.pinned);
        assert_eq!(ws.sort_order, 0);
        assert_eq!(ws.last_opened_at, None, "never opened yet");
        assert!(!ws.hidden);
    }

    #[test]
    fn hiding_a_workspace_keeps_its_row_its_id_and_its_sessions() {
        let db = db();
        let dir = tempfile::tempdir().unwrap();
        let (ws, _) = db.workspaces().resolve(dir.path()).unwrap();
        let session = db
            .sessions()
            .create(crate::NewSession::root(ws.workspace_id))
            .unwrap()
            .session_id;

        db.workspaces()
            .set_preferences(ws.workspace_id, None, None, Some(true))
            .unwrap();

        assert!(db.workspaces().list(false).unwrap().is_empty());
        assert_eq!(db.workspaces().list(true).unwrap().len(), 1);
        assert_eq!(
            db.workspaces().get(ws.workspace_id).unwrap().workspace_id,
            ws.workspace_id
        );
        assert!(db.sessions().find(session).unwrap().is_some());

        let (again, created) = db.workspaces().resolve(dir.path()).unwrap();
        assert!(!created);
        assert_eq!(again.workspace_id, ws.workspace_id);
    }

    #[test]
    fn touching_a_hidden_workspace_brings_it_back() {
        let db = db();
        let dir = tempfile::tempdir().unwrap();
        let (ws, _) = db.workspaces().resolve(dir.path()).unwrap();
        db.workspaces()
            .set_preferences(ws.workspace_id, None, None, Some(true))
            .unwrap();

        db.workspaces().touch(ws.workspace_id).unwrap();

        let after = db.workspaces().get(ws.workspace_id).unwrap();
        assert!(!after.hidden);
        assert!(after.last_opened_at.is_some());
        assert_eq!(db.workspaces().list(false).unwrap().len(), 1);
    }

    #[test]
    fn updating_one_preference_leaves_the_others_alone() {
        let db = db();
        let dir = tempfile::tempdir().unwrap();
        let (ws, _) = db.workspaces().resolve(dir.path()).unwrap();
        db.workspaces()
            .set_preferences(ws.workspace_id, Some(true), Some(7), None)
            .unwrap();

        let after = db
            .workspaces()
            .set_preferences(ws.workspace_id, None, None, Some(false))
            .unwrap();
        assert!(after.pinned, "pinning must not be cleared");
        assert_eq!(after.sort_order, 7, "sort order must not be cleared");
    }

    #[test]
    fn the_sidebar_order_is_pinned_then_manual_then_recent() {
        let db = db();
        let make = |name: &str| {
            let dir = std::env::temp_dir().join(format!("zlogic-order-{name}"));
            std::fs::create_dir_all(&dir).unwrap();
            db.workspaces().resolve(&dir).unwrap().0.workspace_id
        };
        let (a, b, c) = (make("a"), make("b"), make("c"));

        db.workspaces().touch(c).unwrap();
        db.workspaces()
            .set_preferences(b, None, Some(-1), None)
            .unwrap();
        db.workspaces()
            .set_preferences(a, Some(true), Some(100), None)
            .unwrap();

        let order: Vec<_> = db
            .workspaces()
            .list(false)
            .unwrap()
            .into_iter()
            .map(|w| w.workspace_id)
            .collect();
        assert_eq!(
            order,
            [a, b, c],
            "pinned outranks manual position, and manual position outranks recently opened"
        );
    }

    #[test]
    fn the_session_count_covers_only_top_level_live_sessions() {
        let db = db();
        let dir = tempfile::tempdir().unwrap();
        let (ws, _) = db.workspaces().resolve(dir.path()).unwrap();

        let root = db
            .sessions()
            .create(crate::NewSession::root(ws.workspace_id))
            .unwrap();
        db.sessions()
            .create(crate::NewSession::child(root.session_id, "helper"))
            .unwrap();
        let archived = db
            .sessions()
            .create(crate::NewSession::root(ws.workspace_id))
            .unwrap();
        db.sessions().archive(archived.session_id).unwrap();
        db.sessions()
            .create(crate::NewSession::task(ws.workspace_id))
            .unwrap();

        assert_eq!(db.workspaces().session_count(ws.workspace_id).unwrap(), 1);
    }

    #[test]
    fn get_reports_a_missing_workspace_as_not_found() {
        let db = db();
        let err = db.workspaces().get(WorkspaceId::new()).unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::NotFound {
                    kind: "workspace",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn deleting_the_row_leaves_the_sessions_alone() {
        let db = db();
        let dir = tempfile::tempdir().unwrap();
        let (ws, _) = db.workspaces().resolve(dir.path()).unwrap();
        let session = db
            .sessions()
            .create(crate::NewSession::root(ws.workspace_id))
            .unwrap()
            .session_id;

        assert!(db.workspaces().delete(ws.workspace_id).unwrap());
        assert!(db.workspaces().find(ws.workspace_id).unwrap().is_none());
        assert!(
            db.sessions().find(session).unwrap().is_some(),
            "the sessions must not disappear with it"
        );
        assert!(
            !db.workspaces().delete(ws.workspace_id).unwrap(),
            "deleting a second time is false"
        );
    }
}
