//! Local SQLite storage for synced items.
//!
//! Tables:
//! - `items`: all item types with common columns (id, type_) + JSON payload of props
//! - `sync_items`: per-item sync metadata (sync_time, remote updated_time)
//! - `kv`: small key/value store (delta context, sync info cache, password check)
//! - `resource_blobs`: decrypted resource binaries
//!
//! A JSON payload keeps us schema-free for the wide Joplin tables while typed
//! columns (title/body/parent_id/timestamps) power list queries and search.

use crate::error::{Error, Result};
use crate::items::{Item, unserialize};
use rusqlite::Connection;
use serde_json::Value;
use std::path::Path;

pub struct Storage {
    conn: Connection,
}

impl Storage {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::Storage(format!("mkdir: {e}")))?;
        }
        let conn = Connection::open(path).map_err(|e| Error::Storage(format!("open db: {e}")))?;
        let storage = Self { conn };
        storage.migrate()?;
        Ok(storage)
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(|e| Error::Storage(format!("open db: {e}")))?;
        let storage = Self { conn };
        storage.migrate()?;
        Ok(storage)
    }

    fn migrate(&self) -> Result<()> {
        self.conn
            .execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS items (
                    id TEXT PRIMARY KEY,
                    type_ INT NOT NULL,
                    parent_id TEXT NOT NULL DEFAULT '',
                    title TEXT NOT NULL DEFAULT '',
                    body TEXT NOT NULL DEFAULT '',
                    created_time INT NOT NULL DEFAULT 0,
                    updated_time INT NOT NULL DEFAULT 0,
                    deleted_time INT NOT NULL DEFAULT 0,
                    is_todo INT NOT NULL DEFAULT 0,
                    todo_completed INT NOT NULL DEFAULT 0,
                    todo_due INT NOT NULL DEFAULT 0,
                    mime TEXT NOT NULL DEFAULT '',
                    file_extension TEXT NOT NULL DEFAULT '',
                    encryption_applied INT NOT NULL DEFAULT 0,
                    payload TEXT NOT NULL,
                    body_text TEXT NOT NULL DEFAULT ''
                );
                CREATE INDEX IF NOT EXISTS items_type_updated ON items (type_, updated_time);
                CREATE INDEX IF NOT EXISTS items_parent ON items (parent_id);

                CREATE TABLE IF NOT EXISTS sync_items (
                    item_id TEXT PRIMARY KEY,
                    sync_time INT NOT NULL DEFAULT 0,
                    remote_updated_time INT NOT NULL DEFAULT 0
                );

                CREATE TABLE IF NOT EXISTS kv (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS resource_blobs (
                    resource_id TEXT PRIMARY KEY,
                    data BLOB NOT NULL
                );
                "#,
            )
            .map_err(|e| Error::Storage(format!("migrate: {e}")))?;
        Ok(())
    }

    /// Upsert an item from its parsed form.
    pub fn upsert_item(&self, item: &Item) -> Result<()> {
        let id = item.props.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        if id.is_empty() {
            return Err(Error::Storage("item without id".into()));
        }
        let get_str = |k: &str| item.props.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        let get_int = |k: &str| item.props.get(k).and_then(Value::as_i64).unwrap_or(0);
        let title = get_str("title");
        let parent_id = get_str("parent_id");
        let body = if item.item_type == crate::items::TYPE_NOTE { item.body.clone() } else { String::new() };
        // Store the body inside the payload too so items round-trip when read back.
        let mut payload_props = item.props.clone();
        if item.item_type == crate::items::TYPE_NOTE {
            payload_props.insert("body".to_string(), Value::String(body.clone()));
        }
        // Searchable text: body + title. For encrypted notes, no plaintext.
        let body_text = format!("{}\n{}", title, body);
        let payload = serde_json::to_string(&payload_props).map_err(|e| Error::Storage(format!("payload: {e}")))?;
        self.conn
            .execute(
                r#"INSERT INTO items (id, type_, parent_id, title, body, created_time, updated_time, deleted_time,
                     is_todo, todo_completed, todo_due, mime, file_extension, encryption_applied, payload, body_text)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
                   ON CONFLICT(id) DO UPDATE SET
                     type_=?2, parent_id=?3, title=?4, body=?5, created_time=?6, updated_time=?7, deleted_time=?8,
                     is_todo=?9, todo_completed=?10, todo_due=?11, mime=?12, file_extension=?13,
                     encryption_applied=?14, payload=?15, body_text=?16"#,
                rusqlite::params![
                    id,
                    item.item_type,
                    parent_id,
                    title,
                    body,
                    get_int("created_time"),
                    get_int("updated_time"),
                    get_int("deleted_time"),
                    get_int("is_todo"),
                    get_int("todo_completed"),
                    get_int("todo_due"),
                    get_str("mime"),
                    get_str("file_extension"),
                    get_int("encryption_applied"),
                    payload,
                    body_text,
                ],
            )
            .map_err(|e| Error::Storage(format!("upsert: {e}")))?;
        Ok(())
    }

    /// Record sync metadata for an item.
    pub fn save_sync_time(&self, item_id: &str, sync_time: i64, remote_updated_time: i64) -> Result<()> {
        self.conn
            .execute(
                r#"INSERT INTO sync_items (item_id, sync_time, remote_updated_time) VALUES (?1, ?2, ?3)
                   ON CONFLICT(item_id) DO UPDATE SET sync_time=?2, remote_updated_time=?3"#,
                rusqlite::params![item_id, sync_time, remote_updated_time],
            )
            .map_err(|e| Error::Storage(format!("save sync time: {e}")))?;
        Ok(())
    }

    pub fn sync_time(&self, item_id: &str) -> Result<Option<(i64, i64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT sync_time, remote_updated_time FROM sync_items WHERE item_id = ?1")
            .map_err(|e| Error::Storage(format!("sync_time: {e}")))?;
        let mut rows = stmt.query([item_id]).map_err(|e| Error::Storage(format!("sync_time: {e}")))?;
        if let Some(row) = rows.next().map_err(|e| Error::Storage(format!("sync_time: {e}")))? {
            Ok(Some((row.get(0).map_err(|e| Error::Storage(e.to_string()))?, row.get(1).map_err(|e| Error::Storage(e.to_string()))?)))
        } else {
            Ok(None)
        }
    }

    /// All locally-synced item ids (used for deletion detection).
    pub fn all_item_ids(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM items")
            .map_err(|e| Error::Storage(format!("all ids: {e}")))?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| Error::Storage(format!("all ids: {e}")))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| Error::Storage(e.to_string()))?);
        }
        Ok(out)
    }

    /// Mark items as deleted locally when they disappear from the target.
    pub fn delete_items(&self, ids: &[String]) -> Result<()> {
        for id in ids {
            self.conn
                .execute("DELETE FROM items WHERE id = ?1", [id])
                .map_err(|e| Error::Storage(format!("delete: {e}")))?;
            self.conn
                .execute("DELETE FROM sync_items WHERE item_id = ?1", [id])
                .map_err(|e| Error::Storage(format!("delete sync: {e}")))?;
        }
        Ok(())
    }

    pub fn kv_get(&self, key: &str) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare("SELECT value FROM kv WHERE key = ?1").map_err(|e| Error::Storage(e.to_string()))?;
        let mut rows = stmt.query([key]).map_err(|e| Error::Storage(e.to_string()))?;
        if let Some(row) = rows.next().map_err(|e| Error::Storage(e.to_string()))? {
            Ok(Some(row.get(0).map_err(|e| Error::Storage(e.to_string()))?))
        } else {
            Ok(None)
        }
    }

    pub fn kv_set(&self, key: &str, value: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO kv (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value=?2",
                [key, value],
            )
            .map_err(|e| Error::Storage(format!("kv_set: {e}")))?;
        Ok(())
    }

    pub fn save_resource_blob(&self, resource_id: &str, data: &[u8]) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO resource_blobs (resource_id, data) VALUES (?1, ?2) ON CONFLICT(resource_id) DO UPDATE SET data=?2",
                rusqlite::params![resource_id, data],
            )
            .map_err(|e| Error::Storage(format!("save blob: {e}")))?;
        Ok(())
    }

    pub fn resource_blob(&self, resource_id: &str) -> Result<Option<Vec<u8>>> {
        let mut stmt = self
            .conn
            .prepare("SELECT data FROM resource_blobs WHERE resource_id = ?1")
            .map_err(|e| Error::Storage(e.to_string()))?;
        let mut rows = stmt.query([resource_id]).map_err(|e| Error::Storage(e.to_string()))?;
        if let Some(row) = rows.next().map_err(|e| Error::Storage(e.to_string()))? {
            Ok(Some(row.get(0).map_err(|e| Error::Storage(e.to_string()))?))
        } else {
            Ok(None)
        }
    }

    // -- Query helpers used by UI/CLI --------------------------------------

    pub fn folders(&self) -> Result<Vec<Item>> {
        self.items_by_type(crate::items::TYPE_FOLDER)
    }

    pub fn tags(&self) -> Result<Vec<Item>> {
        self.items_by_type(crate::items::TYPE_TAG)
    }

    pub fn note_tags(&self) -> Result<Vec<Item>> {
        self.items_by_type(crate::items::TYPE_NOTE_TAG)
    }

    fn items_by_type(&self, t: i64) -> Result<Vec<Item>> {
        let mut stmt = self
            .conn
            .prepare("SELECT payload FROM items WHERE type_ = ?1 AND deleted_time = 0 ORDER BY title COLLATE NOCASE")
            .map_err(|e| Error::Storage(e.to_string()))?;
        let rows = stmt.query_map([t], |r| r.get::<_, String>(0)).map_err(|e| Error::Storage(e.to_string()))?;
        let mut out = Vec::new();
        for row in rows {
            let payload: String = row.map_err(|e| Error::Storage(e.to_string()))?;
            let props: serde_json::Map<String, Value> =
                serde_json::from_str(&payload).map_err(|e| Error::Storage(format!("payload parse: {e}")))?;
            out.push(Item {
                item_type: t,
                props: props.into_iter().collect(),
                body: String::new(),
            });
        }
        Ok(out)
    }

    /// Notes inside a folder (or all notes when folder_id is empty), newest first.
    pub fn notes(&self, folder_id: &str, include_todos: bool) -> Result<Vec<Item>> {
        let sql = if folder_id.is_empty() {
            "SELECT payload FROM items WHERE type_ = 1 AND deleted_time = 0 ORDER BY updated_time DESC".to_string()
        } else {
            format!(
                "SELECT payload FROM items WHERE type_ = 1 AND parent_id = '{}' AND deleted_time = 0 ORDER BY updated_time DESC",
                folder_id.replace('\'', "''")
            )
        };
        let _ = include_todos;
        let mut stmt = self.conn.prepare(&sql).map_err(|e| Error::Storage(e.to_string()))?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).map_err(|e| Error::Storage(e.to_string()))?;
        let mut out = Vec::new();
        for row in rows {
            let payload: String = row.map_err(|e| Error::Storage(e.to_string()))?;
            let props: serde_json::Map<String, Value> =
                serde_json::from_str(&payload).map_err(|e| Error::Storage(format!("payload parse: {e}")))?;
            let body = props.get("body").and_then(Value::as_str).unwrap_or_default().to_string();
            out.push(Item {
                item_type: crate::items::TYPE_NOTE,
                props: props.into_iter().collect(),
                body,
            });
        }
        Ok(out)
    }

    pub fn note(&self, note_id: &str) -> Result<Option<Item>> {
        let mut stmt = self
            .conn
            .prepare("SELECT payload FROM items WHERE id = ?1 AND type_ = 1")
            .map_err(|e| Error::Storage(e.to_string()))?;
        let mut rows = stmt.query([note_id]).map_err(|e| Error::Storage(e.to_string()))?;
        if let Some(row) = rows.next().map_err(|e| Error::Storage(e.to_string()))? {
            let payload: String = row.get(0).map_err(|e| Error::Storage(e.to_string()))?;
            let props: serde_json::Map<String, Value> =
                serde_json::from_str(&payload).map_err(|e| Error::Storage(format!("payload parse: {e}")))?;
            let body = props.get("body").and_then(Value::as_str).unwrap_or_default().to_string();
            return Ok(Some(Item {
                item_type: crate::items::TYPE_NOTE,
                props: props.into_iter().collect(),
                body,
            }));
        }
        Ok(None)
    }

    pub fn resource(&self, resource_id: &str) -> Result<Option<Item>> {
        let mut stmt = self
            .conn
            .prepare("SELECT payload FROM items WHERE id = ?1 AND type_ = 4")
            .map_err(|e| Error::Storage(e.to_string()))?;
        let mut rows = stmt.query([resource_id]).map_err(|e| Error::Storage(e.to_string()))?;
        if let Some(row) = rows.next().map_err(|e| Error::Storage(e.to_string()))? {
            let payload: String = row.get(0).map_err(|e| Error::Storage(e.to_string()))?;
            let props: serde_json::Map<String, Value> =
                serde_json::from_str(&payload).map_err(|e| Error::Storage(format!("payload parse: {e}")))?;
            return Ok(Some(Item {
                item_type: crate::items::TYPE_RESOURCE,
                props: props.into_iter().collect(),
                body: String::new(),
            }));
        }
        Ok(None)
    }

    /// Simple LIKE-based search over title/body.
    pub fn search_notes(&self, query: &str) -> Result<Vec<Item>> {
        let pattern = format!("%{}%", query.replace('\'', "''"));
        let mut stmt = self
            .conn
            .prepare("SELECT payload FROM items WHERE type_ = 1 AND deleted_time = 0 AND body_text LIKE ?1 ORDER BY updated_time DESC LIMIT 200")
            .map_err(|e| Error::Storage(e.to_string()))?;
        let rows = stmt.query_map([&pattern], |r| r.get::<_, String>(0)).map_err(|e| Error::Storage(e.to_string()))?;
        let mut out = Vec::new();
        for row in rows {
            let payload: String = row.map_err(|e| Error::Storage(e.to_string()))?;
            let props: serde_json::Map<String, Value> =
                serde_json::from_str(&payload).map_err(|e| Error::Storage(format!("payload parse: {e}")))?;
            let body = props.get("body").and_then(Value::as_str).unwrap_or_default().to_string();
            out.push(Item {
                item_type: crate::items::TYPE_NOTE,
                props: props.into_iter().collect(),
                body,
            });
        }
        Ok(out)
    }

    /// Parse helper used when re-reading payloads.
    pub fn parse_payload(payload: &str) -> Result<Item> {
        unserialize(payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::items::unserialize;

    const NOTE_BLOB: &str = "Test note\n\nHello body\n\nid: 0f1e2d3c4b5a69788796a5b4c3d2e1f0\ntype_: 1";

    #[test]
    fn upsert_and_query() {
        let storage = Storage::open_in_memory().unwrap();
        let item = unserialize(NOTE_BLOB).unwrap();
        storage.upsert_item(&item).unwrap();
        storage.save_sync_time(&item.props["id"].as_str().unwrap(), 123, 456).unwrap();

        let notes = storage.notes("", true).unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].props["title"], Value::String("Test note".into()));
        assert_eq!(notes[0].body, "Hello body");

        let st = storage.sync_time("0f1e2d3c4b5a69788796a5b4c3d2e1f0").unwrap().unwrap();
        assert_eq!((st.0, st.1), (123, 456));

        assert_eq!(storage.all_item_ids().unwrap().len(), 1);
        storage.delete_items(&["0f1e2d3c4b5a69788796a5b4c3d2e1f0".to_string()]).unwrap();
        assert_eq!(storage.all_item_ids().unwrap().len(), 0);
    }

    #[test]
    fn kv_roundtrip() {
        let storage = Storage::open_in_memory().unwrap();
        assert!(storage.kv_get("missing").unwrap().is_none());
        storage.kv_set("k", "v1").unwrap();
        storage.kv_set("k", "v2").unwrap();
        assert_eq!(storage.kv_get("k").unwrap().unwrap(), "v2");
    }
}
