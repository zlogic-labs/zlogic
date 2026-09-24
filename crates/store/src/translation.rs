//! Quick translate's history — and its cache.
//!
//! One row per translation the user asked for. Two reads matter: the recent list the history panel
//! shows, and the exact-match lookup that lets the same text into the same language be answered
//! without calling a model at all.
//!
//! The match is on `(target_code, source_text)`, byte for byte: a cache that "found something
//! close" would be a cache that hands back the wrong translation.

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, named_params};

use crate::{StoreError, now};

const COLS: &str = "translation_id, workspace_id, target, target_code, source_text, result_text,
                    model_ref, created_at";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslationRecord {
    pub translation_id: zlogic_protocol::TranslationId,
    pub workspace_id: zlogic_protocol::WorkspaceId,
    pub target: String,
    pub target_code: String,
    pub source_text: String,
    pub result_text: String,
    pub model_ref: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// What to store. Ids and timestamps are the store's business, not the caller's.
pub struct NewTranslation<'a> {
    pub workspace_id: zlogic_protocol::WorkspaceId,
    pub target: &'a str,
    pub target_code: &'a str,
    pub source_text: &'a str,
    pub result_text: &'a str,
    pub model_ref: Option<&'a str>,
}

pub struct TranslationStore<'a> {
    conn: &'a Connection,
}

impl<'a> TranslationStore<'a> {
    pub(crate) fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub fn insert(&self, new: NewTranslation<'_>) -> Result<TranslationRecord, StoreError> {
        let translation_id = zlogic_protocol::TranslationId::new();
        let created_at = now();
        self.conn.execute(
            "INSERT INTO translation (
               translation_id, workspace_id, target, target_code, source_text, result_text,
               model_ref, created_at
             ) VALUES (
               :translation_id, :workspace_id, :target, :target_code, :source_text, :result_text,
               :model_ref, :created_at
             )",
            named_params! {
                ":translation_id": translation_id,
                ":workspace_id": new.workspace_id,
                ":target": new.target,
                ":target_code": new.target_code,
                ":source_text": new.source_text,
                ":result_text": new.result_text,
                ":model_ref": new.model_ref,
                ":created_at": created_at,
            },
        )?;
        Ok(TranslationRecord {
            translation_id,
            workspace_id: new.workspace_id,
            target: new.target.to_owned(),
            target_code: new.target_code.to_owned(),
            source_text: new.source_text.to_owned(),
            result_text: new.result_text.to_owned(),
            model_ref: new.model_ref.map(str::to_owned),
            created_at,
        })
    }

    /// The most recent first. `limit` is clamped by the caller, not here: the protocol owns what a
    /// request is allowed to ask for.
    pub fn list(&self, limit: u32) -> Result<Vec<TranslationRecord>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM translation ORDER BY created_at DESC, id DESC LIMIT :limit"
        ))?;
        let rows = stmt.query_map(named_params! { ":limit": limit }, row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// The newest translation of exactly this text into exactly this language, if there is one.
    pub fn find(
        &self,
        target_code: &str,
        source_text: &str,
    ) -> Result<Option<TranslationRecord>, StoreError> {
        self.conn
            .query_row(
                &format!(
                    "SELECT {COLS} FROM translation
                      WHERE target_code = :target_code AND source_text = :source_text
                      ORDER BY created_at DESC, id DESC
                      LIMIT 1"
                ),
                named_params! {
                    ":target_code": target_code,
                    ":source_text": source_text,
                },
                row,
            )
            .optional()
            .map_err(Into::into)
    }

    /// How many rows are stored, for the history header.
    pub fn count(&self) -> Result<u64, StoreError> {
        self.conn
            .query_row("SELECT COUNT(*) FROM translation", [], |r| r.get(0))
            .map_err(Into::into)
    }

    pub fn delete(&self, translation_id: zlogic_protocol::TranslationId) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            "DELETE FROM translation WHERE translation_id = :translation_id",
            named_params! { ":translation_id": translation_id },
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound {
                kind: "translation",
                id: translation_id.to_string(),
            });
        }
        Ok(())
    }

    /// Forgets everything, and reports how many rows went.
    pub fn clear(&self) -> Result<usize, StoreError> {
        self.conn
            .execute("DELETE FROM translation", [])
            .map_err(Into::into)
    }
}

