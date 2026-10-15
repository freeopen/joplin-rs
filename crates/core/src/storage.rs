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
use crate::items::{unserialize, Item};
use rusqlite::Connection;
use serde_json::Value;
use std::path::Path;

/// Filters for note search.
#[derive(Debug, Clone, Default)]
pub struct SearchOptions {
    /// Restrict to a notebook (folder id).
    pub folder_id: Option<String>,
    /// Restrict to notes carrying a tag whose title contains this text.
    pub tag: Option<String>,
    /// Only notes that are todos.
    pub todo_only: bool,
    /// Only completed todos.
    pub completed_only: bool,
}

/// A search result with a snippet around the first match.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub id: String,
    pub title: String,
    pub snippet: String,
    pub is_todo: bool,
    pub completed: bool,
    pub folder_id: String,
}

pub struct Storage {
    conn: Connection,
}

/// Generate a Joplin-compatible item id: 32 lowercase hex chars (128 bits).
fn new_item_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
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
        let conn =
            Connection::open_in_memory().map_err(|e| Error::Storage(format!("open db: {e}")))?;
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

                CREATE TABLE IF NOT EXISTS resource_fetch_times (
                    resource_id TEXT PRIMARY KEY,
                    fetched_time INT NOT NULL DEFAULT 0
                );
                "#,
            )
            .map_err(|e| Error::Storage(format!("migrate: {e}")))?;
        Ok(())
    }

    /// Upsert an item from its parsed form.
    pub fn upsert_item(&self, item: &Item) -> Result<()> {
        let id = item
            .props
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if id.is_empty() {
            return Err(Error::Storage("item without id".into()));
        }
        let get_str = |k: &str| {
            item.props
                .get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let get_int = |k: &str| item.props.get(k).and_then(Value::as_i64).unwrap_or(0);
        let title = get_str("title");
        let parent_id = get_str("parent_id");
        let body = if item.item_type == crate::items::TYPE_NOTE {
            item.body.clone()
        } else {
            String::new()
        };
        // Store the body inside the payload too so items round-trip when read back.
        let mut payload_props = item.props.clone();
        if item.item_type == crate::items::TYPE_NOTE {
            payload_props.insert("body".to_string(), Value::String(body.clone()));
        }
        // Searchable text: body + title. For encrypted notes, no plaintext.
        let body_text = format!("{}\n{}", title, body);
        let payload = serde_json::to_string(&payload_props)
            .map_err(|e| Error::Storage(format!("payload: {e}")))?;
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
    pub fn save_sync_time(
        &self,
        item_id: &str,
        sync_time: i64,
        remote_updated_time: i64,
    ) -> Result<()> {
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
        let mut rows = stmt
            .query([item_id])
            .map_err(|e| Error::Storage(format!("sync_time: {e}")))?;
        if let Some(row) = rows
            .next()
            .map_err(|e| Error::Storage(format!("sync_time: {e}")))?
        {
            Ok(Some((
                row.get(0).map_err(|e| Error::Storage(e.to_string()))?,
                row.get(1).map_err(|e| Error::Storage(e.to_string()))?,
            )))
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
            self.delete_row(id)?;
        }
        Ok(())
    }

    /// Delete a single item's row plus its sync metadata and local-only
    /// protection marker. Does NOT record a pending remote delete (that is
    /// [`Self::delete_item`], used by the UI).
    fn delete_row(&self, id: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM items WHERE id = ?1", [id])
            .map_err(|e| Error::Storage(format!("delete: {e}")))?;
        self.conn
            .execute("DELETE FROM sync_items WHERE item_id = ?1", [id])
            .map_err(|e| Error::Storage(format!("delete sync: {e}")))?;
        // Drop the local-only protection marker with the note.
        self.conn
            .execute(
                "DELETE FROM kv WHERE key = ?1",
                [format!("{}{}", Self::NOTE_PROTECT_PREFIX, id)],
            )
            .map_err(|e| Error::Storage(format!("delete protection: {e}")))?;
        Ok(())
    }

    pub fn kv_get(&self, key: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT value FROM kv WHERE key = ?1")
            .map_err(|e| Error::Storage(e.to_string()))?;
        let mut rows = stmt
            .query([key])
            .map_err(|e| Error::Storage(e.to_string()))?;
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

    // -- Per-note reading-password protection markers (local only) --------
    //
    // Stored in the local `kv` table under `note_protected:<note_id>`, never
    // synced to Joplin. The password itself is global and lives in config.toml
    // as a PBKDF2 hash; this marker only records which notes the GUI gates.

    const NOTE_PROTECT_PREFIX: &'static str = "note_protected:";

    /// Mark a note as protected by the global reading password.
    pub fn set_note_protected(&self, note_id: &str) -> Result<()> {
        self.kv_set(&format!("{}{}", Self::NOTE_PROTECT_PREFIX, note_id), "1")
    }

    /// Remove the reading-password protection marker from a note.
    pub fn unprotect_note(&self, note_id: &str) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM kv WHERE key = ?1",
                [format!("{}{}", Self::NOTE_PROTECT_PREFIX, note_id)],
            )
            .map_err(|e| Error::Storage(format!("unprotect_note: {e}")))?;
        Ok(())
    }

    /// Whether the note is marked as reading-password protected.
    pub fn is_note_protected(&self, note_id: &str) -> Result<bool> {
        self.kv_get(&format!("{}{}", Self::NOTE_PROTECT_PREFIX, note_id))
            .map(|v| v.is_some())
    }

    /// Ids of every locally protected note (regardless of whether the note
    /// still exists; stale markers are removed by `delete_items`).
    pub fn protected_note_ids(&self) -> Result<std::collections::HashSet<String>> {
        let like = format!("{}%", Self::NOTE_PROTECT_PREFIX);
        let mut stmt = self
            .conn
            .prepare("SELECT key FROM kv WHERE key LIKE ?1")
            .map_err(|e| Error::Storage(format!("protected_note_ids: {e}")))?;
        let rows = stmt
            .query_map([like], |r| r.get::<_, String>(0))
            .map_err(|e| Error::Storage(format!("protected_note_ids: {e}")))?;
        let mut out = std::collections::HashSet::new();
        for row in rows {
            let key = row.map_err(|e| Error::Storage(e.to_string()))?;
            if let Some(id) = key.strip_prefix(Self::NOTE_PROTECT_PREFIX) {
                out.insert(id.to_string());
            }
        }
        Ok(out)
    }

    /// Remove every protection marker. Used when the global reading password
    /// is removed, since protection without a password is meaningless.
    pub fn clear_note_protections(&self) -> Result<()> {
        let like = format!("{}%", Self::NOTE_PROTECT_PREFIX);
        self.conn
            .execute("DELETE FROM kv WHERE key LIKE ?1", [like])
            .map_err(|e| Error::Storage(format!("clear_note_protections: {e}")))?;
        Ok(())
    }

    // -- Pending remote deletions (local only) ----------------------------
    //
    // When the UI deletes an item, its row is removed immediately and an id is
    // recorded under `pending_delete:<id>`. The sync `DELETE_REMOTE` step then
    // physically deletes `{id}.md` on the target and clears the record. This
    // mirrors Joplin's local `deleted_items` table (which only exists to retry
    // a remote delete across syncs); no tombstone file is uploaded.

    const PENDING_DELETE_PREFIX: &'static str = "pending_delete:";

    /// Delete an item locally (row + sync metadata + protection marker) and
    /// record it for remote deletion on the next sync.
    pub fn delete_item(&self, id: &str) -> Result<()> {
        self.delete_row(id)?;
        self.kv_set(&format!("{}{}", Self::PENDING_DELETE_PREFIX, id), "1")
    }

    /// Ids awaiting a remote delete.
    pub fn pending_deletes(&self) -> Result<Vec<String>> {
        let like = format!("{}%", Self::PENDING_DELETE_PREFIX);
        let mut stmt = self
            .conn
            .prepare("SELECT key FROM kv WHERE key LIKE ?1")
            .map_err(|e| Error::Storage(format!("pending_deletes: {e}")))?;
        let rows = stmt
            .query_map([like], |r| r.get::<_, String>(0))
            .map_err(|e| Error::Storage(format!("pending_deletes: {e}")))?;
        let mut out = Vec::new();
        for row in rows {
            let key = row.map_err(|e| Error::Storage(e.to_string()))?;
            if let Some(id) = key.strip_prefix(Self::PENDING_DELETE_PREFIX) {
                out.push(id.to_string());
            }
        }
        Ok(out)
    }

    /// Drop a pending-delete record once the remote file has been deleted.
    pub fn clear_pending_delete(&self, id: &str) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM kv WHERE key = ?1",
                [format!("{}{}", Self::PENDING_DELETE_PREFIX, id)],
            )
            .map_err(|e| Error::Storage(format!("clear_pending_delete: {e}")))?;
        Ok(())
    }

    // -- UI recency (local only) ------------------------------------------
    //
    // Remembers the last opened note (globally and per notebook) and the last
    // selected notebook so the GUI can reopen context on launch. Never synced.

    const LAST_NOTE_KEY: &'static str = "ui:last_note";
    const LAST_FOLDER_KEY: &'static str = "ui:last_folder";

    fn last_note_key(folder: Option<&str>) -> String {
        match folder {
            Some(f) => format!("{}:{}", Self::LAST_NOTE_KEY, f),
            None => Self::LAST_NOTE_KEY.to_string(),
        }
    }

    /// Last note opened in a scope: the global scope when `folder` is `None`,
    /// else the per-notebook scope.
    pub fn last_opened_note(&self, folder: Option<&str>) -> Result<Option<String>> {
        self.kv_get(&Self::last_note_key(folder))
    }

    /// Record the note opened in a scope.
    pub fn set_last_opened_note(&self, folder: Option<&str>, note_id: &str) -> Result<()> {
        self.kv_set(&Self::last_note_key(folder), note_id)
    }

    /// Last selected notebook id (`None` = the "All notes" view).
    pub fn last_selected_folder(&self) -> Result<Option<String>> {
        self.kv_get(Self::LAST_FOLDER_KEY)
    }

    /// Record the selected notebook; `None` clears it (All notes).
    pub fn set_last_selected_folder(&self, folder: Option<&str>) -> Result<()> {
        match folder {
            Some(f) => self.kv_set(Self::LAST_FOLDER_KEY, f),
            None => {
                self.conn
                    .execute("DELETE FROM kv WHERE key = ?1", [Self::LAST_FOLDER_KEY])
                    .map_err(|e| Error::Storage(format!("set_last_selected_folder: {e}")))?;
                Ok(())
            }
        }
    }

    /// Number of local changes waiting to be uploaded: edited/created plaintext
    /// items plus pending remote deletions. Drives the "unsynced" status line
    /// and the sync-before-quit check.
    pub fn local_pending_count(&self) -> Result<usize> {
        Ok(self.items_needing_sync()?.len() + self.pending_deletes()?.len())
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

    pub fn resource_fetched_time(&self, resource_id: &str) -> Result<i64> {
        let mut stmt = self
            .conn
            .prepare("SELECT fetched_time FROM resource_fetch_times WHERE resource_id = ?1")
            .map_err(|e| Error::Storage(e.to_string()))?;
        let mut rows = stmt
            .query([resource_id])
            .map_err(|e| Error::Storage(e.to_string()))?;
        if let Some(row) = rows.next().map_err(|e| Error::Storage(e.to_string()))? {
            Ok(row.get(0).map_err(|e| Error::Storage(e.to_string()))?)
        } else {
            Ok(0)
        }
    }

    pub fn set_resource_fetched_time(&self, resource_id: &str, t: i64) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO resource_fetch_times (resource_id, fetched_time) VALUES (?1, ?2) ON CONFLICT(resource_id) DO UPDATE SET fetched_time=?2",
                rusqlite::params![resource_id, t],
            )
            .map_err(|e| Error::Storage(format!("set fetch time: {e}")))?;
        Ok(())
    }

    pub fn resource_blob(&self, resource_id: &str) -> Result<Option<Vec<u8>>> {
        let mut stmt = self
            .conn
            .prepare("SELECT data FROM resource_blobs WHERE resource_id = ?1")
            .map_err(|e| Error::Storage(e.to_string()))?;
        let mut rows = stmt
            .query([resource_id])
            .map_err(|e| Error::Storage(e.to_string()))?;
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

    pub fn items_by_type(&self, t: i64) -> Result<Vec<Item>> {
        let mut stmt = self
            .conn
            .prepare("SELECT payload FROM items WHERE type_ = ?1 AND deleted_time = 0 ORDER BY title COLLATE NOCASE")
            .map_err(|e| Error::Storage(e.to_string()))?;
        let rows = stmt
            .query_map([t], |r| r.get::<_, String>(0))
            .map_err(|e| Error::Storage(e.to_string()))?;
        let mut out = Vec::new();
        for row in rows {
            let payload: String = row.map_err(|e| Error::Storage(e.to_string()))?;
            let props: serde_json::Map<String, Value> = serde_json::from_str(&payload)
                .map_err(|e| Error::Storage(format!("payload parse: {e}")))?;
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
        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(|e| Error::Storage(e.to_string()))?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| Error::Storage(e.to_string()))?;
        let mut out = Vec::new();
        for row in rows {
            let payload: String = row.map_err(|e| Error::Storage(e.to_string()))?;
            let props: serde_json::Map<String, Value> = serde_json::from_str(&payload)
                .map_err(|e| Error::Storage(format!("payload parse: {e}")))?;
            let body = props
                .get("body")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
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
        let mut rows = stmt
            .query([note_id])
            .map_err(|e| Error::Storage(e.to_string()))?;
        if let Some(row) = rows.next().map_err(|e| Error::Storage(e.to_string()))? {
            let payload: String = row.get(0).map_err(|e| Error::Storage(e.to_string()))?;
            let props: serde_json::Map<String, Value> = serde_json::from_str(&payload)
                .map_err(|e| Error::Storage(format!("payload parse: {e}")))?;
            let body = props
                .get("body")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            return Ok(Some(Item {
                item_type: crate::items::TYPE_NOTE,
                props: props.into_iter().collect(),
                body,
            }));
        }
        Ok(None)
    }

    /// Fetch any item (notes/folders/...) by id, with its body for notes.
    pub fn item(&self, id: &str) -> Result<Option<Item>> {
        let mut stmt = self
            .conn
            .prepare("SELECT type_, payload FROM items WHERE id = ?1")
            .map_err(|e| Error::Storage(e.to_string()))?;
        let mut rows = stmt
            .query([id])
            .map_err(|e| Error::Storage(e.to_string()))?;
        if let Some(row) = rows.next().map_err(|e| Error::Storage(e.to_string()))? {
            let type_: i64 = row.get(0).map_err(|e| Error::Storage(e.to_string()))?;
            let payload: String = row.get(1).map_err(|e| Error::Storage(e.to_string()))?;
            return Self::item_from_payload(type_, &payload).map(Some);
        }
        Ok(None)
    }

    fn item_from_payload(item_type: i64, payload: &str) -> Result<Item> {
        let props: serde_json::Map<String, Value> = serde_json::from_str(payload)
            .map_err(|e| Error::Storage(format!("payload parse: {e}")))?;
        let body = props
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        Ok(Item {
            item_type,
            props: props.into_iter().collect(),
            body,
        })
    }

    /// Update a note's title and body, bumping `updated_time` to now so the
    /// item becomes dirty (`updated_time > sync_time`) and is uploaded on the
    /// next sync. The stored `sync_time` is deliberately left untouched.
    pub fn update_note(&self, id: &str, title: &str, body: &str) -> Result<()> {
        let Some(mut item) = self.note(id)? else {
            return Err(Error::Storage(format!("note not found: {id}")));
        };
        item.props
            .insert("title".to_string(), Value::String(title.to_string()));
        // Strictly increasing: guarantees the item is newer than its recorded
        // sync_time even if the edit lands in the same millisecond.
        let previous = item
            .props
            .get("updated_time")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let next = crate::time_ms().max(previous + 1);
        item.props
            .insert("updated_time".to_string(), Value::from(next));
        item.body = body.to_string();
        self.upsert_item(&item)
    }

    /// Move a note to another notebook, bumping `updated_time` so the change
    /// is uploaded on the next sync.
    pub fn move_note(&self, id: &str, folder_id: &str) -> Result<()> {
        let Some(mut item) = self.note(id)? else {
            return Err(Error::Storage(format!("note not found: {id}")));
        };
        item.props.insert(
            "parent_id".to_string(),
            Value::String(folder_id.to_string()),
        );
        let previous = item
            .props
            .get("updated_time")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let next = crate::time_ms().max(previous + 1);
        item.props
            .insert("updated_time".to_string(), Value::from(next));
        self.upsert_item(&item)
    }

    /// Create a new (empty) note in a folder and return its id.
    pub fn create_note(&self, folder_id: &str, title: &str) -> Result<String> {
        let id = new_item_id();
        let now = crate::time_ms();
        let mut props = std::collections::BTreeMap::new();
        props.insert("id".to_string(), Value::String(id.clone()));
        props.insert(
            "parent_id".to_string(),
            Value::String(folder_id.to_string()),
        );
        props.insert("title".to_string(), Value::String(title.to_string()));
        props.insert("created_time".to_string(), Value::from(now));
        props.insert("updated_time".to_string(), Value::from(now));
        props.insert("is_conflict".to_string(), Value::from(0));
        props.insert("is_todo".to_string(), Value::from(0));
        props.insert("markup_language".to_string(), Value::from(1));
        let item = Item {
            item_type: crate::items::TYPE_NOTE,
            props,
            body: String::new(),
        };
        self.upsert_item(&item)?;
        Ok(id)
    }

    /// Create a new notebook (folder) and return its id.
    pub fn create_folder(&self, title: &str) -> Result<String> {
        let id = new_item_id();
        let now = crate::time_ms();
        let mut props = std::collections::BTreeMap::new();
        props.insert("id".to_string(), Value::String(id.clone()));
        props.insert("title".to_string(), Value::String(title.to_string()));
        props.insert("created_time".to_string(), Value::from(now));
        props.insert("updated_time".to_string(), Value::from(now));
        let item = Item {
            item_type: crate::items::TYPE_FOLDER,
            props,
            body: String::new(),
        };
        self.upsert_item(&item)?;
        Ok(id)
    }

    /// Rename a notebook, bumping `updated_time` so it is re-uploaded.
    pub fn rename_folder(&self, id: &str, title: &str) -> Result<()> {
        let Some(mut item) = self.item(id)? else {
            return Err(Error::Storage(format!("folder not found: {id}")));
        };
        item.props
            .insert("title".to_string(), Value::String(title.to_string()));
        item.props
            .insert("updated_time".to_string(), Value::from(crate::time_ms()));
        self.upsert_item(&item)
    }

    /// Number of live notes directly inside a folder (used to block deleting a
    /// notebook that still contains notes).
    pub fn notes_in_folder(&self, folder_id: &str) -> Result<usize> {
        let n: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM items WHERE type_ = 1 AND parent_id = ?1 AND deleted_time = 0",
                [folder_id],
                |r| r.get(0),
            )
            .map_err(|e| Error::Storage(format!("notes_in_folder: {e}")))?;
        Ok(n as usize)
    }

    /// Locally-modified, decrypted items that must be uploaded: plaintext
    /// items whose `updated_time` is newer than the last recorded `sync_time`
    /// (or that were never synced). Mirrors Joplin's `itemsThatNeedSync`.
    pub fn items_needing_sync(&self) -> Result<Vec<Item>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT i.type_, i.payload FROM items i \
                 LEFT JOIN sync_items s ON s.item_id = i.id \
                 WHERE i.deleted_time = 0 AND i.encryption_applied = 0 \
                   AND i.updated_time > COALESCE(s.sync_time, 0) \
                 ORDER BY i.updated_time DESC",
            )
            .map_err(|e| Error::Storage(format!("items_needing_sync: {e}")))?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
            .map_err(|e| Error::Storage(format!("items_needing_sync: {e}")))?;
        let mut out = Vec::new();
        for row in rows {
            let (t, payload) = row.map_err(|e| Error::Storage(e.to_string()))?;
            out.push(Self::item_from_payload(t, &payload)?);
        }
        Ok(out)
    }

    pub fn resource(&self, resource_id: &str) -> Result<Option<Item>> {
        let mut stmt = self
            .conn
            .prepare("SELECT payload FROM items WHERE id = ?1 AND type_ = 4")
            .map_err(|e| Error::Storage(e.to_string()))?;
        let mut rows = stmt
            .query([resource_id])
            .map_err(|e| Error::Storage(e.to_string()))?;
        if let Some(row) = rows.next().map_err(|e| Error::Storage(e.to_string()))? {
            let payload: String = row.get(0).map_err(|e| Error::Storage(e.to_string()))?;
            let props: serde_json::Map<String, Value> = serde_json::from_str(&payload)
                .map_err(|e| Error::Storage(format!("payload parse: {e}")))?;
            return Ok(Some(Item {
                item_type: crate::items::TYPE_RESOURCE,
                props: props.into_iter().collect(),
                body: String::new(),
            }));
        }
        Ok(None)
    }

    /// Ids of items that are stored in encrypted form (no plaintext available).
    pub fn items_still_encrypted(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM items WHERE encryption_applied = 1")
            .map_err(|e| Error::Storage(e.to_string()))?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| Error::Storage(e.to_string()))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| Error::Storage(e.to_string()))?);
        }
        Ok(out)
    }

    /// Count items of a type (including deleted) - used to decide resource prefetch.
    pub fn items_count(&self, t: i64) -> Result<usize> {
        let mut stmt = self
            .conn
            .prepare("SELECT COUNT(*) FROM items WHERE type_ = ?1")
            .map_err(|e| Error::Storage(e.to_string()))?;
        let n: i64 = stmt
            .query_row([t], |r| r.get(0))
            .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(n as usize)
    }

    /// Full-note search over title/body. Whitespace-separated query words must
    /// all match (case-insensitive, AND). Optional filters: notebook id,
    /// tag title (any of the note's tags matches), todo-only, completed-only.
    /// Returns matches newest-first with a short snippet around the first hit.
    pub fn search_notes(&self, query: &str, opts: &SearchOptions) -> Result<Vec<SearchHit>> {
        let words: Vec<String> = query.split_whitespace().map(|w| w.to_lowercase()).collect();
        let mut sql =
            String::from("SELECT payload FROM items WHERE type_ = 1 AND deleted_time = 0");
        if let Some(folder) = &opts.folder_id {
            sql.push_str(&format!(
                " AND parent_id = '{}'",
                folder.replace('\'', "''")
            ));
        }
        if opts.todo_only {
            sql.push_str(" AND is_todo = 1");
        }
        if opts.completed_only {
            sql.push_str(" AND todo_completed > 0");
        }
        sql.push_str(" ORDER BY updated_time DESC LIMIT 500");

        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(|e| Error::Storage(e.to_string()))?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| Error::Storage(e.to_string()))?;

        // Tag filter: collect note ids having any matching tag title.
        let tag_note_ids: Option<std::collections::HashSet<String>> = if let Some(tag) = &opts.tag {
            let mut ids = std::collections::HashSet::new();
            // note_tag join: note_tag items carry note_id + tag_id; tag titles live in tag items.
            let mut stmt = self
                .conn
                .prepare("SELECT payload FROM items WHERE type_ = 5")
                .map_err(|e| Error::Storage(e.to_string()))?;
            let tag_rows = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .map_err(|e| Error::Storage(e.to_string()))?;
            let mut matching_tag_ids: Vec<String> = Vec::new();
            for row in tag_rows {
                let payload: String = row.map_err(|e| Error::Storage(e.to_string()))?;
                let v: Value = serde_json::from_str(&payload)
                    .map_err(|e| Error::Storage(format!("payload parse: {e}")))?;
                let title = v
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_lowercase();
                if title.contains(&tag.to_lowercase()) {
                    if let Some(id) = v.get("id").and_then(Value::as_str) {
                        matching_tag_ids.push(id.to_string());
                    }
                }
            }
            let mut stmt = self
                .conn
                .prepare("SELECT payload FROM items WHERE type_ = 6")
                .map_err(|e| Error::Storage(e.to_string()))?;
            let nt_rows = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .map_err(|e| Error::Storage(e.to_string()))?;
            for row in nt_rows {
                let payload: String = row.map_err(|e| Error::Storage(e.to_string()))?;
                let v: Value = serde_json::from_str(&payload)
                    .map_err(|e| Error::Storage(format!("payload parse: {e}")))?;
                let tag_id = v.get("tag_id").and_then(Value::as_str).unwrap_or_default();
                if matching_tag_ids.iter().any(|t| t == tag_id) {
                    if let Some(nid) = v.get("note_id").and_then(Value::as_str) {
                        ids.insert(nid.to_string());
                    }
                }
            }
            Some(ids)
        } else {
            None
        };

        let mut out = Vec::new();
        for row in rows {
            let payload: String = row.map_err(|e| Error::Storage(e.to_string()))?;
            let props: serde_json::Map<String, Value> = serde_json::from_str(&payload)
                .map_err(|e| Error::Storage(format!("payload parse: {e}")))?;
            if let Some(ids) = &tag_note_ids {
                let nid = props.get("id").and_then(Value::as_str).unwrap_or_default();
                if !ids.contains(nid) {
                    continue;
                }
            }
            let title = props
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let body = props
                .get("body")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let haystack = format!("{title}\n{body}").to_lowercase();
            if !words.is_empty() && !words.iter().all(|w| haystack.contains(w)) {
                continue;
            }
            // Snippet around the first matching word (about 70 chars context).
            let snippet = words
                .iter()
                .find_map(|w| {
                    haystack.find(w).map(|pos| {
                        let mut bs = pos.saturating_sub(30);
                        while bs > 0 && !haystack.is_char_boundary(bs) {
                            bs -= 1;
                        }
                        let mut be = (pos + 40).min(haystack.len());
                        while be < haystack.len() && !haystack.is_char_boundary(be) {
                            be += 1;
                        }
                        let mut snip = haystack[bs..be].replace('\n', " ");
                        if bs > 0 {
                            snip = format!("...{snip}");
                        }
                        if be < haystack.len() {
                            snip.push_str("...");
                        }
                        snip
                    })
                })
                .unwrap_or_else(|| body.chars().take(80).collect::<String>().replace('\n', " "));
            out.push(SearchHit {
                id: props
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                title: title.to_string(),
                snippet,
                is_todo: props.get("is_todo").and_then(Value::as_i64).unwrap_or(0) == 1,
                completed: props
                    .get("todo_completed")
                    .and_then(Value::as_i64)
                    .unwrap_or(0)
                    > 0,
                folder_id: props
                    .get("parent_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            });
            if out.len() >= 200 {
                break;
            }
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

    const NOTE_BLOB: &str =
        "Test note\n\nHello body\n\nid: 0f1e2d3c4b5a69788796a5b4c3d2e1f0\ntype_: 1";

    #[test]
    fn upsert_and_query() {
        let storage = Storage::open_in_memory().unwrap();
        let item = unserialize(NOTE_BLOB).unwrap();
        storage.upsert_item(&item).unwrap();
        storage
            .save_sync_time(item.props["id"].as_str().unwrap(), 123, 456)
            .unwrap();

        let notes = storage.notes("", true).unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].props["title"], Value::String("Test note".into()));
        assert_eq!(notes[0].body, "Hello body");

        let st = storage
            .sync_time("0f1e2d3c4b5a69788796a5b4c3d2e1f0")
            .unwrap()
            .unwrap();
        assert_eq!((st.0, st.1), (123, 456));

        assert_eq!(storage.all_item_ids().unwrap().len(), 1);
        storage
            .delete_items(&["0f1e2d3c4b5a69788796a5b4c3d2e1f0".to_string()])
            .unwrap();
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

    #[test]
    fn ui_recency_and_pending_count() {
        let storage = Storage::open_in_memory().unwrap();
        // Recency is empty until recorded, and scopes are isolated.
        assert_eq!(storage.last_opened_note(None).unwrap(), None);
        storage.set_last_opened_note(None, "n1").unwrap();
        storage.set_last_opened_note(Some("f1"), "n2").unwrap();
        assert_eq!(storage.last_opened_note(None).unwrap().as_deref(), Some("n1"));
        assert_eq!(
            storage.last_opened_note(Some("f1")).unwrap().as_deref(),
            Some("n2")
        );
        assert_eq!(storage.last_opened_note(Some("f2")).unwrap(), None);

        assert_eq!(storage.last_selected_folder().unwrap(), None);
        storage.set_last_selected_folder(Some("f1")).unwrap();
        assert_eq!(
            storage.last_selected_folder().unwrap().as_deref(),
            Some("f1")
        );
        storage.set_last_selected_folder(None).unwrap();
        assert_eq!(storage.last_selected_folder().unwrap(), None);

        // Pending count: fresh items count until synced; a pending delete too.
        assert_eq!(storage.local_pending_count().unwrap(), 0);
        let folder = storage.create_folder("F").unwrap();
        let note = storage.create_note(&folder, "N").unwrap();
        assert_eq!(storage.local_pending_count().unwrap(), 2);
        storage.save_sync_time(&folder, i64::MAX / 2, 0).unwrap();
        storage.save_sync_time(&note, i64::MAX / 2, 0).unwrap();
        assert_eq!(storage.local_pending_count().unwrap(), 0);
        storage.delete_item(&note).unwrap();
        assert_eq!(storage.local_pending_count().unwrap(), 1);
    }

    #[test]
    fn note_protection_markers_roundtrip() {
        let storage = Storage::open_in_memory().unwrap();
        assert!(storage.protected_note_ids().unwrap().is_empty());
        assert!(!storage.is_note_protected("n1").unwrap());

        storage.set_note_protected("n1").unwrap();
        storage.set_note_protected("n2").unwrap();
        // Setting twice is idempotent.
        storage.set_note_protected("n1").unwrap();
        assert!(storage.is_note_protected("n1").unwrap());
        let ids = storage.protected_note_ids().unwrap();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains("n1") && ids.contains("n2"));

        // Unrelated kv keys are not listed as protected notes.
        storage.kv_set("delta_context", "x").unwrap();
        assert_eq!(storage.protected_note_ids().unwrap().len(), 2);

        storage.unprotect_note("n1").unwrap();
        assert!(!storage.is_note_protected("n1").unwrap());
        assert!(storage.is_note_protected("n2").unwrap());

        storage.clear_note_protections().unwrap();
        assert!(storage.protected_note_ids().unwrap().is_empty());
        // Unrelated kv survives the protection clear.
        assert_eq!(
            storage.kv_get("delta_context").unwrap().as_deref(),
            Some("x")
        );
    }

    #[test]
    fn update_note_marks_dirty_and_clears_on_sync_time() {
        let storage = Storage::open_in_memory().unwrap();
        let item = unserialize(NOTE_BLOB).unwrap();
        let id = item.props["id"].as_str().unwrap().to_string();
        storage.upsert_item(&item).unwrap();
        // Already synced: sync_time ahead of updated_time -> nothing to upload.
        storage.save_sync_time(&id, 1000, 123).unwrap();
        assert!(storage.items_needing_sync().unwrap().is_empty());

        storage.update_note(&id, "New title", "New body").unwrap();
        let dirty = storage.items_needing_sync().unwrap();
        assert_eq!(dirty.len(), 1);
        assert_eq!(dirty[0].props["id"].as_str(), Some(id.as_str()));
        assert_eq!(dirty[0].body, "New body");
        assert_eq!(dirty[0].props["title"].as_str(), Some("New title"));

        // Recording a fresh sync_time clears it again.
        storage.save_sync_time(&id, i64::MAX / 2, 0).unwrap();
        assert!(storage.items_needing_sync().unwrap().is_empty());
    }

    #[test]
    fn create_note_and_folder_roundtrip() {
        let storage = Storage::open_in_memory().unwrap();
        let folder = storage.create_folder("My Notebook").unwrap();
        assert_eq!(folder.len(), 32);
        assert!(folder.chars().all(|c| c.is_ascii_hexdigit()));
        let folders = storage.folders().unwrap();
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].props["title"].as_str(), Some("My Notebook"));

        let note = storage.create_note(&folder, "New note").unwrap();
        assert_eq!(note.len(), 32);
        assert_eq!(storage.notes_in_folder(&folder).unwrap(), 1);
        let n = storage.note(&note).unwrap().unwrap();
        assert_eq!(n.props["title"].as_str(), Some("New note"));
        assert_eq!(n.props["parent_id"].as_str(), Some(folder.as_str()));
        assert_eq!(n.body, "");
        // A freshly created note is dirty (never synced).
        assert!(storage
            .items_needing_sync()
            .unwrap()
            .iter()
            .any(|i| i.props["id"].as_str() == Some(note.as_str())));

        storage.rename_folder(&folder, "Renamed").unwrap();
        assert_eq!(
            storage.folders().unwrap()[0].props["title"].as_str(),
            Some("Renamed")
        );
    }

    #[test]
    fn delete_item_records_pending_and_frees_folder() {
        let storage = Storage::open_in_memory().unwrap();
        let folder = storage.create_folder("F").unwrap();
        let note = storage.create_note(&folder, "N").unwrap();
        assert_eq!(storage.notes_in_folder(&folder).unwrap(), 1);

        storage.delete_item(&note).unwrap();
        assert!(storage.note(&note).unwrap().is_none());
        assert_eq!(storage.notes_in_folder(&folder).unwrap(), 0);
        assert_eq!(storage.pending_deletes().unwrap(), vec![note.clone()]);

        storage.clear_pending_delete(&note).unwrap();
        assert!(storage.pending_deletes().unwrap().is_empty());

        // Deleting via the sync path (delete_items) does NOT record a pending
        // remote delete.
        storage.delete_item(&folder).unwrap();
        storage.clear_pending_delete(&folder).unwrap();
        storage.delete_items(std::slice::from_ref(&folder)).unwrap();
        assert!(storage.pending_deletes().unwrap().is_empty());
    }

    #[test]
    fn note_protection_survives_item_upsert_but_not_delete() {
        let storage = Storage::open_in_memory().unwrap();
        let item = unserialize(NOTE_BLOB).unwrap();
        let id = item.props["id"].as_str().unwrap();
        storage.upsert_item(&item).unwrap();
        storage.set_note_protected(id).unwrap();
        // A sync-driven re-upsert must not drop the local marker.
        storage.upsert_item(&item).unwrap();
        assert!(storage.is_note_protected(id).unwrap());
        // Deleting the note removes the marker.
        storage.delete_items(&[id.to_string()]).unwrap();
        assert!(!storage.is_note_protected(id).unwrap());
        assert!(storage.protected_note_ids().unwrap().is_empty());
    }

    fn seed_note(
        storage: &Storage,
        id: &str,
        title: &str,
        body: &str,
        folder: &str,
        todo: bool,
        done: bool,
    ) {
        let mut ser = format!(
            "{title}\n\n{body}\n\nid: {id}\ntype_: 1\nparent_id: {folder}\nupdated_time: 2026-01-01T00:00:00.000Z"
        );
        if todo {
            ser.push_str("\nis_todo: 1");
            if done {
                ser.push_str("\ntodo_completed: 1700000000000");
            }
        }
        storage.upsert_item(&unserialize(&ser).unwrap()).unwrap();
    }

    #[test]
    fn search_multi_word_and_filters() {
        let storage = Storage::open_in_memory().unwrap();
        seed_note(
            &storage,
            &"1".repeat(32),
            "Meeting notes",
            "Discussed the project roadmap",
            "f1",
            false,
            false,
        );
        seed_note(
            &storage,
            &"2".repeat(32),
            "Groceries",
            "buy apples and pears",
            "f1",
            false,
            false,
        );
        seed_note(
            &storage,
            &"3".repeat(32),
            "Todo A",
            "project meeting follow-up",
            "f2",
            true,
            false,
        );
        seed_note(
            &storage,
            &"4".repeat(32),
            "Todo done",
            "finished project meeting",
            "f2",
            true,
            true,
        );

        // Multi-word AND, case-insensitive. (3 notes have both words: two
        // todos' titles say "project meeting" and the first note's body.)
        let hits = storage
            .search_notes("PROJECT meeting", &SearchOptions::default())
            .unwrap();
        assert_eq!(hits.len(), 3, "three notes contain both words: {hits:?}");

        // Folder filter.
        let hits = storage
            .search_notes(
                "meeting",
                &SearchOptions {
                    folder_id: Some("f2".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(hits.len(), 2);

        // Todo filters.
        let hits = storage
            .search_notes(
                "meeting",
                &SearchOptions {
                    todo_only: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(hits.len(), 2);
        let hits = storage
            .search_notes(
                "meeting",
                &SearchOptions {
                    completed_only: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].completed);

        // Snippet contains the matched word with ellipses trimmed to context.
        let hits = storage
            .search_notes("roadmap", &SearchOptions::default())
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].snippet.contains("roadmap"));

        // No match -> empty.
        assert!(storage
            .search_notes("nonexistentword", &SearchOptions::default())
            .unwrap()
            .is_empty());
    }
}
