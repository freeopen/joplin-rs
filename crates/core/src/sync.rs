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
    pub errors: Vec<String>,
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
                    out.push_str(&String::from_utf16(&units).map_err(|_| Error::Crypto("bad utf16 chunk".into()))?);
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
        let mut outcome = SyncOutcome { downloaded: 0, deleted: 0, skipped_unsupported: 0, resources_downloaded: 0, errors: Vec::new() };

        progress("fetching info.json");
        let info = self.fetch_sync_info().await?;

        let key_material: Option<String> = if info.e2ee_enabled() {
            let pw = password.ok_or_else(|| Error::Sync("vault is encrypted: password required".into()))?;
            Some(self.unlock_master_key(&info, pw)?)
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
            } else {
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

            // Parse first to learn type_ before decryption (keepKeys make the
            // encrypted form parseable directly).
            let parsed = match items::unserialize(&text) {
                Ok(p) => p,
                Err(e) => {
                    outcome.errors.push(format!("{}: {}", stat.path, e));
                    continue;
                }
            };

            let final_item = if parsed.props.get("encryption_applied").and_then(Value::as_i64).unwrap_or(0) == 1 {
                match &key_material {
                    Some(km) => match Self::decrypt_item_blob(&text, km) {
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
                    },
                    None => {
                        outcome.skipped_unsupported += 1;
                        outcome.errors.push(format!("{}: encrypted but no password provided", stat.path));
                        parsed
                    }
                }
            } else {
                parsed
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

        // ---- resource prefetch ----
        if outcome.downloaded > 0 || self.storage.items_count(crate::items::TYPE_RESOURCE)? > 0 {
            progress("syncing resources");
            match self.sync_resources(key_material.as_deref(), progress).await {
                Ok(n) => outcome.resources_downloaded = n,
                Err(e) => outcome.errors.push(format!("resources: {e}")),
            }
        }

        Ok(outcome)
    }

    async fn sync_resources(&self, key_material: Option<&str>, progress: &mut dyn FnMut(&str)) -> Result<usize> {
        let mut downloaded = 0;
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
            if self.storage.resource_blob(&id)?.is_some() {
                continue; // already downloaded
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
                            continue;
                        }
                    },
                    None => continue,
                }
            } else {
                raw.into_bytes()
            };
            self.storage.save_resource_blob(&id, &data)?;
            downloaded += 1;
        }
        Ok(downloaded)
    }
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
