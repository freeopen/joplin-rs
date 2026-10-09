//! Read-only sync engine for WebDAV targets.
//!
//! Flow (mirrors Synchronizer.start + basicDelta for the download path):
//! 1. GET info.json -> SyncInfo (version, e2ee, activeMasterKeyId, masterKeys[])
//! 2. Unlock the active master key with the password (KeyV1 or Custom/PPK)
//! 3. PROPFIND root -> basicDelta (timestamp-based context, page size 50)
//! 4. Download changed `{id}.md`, unserialize, decrypt if needed, upsert
//! 5. Deletion detection with a 90% failsafe
//! 6. Prefetch all resource blobs (.resource/{id}), decrypting FileV1 blobs

use crate::crypto as jcrypto;
use crate::error::{Error, Result};
use crate::items::{self};
use crate::storage::Storage;
use crate::webdav::{ItemStat, WebDavClient};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;

pub const FAILSAFE_THRESHOLD: f64 = 0.90;

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct MasterKeyEntity {
    pub id: String,
    #[serde(default)]
    pub created_time: i64,
    #[serde(default)]
    pub updated_time: i64,
    #[serde(default)]
    pub source_application: String,
    #[serde(default)]
    pub encryption_method: i64,
    #[serde(default)]
    pub checksum: String,
    #[serde(default)]
    pub content: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct SyncInfo {
    #[serde(default)]
    pub version: i64,
    #[serde(default)]
    pub e2ee: Option<Value>,
    #[serde(default, rename = "activeMasterKeyId")]
    pub active_master_key_id: Option<Value>,
    #[serde(default, rename = "masterKeys")]
    pub master_keys: Vec<MasterKeyEntity>,
    #[serde(default)]
    pub ppk: Option<Value>,
    #[serde(default, rename = "appMinVersion")]
    pub app_min_version: Option<String>,
}

impl SyncInfo {
    pub fn e2ee_enabled(&self) -> bool {
        self.e2ee.as_ref().and_then(|v| v.get("value")).and_then(Value::as_bool).unwrap_or(false)
    }

    pub fn active_master_key_id(&self) -> Option<String> {
        self.active_master_key_id.as_ref().and_then(|v| v.get("value")).and_then(Value::as_str).map(String::from)
    }
}

#[derive(Debug, Clone)]
pub struct SyncOutcome {
    pub downloaded: usize,
    pub deleted: usize,
    pub skipped_unsupported: usize,
    pub resources_downloaded: usize,
    /// Resources fetched but kept encrypted because no password was available.
    pub resources_pending: usize,
    pub errors: Vec<String>,
    /// Item ids stored in encrypted form that could not be decrypted (no
    /// password). They are retried on the next sync once a password is given.
    pub pending_decryption: Vec<String>,
}

/// Delta context persisted between syncs (basicDelta context).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DeltaContext {
    #[serde(default)]
    pub timestamp: i64,
    #[serde(default, rename = "filesAtTimestamp")]
    pub files_at_timestamp: Vec<String>,
    #[serde(default, rename = "deletedItemsProcessed")]
    pub deleted_items_processed: bool,
}

pub struct SyncEngine<'a> {
    pub client: &'a WebDavClient,
    pub storage: &'a Storage,
}

impl<'a> SyncEngine<'a> {
    pub fn new(client: &'a WebDavClient, storage: &'a Storage) -> Self {
        Self { client, storage }
    }

    pub async fn fetch_sync_info(&self) -> Result<SyncInfo> {
        let text = self.client.get_text("info.json").await?;
        match text {
            Some(t) if !t.trim().is_empty() => {
                let info: SyncInfo = serde_json::from_str(&t).map_err(|e| Error::Sync(format!("bad info.json: {e}")))?;
                if info.version == 0 {
                    return Err(Error::Sync("info.json missing version field".into()));
                }
                Ok(info)
            }
            _ => Ok(SyncInfo::default()),
        }
    }