fn row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TranslationRecord> {
    Ok(TranslationRecord {
        translation_id: row.get("translation_id")?,
        workspace_id: row.get("workspace_id")?,
        target: row.get("target")?,
        target_code: row.get("target_code")?,
        source_text: row.get("source_text")?,
        result_text: row.get("result_text")?,
        model_ref: row.get("model_ref")?,
        created_at: row.get("created_at")?,
    })
}

#[cfg(test)]
mod tests {
    use zlogic_protocol::{TranslationId, WorkspaceId};

    use crate::Db;

    fn new<'a>(workspace_id: WorkspaceId, to: &'a str, text: &'a str) -> crate::NewTranslation<'a> {
        crate::NewTranslation {
            workspace_id,
            target: "English",
            target_code: to,
            source_text: text,
            result_text: "hello",
            model_ref: Some("gpt-5:mini"),
        }
    }

    #[test]
    fn stores_lists_and_deletes_newest_first() {
        let db = Db::open_in_memory().unwrap();
        let store = db.translations();
        let ws = WorkspaceId::new();
        assert!(store.list(10).unwrap().is_empty());

        let first = store.insert(new(ws, "en", "你好")).unwrap();
        let second = store.insert(new(ws, "en", "谢谢")).unwrap();

        let listed = store.list(10).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].translation_id, second.translation_id);
        assert_eq!(listed[1].translation_id, first.translation_id);
        assert_eq!(listed[0].source_text, "谢谢");
        assert_eq!(listed[0].model_ref.as_deref(), Some("gpt-5:mini"));
        assert_eq!(store.count().unwrap(), 2);

        store.delete(first.translation_id).unwrap();
        assert_eq!(store.count().unwrap(), 1);
        assert!(matches!(
            store.delete(first.translation_id),
            Err(crate::StoreError::NotFound { .. })
        ));
        // A well-formed id that was never stored is a NotFound too, not a silent success.
        assert!(matches!(
            store.delete(TranslationId::new()),
            Err(crate::StoreError::NotFound { .. })
        ));
    }

    #[test]
    fn the_lookup_is_exact_on_both_language_and_text() {
        let db = Db::open_in_memory().unwrap();
        let store = db.translations();
        let ws = WorkspaceId::new();
        let stored = store.insert(new(ws, "en", "你好")).unwrap();

        let hit = store.find("en", "你好").unwrap().unwrap();
        assert_eq!(hit.translation_id, stored.translation_id);
        // Same text, another language: not a hit.
        assert!(store.find("ja", "你好").unwrap().is_none());
        // Same language, different text — including a whitespace difference: not a hit.
        assert!(store.find("en", "你好 ").unwrap().is_none());
    }

    #[test]
    fn the_lookup_takes_the_newest_of_repeats() {
        let db = Db::open_in_memory().unwrap();
        let store = db.translations();
        let ws = WorkspaceId::new();
        store.insert(new(ws, "en", "你好")).unwrap();
        let newer = store.insert(new(ws, "en", "你好")).unwrap();

        assert_eq!(
            store.find("en", "你好").unwrap().unwrap().translation_id,
            newer.translation_id
        );
    }

    #[test]
    fn clear_reports_what_it_removed() {
        let db = Db::open_in_memory().unwrap();
        let store = db.translations();
        let ws = WorkspaceId::new();
        store.insert(new(ws, "en", "你好")).unwrap();
        store.insert(new(ws, "en", "谢谢")).unwrap();

        assert_eq!(store.clear().unwrap(), 2);
        assert!(store.list(10).unwrap().is_empty());
        assert_eq!(store.clear().unwrap(), 0);
    }

    #[test]
    fn the_list_honours_its_limit() {
        let db = Db::open_in_memory().unwrap();
        let store = db.translations();
        let ws = WorkspaceId::new();
        for text in ["一", "二", "三"] {
            store.insert(new(ws, "en", text)).unwrap();
        }
        assert_eq!(store.list(2).unwrap().len(), 2);
        assert_eq!(store.list(2).unwrap()[0].source_text, "三");
    }
}