    /// Unlock the active master key with a password. Returns the 512-char hex
    /// key material, or an error if the password is wrong.
    pub fn unlock_master_key(&self, info: &SyncInfo, password: &str) -> Result<String> {
        let key_id = info
            .active_master_key_id()
            .ok_or_else(|| Error::Sync("no active master key in sync info".into()))?;
        let mk = info
            .master_keys
            .iter()
            .find(|k| k.id == key_id)
            .ok_or_else(|| Error::Sync(format!("master key {key_id} not found in info.json")))?;
        match mk.encryption_method {
            jcrypto::METHOD_KEY_V1 => {
                let plain = jcrypto::native_decrypt(password, &mk.content, 220000)?;
                let hex_str = String::from_utf8(plain).map_err(|_| Error::Crypto("master key not utf8".into()))?;
                if hex_str.len() != 512 || !hex_str.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Err(Error::Crypto("decrypted master key has unexpected format".into()));
                }
                Ok(hex_str)
            }
            other => Err(Error::Unsupported(format!("master key encryption method {other} not supported (only password/KeyV1)"))),
        }
    }

    /// Decrypt an item blob (JED01 container) with the given key material.
    /// Returns the serialized item text.
    pub fn decrypt_item_blob(blob: &str, key_material_hex: &str) -> Result<String> {
        let header = jcrypto::decode_header(blob)?;
        let md_size = usize::from_str_radix(&blob[5..11], 16).map_err(|_| Error::Crypto("bad header size".into()))?;
        let chunks = jcrypto::decode_chunks(&blob[11 + md_size..])?;
        let mut out = String::new();
        for chunk in chunks {
            match header.method {
                jcrypto::METHOD_STRING_V1 => {
                    let plain = jcrypto::native_decrypt(key_material_hex, &chunk, 3)?;
                    let units: Vec<u16> = plain.chunks_exact(2).map(|p| u16::from_le_bytes([p[0], p[1]])).collect();
                    out.push_str(&decode_utf16_lossy_combine(&units));
                }
                other => return Err(Error::Unsupported(format!("item encryption method {other} not supported"))),
            }
        }
        Ok(out)
    }

    /// Decrypt a resource blob (FileV1) with the given key material. Returns raw bytes.
    pub fn decrypt_resource_blob(blob: &str, key_material_hex: &str) -> Result<Vec<u8>> {
        let header = jcrypto::decode_header(blob)?;
        let md_size = usize::from_str_radix(&blob[5..11], 16).map_err(|_| Error::Crypto("bad header size".into()))?;
        let chunks = jcrypto::decode_chunks(&blob[11 + md_size..])?;
        use base64::Engine;
        let mut raw = Vec::new();
        for chunk in chunks {
            match header.method {
                jcrypto::METHOD_FILE_V1 => {
                    let plain = jcrypto::native_decrypt(key_material_hex, &chunk, 3)?;
                    raw.extend_from_slice(&plain);
                }
                other => return Err(Error::Unsupported(format!("resource encryption method {other} not supported"))),
            }
        }
        base64::engine::general_purpose::STANDARD
            .decode(&raw)
            .map_err(|e| Error::Crypto(format!("resource base64: {e}")))
    }

    /// Run one sync pass. `password` is only needed for encrypted vaults.
    pub async fn sync(&self, password: Option<&str>, progress: &mut dyn FnMut(&str)) -> Result<SyncOutcome> {
        let mut outcome = SyncOutcome { downloaded: 0, deleted: 0, skipped_unsupported: 0, resources_downloaded: 0, resources_pending: 0, errors: Vec::new(), pending_decryption: Vec::new() };

        progress("fetching info.json");
        let info = self.fetch_sync_info().await?;

        // Encrypted vault without a password is allowed: items are stored in
        // encrypted form (pending_decryption) and retried on a later sync once
        // a password is provided. A WRONG password still fails hard.
        let key_material: Option<String> = if info.e2ee_enabled() {
            match password {
                Some(pw) => Some(self.unlock_master_key(&info, pw)?),
                None => None,
            }
        } else {
            None
        };

        // ---- basicDelta: list root and diff against context ----
        progress("listing remote items");
        let stats = self.client.list("").await?;
        let remote_files: Vec<&ItemStat> = stats.iter().filter(|s| !s.is_dir && items::is_system_path(&s.path)).collect();

        let context_json = self.storage.kv_get("delta_context")?.unwrap_or_default();
        let mut context: DeltaContext = if context_json.is_empty() {
            DeltaContext::default()
        } else {
            serde_json::from_str(&context_json).unwrap_or_default()
        };

        // Sort by updated_time (basicDelta requirement)
        let mut sorted: Vec<ItemStat> = remote_files.iter().map(|s| (*s).clone()).collect();
        sorted.sort_by_key(|s| s.updated_time);

        // Determine which remote items to download (timestamp-based diff).
        let mut to_download: Vec<ItemStat> = Vec::new();
        let mut new_timestamp = context.timestamp;
        let mut files_at_timestamp: Vec<String> = context.files_at_timestamp.clone();

        for stat in &sorted {
            if stat.updated_time < context.timestamp {
                continue;
            }
            if stat.updated_time == context.timestamp {
                if files_at_timestamp.contains(&stat.path) {
                    continue; // processed in a previous run at the exact same ms
                }
            } else if stat.updated_time > new_timestamp {
                new_timestamp = stat.updated_time;
                files_at_timestamp.clear();
            }
            files_at_timestamp.push(stat.path.clone());
            to_download.push(stat.clone());
        }

        // Deletion detection: items in local DB that no longer exist remotely.
        // (Only after the first sync; the failsafe guards against a wiped target.)
        let local_ids = self.storage.all_item_ids()?;
        let remote_paths: HashSet<String> = sorted.iter().map(|s| s.path.clone()).collect();
        if !local_ids.is_empty() {
            let deleted: Vec<String> = local_ids
                .iter()
                .filter(|id| {
                    let path = items::system_path(id);
                    !remote_paths.contains(&path)
                })
                .cloned()
                .collect();
            let percent_deleted = if local_ids.is_empty() { 0.0 } else { deleted.len() as f64 / local_ids.len() as f64 };
            if percent_deleted >= FAILSAFE_THRESHOLD {
                return Err(Error::Sync(format!(
                    "fail-safe: {:.0}% of items ({}) would be deleted - refusing. If this is intended, clear the local database.",
                    percent_deleted * 100.0,
                    deleted.len()
                )));
            }
            if !deleted.is_empty() {
                progress(&format!("deleting {} locally-removed items", deleted.len()));
                self.storage.delete_items(&deleted)?;
                outcome.deleted = deleted.len();
            }
        }

        // ---- download loop (paginated by OUTPUT_LIMIT like basicDelta) ----
        let total = to_download.len();
        for (done, stat) in to_download.iter().enumerate() {
            if done % 10 == 0 {
                progress(&format!("downloading {}/{}", done + 1, total));
            }
            let id = items::path_to_id(&stat.path);
            let text = match self.client.get_text(&stat.path).await? {
                Some(t) => t,
                None => {
                    outcome.errors.push(format!(" vanished mid-sync: {}", stat.path));
                    continue;
                }
            };

            // Joplin's serializeForSync keeps plaintext keys (id, type_, timestamps...)
            // and puts the encrypted container in encryption_cipher_text. But a
            // fully-encrypted blob (no plaintext props) also exists if the whole
            // serialized string was encrypted. Handle both:
            // 1. If the text starts with JED (fully encrypted), decrypt first.
            // 2. Otherwise parse, and decrypt encryption_cipher_text when present.
            // When no password is available the raw blob is stored with
            // `encryption_applied=1` so a later sync (with password) can retry.
            let final_item = if text.starts_with("JED") {
                let Some(km) = key_material.as_ref() else {
                    outcome.skipped_unsupported += 1;
                    outcome.pending_decryption.push(items::path_to_id(&stat.path));
                    // Store a minimal encrypted placeholder so the id is tracked.
                    let mut placeholder = items::unserialize(&format!(
                        "{}\n\nid: {}\nencryption_applied: 1\ntype_: 1",
                        "(encrypted)", items::path_to_id(&stat.path)
                    ))?;
                    placeholder.body = String::new();
                    if let Err(e) = self.storage.upsert_item(&placeholder) {
                        outcome.errors.push(format!("{}: store placeholder: {e}", stat.path));
                    }
                    continue;
                };
                match Self::decrypt_item_blob(&text, km) {
                    Ok(plain) => match items::unserialize(&plain) {
                        Ok(p) => p,
                        Err(e) => {
                            outcome.errors.push(format!("{}: decrypt parse: {e}", stat.path));
                            continue;
                        }
                    },
                    Err(Error::Unsupported(msg)) => {
                        outcome.skipped_unsupported += 1;
                        outcome.errors.push(format!("{}: {}", stat.path, msg));
                        continue;
                    }
                    Err(e) => {
                        outcome.errors.push(format!("{}: decrypt: {e}", stat.path));
                        continue;
                    }
                }
            } else {
                let parsed = match items::unserialize(&text) {
                    Ok(p) => p,
                    Err(e) => {
                        outcome.errors.push(format!("{}: {}", stat.path, e));
                        continue;
                    }
                };
                let cipher_text = parsed.props.get("encryption_cipher_text").and_then(Value::as_str).unwrap_or_default().to_string();
                if parsed.props.get("encryption_applied").and_then(Value::as_i64).unwrap_or(0) == 1 && !cipher_text.is_empty() {
                    match &key_material {
                        Some(km) => match Self::decrypt_item_blob(&cipher_text, km) {
                            Ok(plain) => match items::unserialize(&plain) {
                                Ok(mut p) => {
                                    // Restore the keepKeys that Joplin preserves on the outside.
                                    for keep_key in ["id", "type_", "updated_time", "parent_id", "note_id", "tag_id"] {
                                        if let Some(v) = parsed.props.get(keep_key) {
                                            p.props.entry(keep_key.to_string()).or_insert_with(|| v.clone());
                                        }
                                    }
                                    p
                                }
                                Err(e) => {
                                    outcome.errors.push(format!("{}: decrypt parse: {e}", stat.path));
                                    continue;
                                }
                            },
                            Err(Error::Unsupported(msg)) => {
                                outcome.skipped_unsupported += 1;
                                outcome.errors.push(format!("{}: {}", stat.path, msg));
                                continue;
                            }
                            Err(e) => {
                                outcome.errors.push(format!("{}: decrypt: {e}", stat.path));
                                continue;
                            }
                        },
                        None => {
                            outcome.skipped_unsupported += 1;
                            outcome.pending_decryption.push(items::path_to_id(&stat.path));
                            parsed
                        }
                    }
                } else {
                    parsed
                }
            };

            if let Err(e) = self.storage.upsert_item(&final_item) {
                outcome.errors.push(format!("{}: store: {e}", stat.path));
                continue;
            }
            self.storage.save_sync_time(&id, crate::time_ms(), stat.updated_time)?;
            outcome.downloaded += 1;
        }

        // Save the delta context.
        context.timestamp = new_timestamp;
        context.files_at_timestamp = files_at_timestamp;
        context.deleted_items_processed = false;
        self.storage.kv_set("delta_context", &serde_json::to_string(&context).unwrap_or_default())?;

        // ---- retry items that were stored encrypted (password now available) ----
        if let Some(km) = &key_material {
            let still_encrypted = self.storage.items_still_encrypted()?;
            if !still_encrypted.is_empty() {
                progress(&format!("retrying {} encrypted items", still_encrypted.len()));
                for id in still_encrypted {
                    let path = items::system_path(&id);
                    let Some(text) = self.client.get_text(&path).await? else { continue };
                    match Self::decrypt_item_blob(&text, km).and_then(|plain| items::unserialize(&plain)) {
                        Ok(item) => {
                            if let Err(e) = self.storage.upsert_item(&item) {
                                outcome.errors.push(format!("{path}: store: {e}"));
                            } else {
                                self.storage.save_sync_time(&id, crate::time_ms(), 0)?;
                                outcome.downloaded += 1;
                            }
                        }
                        Err(e) => outcome.errors.push(format!("{path}: retry: {e}")),
                    }
                }
            }
        }

        // ---- resource prefetch ----
        if outcome.downloaded > 0 || self.storage.items_count(crate::items::TYPE_RESOURCE)? > 0 {
            progress("syncing resources");
            match self.sync_resources(key_material.as_deref(), progress).await {
                Ok((n, pending, mut res_errors)) => {
                    outcome.resources_downloaded = n;
                    outcome.resources_pending = pending;
                    outcome.errors.append(&mut res_errors);
                }
                Err(e) => outcome.errors.push(format!("resources: {e}")),
            }
        }

        Ok(outcome)
    }

    async fn sync_resources(&self, key_material: Option<&str>, progress: &mut dyn FnMut(&str)) -> Result<(usize, usize, Vec<String>)> {
        let mut downloaded = 0;
        let mut pending = 0;
        let mut pending_errors = Vec::new();
        let resources = self.storage.items_by_type(crate::items::TYPE_RESOURCE)?;
        let total = resources.len();
        for (idx, res) in resources.iter().enumerate() {
            let id = res.props.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
            if id.is_empty() {
                continue;
            }
            if idx % 20 == 0 {
                progress(&format!("resources {}/{}", idx + 1, total));
            }
            // Skip only when a blob is cached AND its blob_updated_time hasn't
            // advanced since the last fetch.
            let cached = self.storage.resource_blob(&id)?.is_some();
            let blob_time = res.props.get("blob_updated_time").and_then(Value::as_i64).unwrap_or(0);
            let fetched_at = self.storage.resource_fetched_time(&id)?;
            if cached && fetched_at >= blob_time {
                continue; // already downloaded and fresh
            }
            let path = format!(".resource/{id}");
            let raw = match self.client.get_text(&path).await? {
                Some(t) => t,
                None => continue,
            };
            let encrypted = raw.starts_with("JED");
            let data = if encrypted {
                match key_material {
                    Some(km) => match Self::decrypt_resource_blob(&raw, km) {
                        Ok(d) => d,
                        Err(e) => {
                            tracing::warn!("resource {id}: {e}");
                            pending_errors.push(format!("resource {id}: {e}"));
                            continue;
                        }
                    },
                    None => {
                        pending += 1;
                        continue;
                    }
                }
            } else {
                raw.into_bytes()
            };
            self.storage.save_resource_blob(&id, &data)?;
            self.storage.set_resource_fetched_time(&id, crate::time_ms())?;
            downloaded += 1;
        }
        Ok((downloaded, pending, pending_errors))
    }
}

/// Decode UTF-16 code units like JS `new TextDecoder("utf-16le").decode`:
/// valid surrogate pairs combine; lone surrogates become U+FFFD (replacement),
/// and pairs split across chunk boundaries are still combined because each
/// chunk is decoded independently... they are not, so a pair split across two
/// chunks produces two replacement chars. Joplin has the same behaviour since
/// it decodes chunk-by-chunk too.
fn decode_utf16_lossy_combine(units: &[u16]) -> String {
    let mut out = String::with_capacity(units.len());
    let mut i = 0;
    while i < units.len() {
        let u = units[i];
        if (0xD800..0xDC00).contains(&u) && i + 1 < units.len() && (0xDC00..0xE000).contains(&units[i + 1]) {
            let cp = 0x10000 + ((u as u32 - 0xD800) << 10) + (units[i + 1] as u32 - 0xDC00);
            out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
            i += 2;
        } else if (0xD800..0xE000).contains(&u) {
            out.push('\u{FFFD}');
            i += 1;
        } else {
            out.push(char::from_u32(u as u32).unwrap_or('\u{FFFD}'));
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sync_info() {
        let json = r#"{
            "version": 3,
            "e2ee": { "value": true, "updatedTime": 1 },
            "activeMasterKeyId": { "value": "abc", "updatedTime": 1 },
            "masterKeys": [{ "id": "abc", "encryption_method": 8, "content": "{}" }],
            "ppk": { "value": null }
        }"#;
        let info: SyncInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.version, 3);
        assert!(info.e2ee_enabled());
        assert_eq!(info.active_master_key_id().unwrap(), "abc");
        assert_eq!(info.master_keys[0].encryption_method, 8);
    }

    #[test]
    fn sync_info_empty() {
        let info = SyncInfo::default();
        assert!(!info.e2ee_enabled());
        assert!(info.active_master_key_id().is_none());
    }
}
