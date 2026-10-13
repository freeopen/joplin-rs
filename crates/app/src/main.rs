//! joplin-rs GUI: read-only note browser.
//!
//! Two coexisting frontends during the migration:
//! - default: the original egui/eframe UI (main.rs body below)
//! - `gpui-view`: the GPUI root view (view.rs) + widgets (ui/), launched
//!   by `gpui_main()` behind the feature flag

mod read_password;

#[cfg(feature = "gpui-view")]
mod app;
#[cfg(feature = "gpui-view")]
mod jobs;
#[cfg(feature = "gpui-view")]
mod ui;
#[cfg(feature = "gpui-view")]
mod view;

#[cfg(target_os = "macos")]
mod macos_about;

/// Human-facing version (see core::VERSION_DISPLAY), e.g. `v1.0.0 (b30d7ede3)`.
const APP_VERSION: &str = joplin_rs_core::VERSION_DISPLAY;

use joplin_rs_core::items::Item;
use joplin_rs_core::secret::LocalKey;
use joplin_rs_core::storage::Storage;
use read_password::{hash_password, verify_password};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Config {
    webdav_url: String,
    username: String,
    #[serde(default)]
    password: Option<String>,
    /// E2EE master key password (the in-app field overrides this)
    #[serde(default)]
    e2ee_password: Option<String>,
    /// Accept self-signed/invalid TLS certificates
    #[serde(default)]
    insecure_tls: bool,
    /// Path to a PEM CA certificate to trust
    #[serde(default)]
    ca_cert: Option<String>,
    /// PBKDF2 envelope of the global "reading password". It does not lock the
    /// app on launch; it gates notes the user marked as protected.
    #[serde(default)]
    reader_password_hash: Option<String>,
}

impl Config {
    fn tls_options(&self) -> joplin_rs_core::webdav::TlsOptions {
        joplin_rs_core::webdav::TlsOptions {
            insecure: self.insecure_tls,
            ca_path: self.ca_cert.as_ref().map(std::path::PathBuf::from),
        }
    }
}

/// Default base directory for config and data: ~/.joplin/
fn default_base_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".joplin")
}

fn config_path() -> PathBuf {
    default_base_dir().join("config.toml")
}

fn load_config() -> Config {
    let mut cfg: Config = std::fs::read_to_string(config_path())
        .ok()
        .and_then(|raw| toml::from_str(&raw).ok())
        .unwrap_or_default();
    // Decrypt secrets stored as `enc:v1:` by the CLI `set-password` command.
    // Legacy plaintext values pass through unchanged. The key file lives next
    // to the database, the same location the CLI uses. Only touch the key
    // file when there is actually a ciphertext to decrypt (don't create an
    // unused secret.key on every launch).
    let has_encrypted = cfg.password.as_deref().is_some_and(LocalKey::is_encrypted)
        || cfg
            .e2ee_password
            .as_deref()
            .is_some_and(LocalKey::is_encrypted);
    if has_encrypted {
        match LocalKey::load_or_create(&data_dir().join("secret.key")) {
            Ok(key) => decrypt_config_secrets(&mut cfg, &key),
            Err(e) => {
                tracing::error!("cannot load local secret key: {e}");
                cfg.password = None;
                cfg.e2ee_password = None;
            }
        }
    }
    cfg
}

/// Decrypt every `enc:v1:` secret in a config in place. A value that fails to
/// decrypt is dropped (sync then fails with an auth error rather than sending
/// ciphertext as the password).
fn decrypt_config_secrets(cfg: &mut Config, key: &LocalKey) {
    cfg.password = cfg
        .password
        .take()
        .and_then(|stored| match key.decrypt(&stored) {
            Ok(plain) => Some(plain),
            Err(e) => {
                tracing::error!("cannot decrypt webdav password from config.toml: {e}");
                None
            }
        });
    cfg.e2ee_password = cfg
        .e2ee_password
        .take()
        .and_then(|stored| match key.decrypt(&stored) {
            Ok(plain) => Some(plain),
            Err(e) => {
                tracing::error!("cannot decrypt e2ee password from config.toml: {e}");
                None
            }
        });
}

/// Update ONLY the reading-password hash in config.toml, preserving every
/// other key verbatim. This is critical for password security: the in-memory
/// Config holds DECRYPTED webdav/e2ee passwords, so serialising the whole
/// struct would silently downgrade the on-disk ciphertext back to plaintext.
///
/// The password gates notes marked as protected; it does not lock the app.
fn save_reader_password_hash(hash: &Option<String>) -> anyhow::Result<()> {
    let value = hash.as_ref().map(|h| h.as_str());
    write_config_string_key(&config_path(), "reader_password_hash", value)
}

/// Set (or remove when `value` is None) ONE string key in a TOML config file,
/// preserving every other key verbatim. Used for all app-side config writes:
/// the in-memory Config holds DECRYPTED webdav/e2ee passwords, so serialising
/// the whole struct would silently downgrade on-disk ciphertext to plaintext.
fn write_config_string_key(
    path: &std::path::Path,
    key_name: &str,
    value: Option<&str>,
) -> anyhow::Result<()> {
    use toml::Value;
    let mut table: toml::Table = if path.exists() {
        let raw = std::fs::read_to_string(path)?;
        raw.parse()?
    } else {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        toml::Table::new()
    };
    match value {
        Some(v) => {
            table.insert(key_name.to_owned(), Value::String(v.to_owned()));
        }
        None => {
            table.remove(key_name);
        }
    }
    let raw = toml::to_string_pretty(&table)?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, raw)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("JOPLIN_RS_DATA_DIR") {
        return PathBuf::from(dir);
    }
    default_base_dir()
}

/// Which modal dialog is currently open.
#[derive(Default, PartialEq, Eq)]
enum Dialog {
    #[default]
    None,
    /// Set / change / remove the reading password (used to open protected
    /// notes).
    ReaderPasswordSettings,
    /// Prompt for the E2EE master-key password after a sync downloaded items.
    E2eePrompt,
    /// App info: official icon, name, version. Non-macOS only: macOS uses the
    /// native system-menu About panel (see macos_about).
    #[cfg(not(target_os = "macos"))]
    About,
}

/// Stage-1 (password-less download) result handed from the worker thread.
#[derive(Clone)]
struct Stage1Summary {
    downloaded: usize,
    deleted: usize,
    resources: usize,
}

/// Stage-2 (E2EE decryption) result handed from the worker thread.
#[derive(Clone)]
struct DecryptSummary {
    items: usize,
    resources: usize,
    still_encrypted: usize,
}

/// One-shot result posted by the background sync/decrypt job.
enum JobEvent {
    /// Stage 1 (password-less download) finished. Carries the summary and the
    /// number of items still stored encrypted after it.
    SyncDone(Result<(Stage1Summary, usize), String>),
    /// Stage 2 (E2EE decryption) finished. An Err means the password was wrong
    /// or unsupported; placeholders remain in the list.
    DecryptDone(Result<DecryptSummary, String>),
}

struct App {
    storage: Storage,
    config: Config,
    // UI state
    folders: Vec<Item>,
    selected_folder: Option<String>,
    notes: Vec<Item>,
    selected_note: Option<String>,
    /// The selected note is stored as an encrypted placeholder (no plaintext).
    selected_note_encrypted: bool,
    note_body: String,
    note_title: String,
    search: String,
    /// Search-hit snippets keyed by note id (shown under the note title).
    search_hits: std::collections::HashMap<String, String>,
    /// E2EE password typed in the post-sync prompt. Session-only: never
    /// persisted by the GUI. Empty until the user is prompted (or a previous
    /// decryption supplied it).
    e2ee_session_password: String,
    status: String,
    /// True while either the download sync or the decryption pass runs.
    sync_running: Arc<Mutex<bool>>,
    status_shared: Arc<Mutex<String>>,
    /// One-shot event posted by the background job, consumed by update().
    job_event: Arc<Mutex<Option<JobEvent>>>,
    resource_dir: PathBuf,
    note_tags: std::collections::HashMap<String, Vec<String>>, // note_id -> tag titles

    // ---- reading password (per-note protection) ----
    /// Ids of notes marked as protected by the global reading password.
    /// Local-only (stored in the kv table, never synced).
    protected: std::collections::HashSet<String>,
    /// The currently selected note is showing its password gate instead of
    /// its body. Reset on every selection, so re-opening a note always asks.
    note_locked: bool,
    prot_input: String,
    prot_error: Option<String>,

    // ---- modal dialogs ----
    dialog: Dialog,
    // Reader-password settings dialog fields:
    rp_current: String,
    rp_new: String,
    rp_confirm: String,
    rp_message: Option<String>,
    rp_error: Option<String>,
    // E2EE prompt dialog fields:
    e2ee_input: String,
    e2ee_prompt_error: Option<String>,
    /// Number of encrypted items awaiting the password shown in the prompt.
    e2ee_pending_count: usize,

    // ---- about dialog (non-macOS; macOS uses native panel) ----
    /// Official Joplin icon texture for the About dialog (loaded lazily).
    #[cfg(not(target_os = "macos"))]
    about_icon: Option<egui::TextureHandle>,
}

impl App {
    fn new() -> Self {
        let storage = Storage::open(&data_dir().join("joplin-rs.db")).expect("open storage");
        let config: Config = load_config();
        let mut app = Self {
            storage,
            config: config.clone(),
            folders: Vec::new(),
            selected_folder: None,
            notes: Vec::new(),
            selected_note: None,
            selected_note_encrypted: false,
            note_body: String::new(),
            note_title: String::new(),
            search: String::new(),
            search_hits: Default::default(),
            e2ee_session_password: String::new(),
            status: "ready".into(),
            sync_running: Arc::new(Mutex::new(false)),
            status_shared: Arc::new(Mutex::new(String::new())),
            job_event: Arc::new(Mutex::new(None)),
            resource_dir: PathBuf::new(),
            note_tags: Default::default(),
            protected: Default::default(),
            note_locked: false,
            prot_input: String::new(),
            prot_error: None,
            dialog: Dialog::None,
            rp_current: String::new(),
            rp_new: String::new(),
            rp_confirm: String::new(),
            rp_message: None,
            rp_error: None,
            e2ee_input: String::new(),
            e2ee_prompt_error: None,
            e2ee_pending_count: 0,
            #[cfg(not(target_os = "macos"))]
            about_icon: None,
        };
        app.reload_meta();
        app
    }

    fn reload_meta(&mut self) {
        self.folders = self.storage.folders().unwrap_or_default();
        self.load_notes();
        let note_tags = self.storage.note_tags().unwrap_or_default();
        let tags = self.storage.tags().unwrap_or_default();
        let tag_titles: std::collections::HashMap<String, String> = tags
            .iter()
            .filter_map(|t| {
                let id = t.props.get("id")?.as_str()?.to_string();
                let title = t.props.get("title")?.as_str()?.to_string();
                Some((id, title))
            })
            .collect();
        let mut map: std::collections::HashMap<String, Vec<String>> = Default::default();
        for nt in note_tags {
            let note_id = nt
                .props
                .get("note_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            let tag_id = nt
                .props
                .get("tag_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            if let Some(title) = tag_titles.get(&tag_id) {
                map.entry(note_id).or_default().push(title.clone());
            }
        }
        self.note_tags = map;
        self.protected = self.storage.protected_note_ids().unwrap_or_default();

        // A decrypt pass may have just replaced the open placeholder with its
        // plaintext; refresh the open note so its body/lock state updates.
        // Route through the gate so a protected note re-locks after sync.
        if let Some(id) = self.selected_note.clone() {
            self.request_open_note(&id);
        }
    }

    fn load_notes(&mut self) {
        self.search_hits.clear();
        self.notes = if self.search.is_empty() {
            self.storage
                .notes(self.selected_folder.as_deref().unwrap_or(""), true)
                .unwrap_or_default()
        } else {
            // Search honours the selected notebook too.
            let opts = joplin_rs_core::storage::SearchOptions {
                folder_id: self.selected_folder.clone(),
                ..Default::default()
            };
            let hits = self
                .storage
                .search_notes(&self.search, &opts)
                .unwrap_or_default();
            // Map hits back to notes so the note list reuses the same rendering.
            self.search_hits = hits
                .iter()
                .map(|h| (h.id.clone(), h.snippet.clone()))
                .collect();
            hits.iter()
                .filter_map(|h| self.storage.note(&h.id).ok().flatten())
                .collect()
        };
    }

    fn open_note(&mut self, id: &str) {
        if let Ok(Some(note)) = self.storage.note(id) {
            self.selected_note_encrypted = is_encrypted_placeholder(&note);
            self.note_title = note
                .props
                .get("title")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string();
            let mut body = note.body.clone();
            // Rewrite Joplin resource links ":/id" to decrypted temp files.
            self.resource_dir = data_dir().join("resources");
            let _ = std::fs::create_dir_all(&self.resource_dir);
            let resource_ids = crate_resource_ids(&body);
            for rid in resource_ids {
                let mime = self.storage.resource(&rid).ok().flatten().and_then(|r| {
                    r.props
                        .get("mime")
                        .and_then(serde_json::Value::as_str)
                        .map(String::from)
                });
                let Some(data) = self.storage.resource_blob(&rid).ok().flatten() else {
                    continue;
                };
                let ext = match mime.as_deref() {
                    Some("image/png") => "png",
                    Some("image/jpeg") => "jpg",
                    Some("image/gif") => "gif",
                    Some("image/svg+xml") => "svg",
                    Some("image/webp") => "webp",
                    _ => continue, // non-image resources stay as links
                };
                let path = self.resource_dir.join(format!("{rid}.{ext}"));
                if std::fs::write(&path, &data).is_ok() {
                    body = body.replace(&format!(":/{rid}"), &path.to_string_lossy());
                }
            }
            self.note_body = body;
            self.selected_note = Some(id.to_string());
        }
    }

    // ---- reading password (per-note protection) ----

    fn has_reader_password(&self) -> bool {
        self.config.reader_password_hash.is_some()
    }

    /// Open a note, routing protected notes through the password gate. This
    /// is the only UI path that loads a note body, so every (re)selection of
    /// a protected note asks again; nothing is cached for the session.
    fn request_open_note(&mut self, id: &str) {
        if self.protected.contains(id) {
            // Read only the title for the gate header; body and resource
            // files stay unloaded until the password is verified.
            if let Ok(Some(note)) = self.storage.note(id) {
                self.note_title = note
                    .props
                    .get("title")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                self.selected_note_encrypted = is_encrypted_placeholder(&note);
            }
            self.selected_note = Some(id.to_string());
            self.note_locked = true;
            self.note_body.clear();
            self.prot_input.clear();
            self.prot_error = None;
            return;
        }
        self.note_locked = false;
        self.open_note(id);
    }

    /// Verify the gate input against the global reading-password hash and,
    /// on success, load the gated note.
    fn try_unlock_note(&mut self) {
        let Some(id) = self.selected_note.clone() else {
            return;
        };
        let Some(envelope) = self.config.reader_password_hash.as_deref() else {
            // No global password configured: protection is a no-op.
            self.note_locked = false;
            self.open_note(&id);
            return;
        };
        if verify_password(&self.prot_input, envelope) {
            self.prot_input.clear();
            self.prot_error = None;
            self.note_locked = false;
            self.open_note(&id);
        } else {
            self.prot_error = Some("密码错误，请重试".into());
            self.prot_input.clear();
        }
    }

    /// Remove protection from the gated note after verifying the password.
    fn unprotect_gated_note(&mut self) {
        let Some(id) = self.selected_note.clone() else {
            return;
        };
        let Some(envelope) = self.config.reader_password_hash.as_deref() else {
            // No global password: unprotect without prompting.
            let _ = self.storage.unprotect_note(&id);
            self.protected.remove(&id);
            self.note_locked = false;
            self.open_note(&id);
            return;
        };
        if !verify_password(&self.prot_input, envelope) {
            self.prot_error = Some("密码错误，请重试".into());
            self.prot_input.clear();
            return;
        }
        if let Err(e) = self.storage.unprotect_note(&id) {
            self.prot_error = Some(format!("取消保护失败: {e}"));
            return;
        }
        self.protected.remove(&id);
        self.prot_input.clear();
        self.prot_error = None;
        self.note_locked = false;
        self.open_note(&id);
    }

    /// Mark the open note as protected. No current password is required:
    /// adding a lock cannot reduce security. Without a global password set,
    /// open the password settings dialog instead.
    fn protect_open_note(&mut self) {
        if !self.has_reader_password() {
            self.open_reader_pw_settings();
            self.set_status("请先设置阅读密码，再保护笔记".into());
            return;
        }
        let Some(id) = self.selected_note.clone() else {
            return;
        };
        match self.storage.set_note_protected(&id) {
            Ok(()) => {
                self.protected.insert(id.clone());
                // Route through the gate immediately.
                self.request_open_note(&id);
            }
            Err(e) => self.set_status(format!("保护失败: {e}")),
        }
    }

    /// From an unlocked protected note, jump back to its gate (where the
    /// password can remove protection).
    fn gate_open_note(&mut self) {
        if let Some(id) = self.selected_note.clone() {
            self.request_open_note(&id);
        }
    }

    fn open_reader_pw_settings(&mut self) {
        self.rp_current.clear();
        self.rp_new.clear();
        self.rp_confirm.clear();
        self.rp_message = None;
        self.rp_error = None;
        self.dialog = Dialog::ReaderPasswordSettings;
    }

    /// Apply the set/change form: when no password is set the new field
    /// establishes one; when one is set the current password must match.
    fn submit_reader_pw(&mut self) {
        self.rp_error = None;
        self.rp_message = None;
        if let Some(envelope) = self.config.reader_password_hash.clone() {
            if !verify_password(&self.rp_current, &envelope) {
                self.rp_error = Some("当前密码错误".into());
                return;
            }
        }
        if self.rp_new.is_empty() {
            self.rp_error = Some("新密码不能为空".into());
            return;
        }
        if self.rp_new != self.rp_confirm {
            self.rp_error = Some("两次输入的新密码不一致".into());
            return;
        }
        let previous = self.config.reader_password_hash.clone();
        self.config.reader_password_hash = Some(hash_password(&self.rp_new));
        match save_reader_password_hash(&self.config.reader_password_hash) {
            Ok(()) => {
                self.rp_current.clear();
                self.rp_new.clear();
                self.rp_confirm.clear();
                self.rp_message =
                    Some("阅读密码已保存，对受保护的笔记立即生效".into());
            }
            Err(e) => {
                self.config.reader_password_hash = previous;
                self.rp_error = Some(format!("保存失败: {e}"));
            }
        }
    }

    /// Remove the global reading password (requires the current password).
    /// Protection markers become meaningless without it, so they are all
    /// cleared on success.
    fn remove_reader_pw(&mut self) {
        self.rp_error = None;
        self.rp_message = None;
        let Some(envelope) = self.config.reader_password_hash.clone() else {
            return;
        };
        if !verify_password(&self.rp_current, &envelope) {
            self.rp_error = Some("当前密码错误".into());
            return;
        }
        let previous = self.config.reader_password_hash.take();
        if let Err(e) = save_reader_password_hash(&None) {
            self.config.reader_password_hash = previous;
            self.rp_error = Some(format!("保存失败: {e}"));
            return;
        }
        if let Err(e) = self.storage.clear_note_protections() {
            // Password is already removed; report but do not roll it back.
            tracing::error!("cannot clear note protections: {e}");
        }
        self.protected.clear();
        self.rp_current.clear();
        self.rp_new.clear();
        self.rp_confirm.clear();
        self.rp_message = Some("阅读密码已取消，所有笔记保护已移除".into());
    }

    // ---- E2EE decryption (separate stage after download) ----

    /// Persisted/fallback E2EE password available without prompting: the
    /// decrypted config value, else the WebDAV password (users often reuse
    /// one). Used for the automatic post-sync decryption; the prompt is shown
    /// when none exists.
    fn stored_e2ee_password(&self) -> Option<String> {
        self.config
            .e2ee_password
            .clone()
            .filter(|p| !p.is_empty())
            .or_else(|| self.config.password.clone().filter(|p| !p.is_empty()))
    }

    /// Password to use for the decrypt pass: session prompt first, then the
    /// stored/fallback value.
    fn e2ee_password_for_decrypt(&self) -> Option<String> {
        if !self.e2ee_session_password.is_empty() {
            return Some(self.e2ee_session_password.clone());
        }
        self.stored_e2ee_password()
    }

    /// Sync button: always start with the password-less download pass. E2EE
    /// decryption is a separate step triggered after the download finishes.
    fn request_sync(&mut self, ctx: &egui::Context) {
        self.start_download(ctx);
    }

    /// Accept the E2EE prompt shown after a download and run decryption.
    fn submit_e2ee_prompt(&mut self, ctx: &egui::Context) {
        if self.e2ee_input.is_empty() {
            self.e2ee_prompt_error = Some("密码不能为空".into());
            return;
        }
        self.e2ee_session_password = self.e2ee_input.clone();
        self.e2ee_input.clear();
        self.e2ee_prompt_error = None;
        self.dialog = Dialog::None;
        self.start_decrypt(ctx);
    }

    /// Cancel the post-sync E2EE prompt: placeholders stay in the list.
    fn cancel_e2ee_prompt(&mut self) {
        self.e2ee_input.clear();
        self.e2ee_prompt_error = None;
        self.dialog = Dialog::None;
    }

    fn busy(&self) -> bool {
        *self.sync_running.lock().unwrap()
    }

    // ---- stage 1: password-less download sync ----

    fn start_download(&mut self, ctx: &egui::Context) {
        if self.busy() {
            return;
        }
        *self.sync_running.lock().unwrap() = true;
        *self.job_event.lock().unwrap() = None;
        *self.status_shared.lock().unwrap() = "开始同步…".into();
        self.status = "开始同步…".into();

        let config = self.config.clone();
        let data_dir = data_dir();
        let status = self.status_shared.clone();
        let running = self.sync_running.clone();
        let job_event = self.job_event.clone();
        let ctx = ctx.clone();

        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let result = rt.block_on(async move {
                let storage = Storage::open(&data_dir.join("joplin-rs.db"))?;
                let client = joplin_rs_core::webdav::WebDavClient::new_with_tls(
                    &config.webdav_url,
                    &config.username,
                    config.password.as_deref().unwrap_or(""),
                    &config.tls_options(),
                )?;
                let engine = joplin_rs_core::sync::SyncEngine::new(&client, &storage);
                // No E2EE password here: encrypted items download as
                // placeholders and are decrypted in the separate pass.
                let outcome = engine
                    .sync(None, &mut |msg| {
                        *status.lock().unwrap() = msg.to_string();
                    })
                    .await?;
                let pending = storage.items_still_encrypted().unwrap_or_default().len();
                Ok::<_, anyhow::Error>((
                    Stage1Summary {
                        downloaded: outcome.downloaded,
                        deleted: outcome.deleted,
                        resources: outcome.resources_downloaded,
                    },
                    pending,
                ))
            });
            *running.lock().unwrap() = false;
            // Post the event only after clearing the busy flag: the UI reacts
            // to this event by possibly starting the decrypt job, which is
            // skipped while busy() is still true.
            *job_event.lock().unwrap() =
                Some(JobEvent::SyncDone(result.map_err(|e| e.to_string())));
            ctx.request_repaint();
        });
    }

    // ---- stage 2: E2EE decryption ----

    fn start_decrypt(&mut self, ctx: &egui::Context) {
        if self.busy() {
            return;
        }
        let Some(password) = self.e2ee_password_for_decrypt() else {
            self.e2ee_input.clear();
            self.e2ee_prompt_error = None;
            self.dialog = Dialog::E2eePrompt;
            return;
        };
        *self.sync_running.lock().unwrap() = true;
        *self.job_event.lock().unwrap() = None;
        *self.status_shared.lock().unwrap() = "开始解密…".into();
        self.status = "开始解密…".into();

        let config = self.config.clone();
        let data_dir = data_dir();
        let status = self.status_shared.clone();
        let running = self.sync_running.clone();
        let job_event = self.job_event.clone();
        let ctx = ctx.clone();

        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let result = rt.block_on(async move {
                let storage = Storage::open(&data_dir.join("joplin-rs.db"))?;
                let client = joplin_rs_core::webdav::WebDavClient::new_with_tls(
                    &config.webdav_url,
                    &config.username,
                    config.password.as_deref().unwrap_or(""),
                    &config.tls_options(),
                )?;
                let engine = joplin_rs_core::sync::SyncEngine::new(&client, &storage);
                let info = engine.fetch_sync_info().await?;
                let out = engine
                    .decrypt_pending(&info, &password, &mut |msg| {
                        *status.lock().unwrap() = msg.to_string();
                    })
                    .await?;
                Ok::<_, anyhow::Error>(DecryptSummary {
                    items: out.items_decrypted,
                    resources: out.resources_decrypted,
                    still_encrypted: out.still_encrypted,
                })
            });
            *running.lock().unwrap() = false;
            *job_event.lock().unwrap() =
                Some(JobEvent::DecryptDone(result.map_err(|e| e.to_string())));
            ctx.request_repaint();
        });
    }

    /// Consume the background-job event on the UI thread.
    fn handle_job_event(&mut self, ctx: &egui::Context) {
        let event = self.job_event.lock().unwrap().take();
        let Some(event) = event else { return };
        match event {
            JobEvent::SyncDone(result) => match result {
                Ok((summary, pending)) => {
                    self.reload_meta();
                    self.e2ee_pending_count = pending;
                    let base = format!(
                        "同步完成：{} 项已下载，{} 项已删除，{} 个资源",
                        summary.downloaded, summary.deleted, summary.resources
                    );
                    if pending == 0 {
                        self.set_status(base);
                    } else if self.e2ee_password_for_decrypt().is_some() {
                        self.set_status(format!("{base}；正在解密 {pending} 项…"));
                        self.start_decrypt(ctx);
                    } else {
                        self.set_status(format!("{base}；{pending} 项已加密，等待 E2EE 密码"));
                        self.e2ee_input.clear();
                        self.e2ee_prompt_error = None;
                        self.dialog = Dialog::E2eePrompt;
                    }
                }
                Err(e) => self.set_status(format!("同步失败: {e}")),
            },
            JobEvent::DecryptDone(result) => {
                self.reload_meta();
                match result {
                    Ok(sum) => {
                        self.e2ee_pending_count = sum.still_encrypted;
                        if sum.still_encrypted == 0 {
                            self.set_status(format!(
                                "解密完成：{} 项、{} 个资源",
                                sum.items, sum.resources
                            ));
                        } else {
                            self.set_status(format!(
                                "已解密 {} 项，仍有 {} 项保持加密",
                                sum.items, sum.still_encrypted
                            ));
                        }
                    }
                    Err(e) => {
                        // Wrong/unsupported E2EE password: drop the session
                        // password and prompt again; placeholders remain.
                        let looks_wrong = looks_like_wrong_e2ee_password(&e);
                        self.e2ee_session_password.clear();
                        self.e2ee_input.clear();
                        self.set_status(format!("E2EE 解密失败: {e}"));
                        if self.e2ee_pending_count > 0 {
                            self.e2ee_prompt_error = Some(if looks_wrong {
                                "E2EE 密码错误，未解密笔记保持加密状态".into()
                            } else {
                                format!("解密失败: {e}")
                            });
                            self.dialog = Dialog::E2eePrompt;
                        }
                    }
                }
            }
        }
    }

    fn set_status(&mut self, text: String) {
        self.status = text.clone();
        *self.status_shared.lock().unwrap() = text;
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Consume any finished background sync/decrypt job before drawing.
        self.handle_job_event(ctx);

        // On macOS the About entry lives in the OS menu bar (next to the
        // Apple logo), wired up in macos_about. Other platforms show an
        // in-window app menu instead.
        #[cfg(not(target_os = "macos"))]
        egui::TopBottomPanel::top("menu_bar").show(ctx, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("joplin-rs-app", |ui| {
                    if ui.button("About joplin-rs-app").clicked() {
                        self.dialog = Dialog::About;
                        ui.close();
                    }
                });
            });
        });

        // Top bar
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let sync_label = if self.busy() { "Sync…" } else { "Sync" };
                let sync_clicked = ui
                    .add_enabled(!self.busy(), egui::Button::new(sync_label))
                    .clicked();
                if sync_clicked {
                    self.request_sync(ctx);
                }
                // Reflect live worker progress (download / decrypt status).
                let worker_status = self.status_shared.lock().unwrap().clone();
                if !worker_status.is_empty() {
                    self.status = worker_status;
                }
                if self.status.is_empty() {
                    self.status = "ready".into();
                }
                ui.label(&self.status);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("阅读密码").clicked() {
                        self.open_reader_pw_settings();
                    }
                    if ui.button("Reload").clicked() {
                        self.reload_meta();
                    }
                });
            });
        });

        // Search
        egui::TopBottomPanel::top("search").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("Search:");
                if ui.text_edit_singleline(&mut self.search).changed() {
                    self.load_notes();
                }
            });
        });

        // Left: folders + note list
        egui::SidePanel::left("left")
            .min_width(280.0)
            .show(ctx, |ui| {
                ui.heading("Notebooks");
                if ui
                    .selectable_label(self.selected_folder.is_none(), "All notes")
                    .clicked()
                {
                    self.selected_folder = None;
                    self.load_notes();
                }
                egui::ScrollArea::vertical()
                    .max_height(180.0)
                    .id_salt("folders")
                    .show(ui, |ui| {
                        let folders = self.folders.clone();
                        for folder in &folders {
                            let id = folder
                                .props
                                .get("id")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            let title = folder
                                .props
                                .get("title")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            if ui
                                .selectable_label(
                                    self.selected_folder.as_deref() == Some(id.as_str()),
                                    &title,
                                )
                                .clicked()
                            {
                                self.selected_folder = Some(id);
                                self.load_notes();
                            }
                        }
                    });
                ui.separator();
                ui.heading("Notes");
                egui::ScrollArea::vertical()
                    .auto_shrink(false)
                    .id_salt("notes")
                    .show(ui, |ui| {
                        let notes = self.notes.clone();
                        let note_tags = self.note_tags.clone();
                        let search_hits = self.search_hits.clone();
                        for note in &notes {
                            let id = note
                                .props
                                .get("id")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            let encrypted = is_encrypted_placeholder(note);
                            let protected = self.protected.contains(&id);
                            let raw_title = note
                                .props
                                .get("title")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            let title = if encrypted {
                                if raw_title.is_empty() {
                                    format!("🔒 已加密 {}", short_id(&id))
                                } else {
                                    format!("🔒 {raw_title}")
                                }
                            } else if raw_title.is_empty() {
                                "(no title)".to_string()
                            } else {
                                raw_title
                            };
                            // Reading-password-protected notes carry an 🔐
                            // marker (title stays visible, body stays gated).
                            let title = if protected {
                                format!("🔐 {title}")
                            } else {
                                title
                            };
                            let is_todo = note
                                .props
                                .get("is_todo")
                                .and_then(serde_json::Value::as_i64)
                                .unwrap_or(0)
                                == 1;
                            let done = note
                                .props
                                .get("todo_completed")
                                .and_then(serde_json::Value::as_i64)
                                .unwrap_or(0)
                                > 0;
                            let label = if is_todo {
                                format!("{} {}", if done { "\u{2611}" } else { "\u{2610}" }, title)
                            } else {
                                title.clone()
                            };
                            let selected = self.selected_note.as_deref() == Some(id.as_str());
                            if ui.selectable_label(selected, label).clicked() {
                                self.request_open_note(&id);
                            }
                            if let Some(tags) = note_tags.get(&id) {
                                if !tags.is_empty() {
                                    ui.horizontal(|ui| {
                                        for t in tags {
                                            ui.small(format!("#{t}"));
                                        }
                                    });
                                }
                            }
                            if let Some(snippet) = search_hits.get(&id) {
                                // Encrypted placeholders have no plaintext body;
                                // protected notes have a gated body: neither
                                // shows a search snippet (title still matches).
                                if !encrypted && !protected {
                                    ui.small(egui::RichText::new(snippet).weak().size(11.0));
                                }
                            }
                        }
                    });
            });

        // Right: rendered note
        egui::CentralPanel::default().show(ctx, |ui| {
            if self.selected_note.is_none() {
                ui.centered_and_justified(|ui| {
                    ui.weak("Select a note, then press Sync to download the vault.")
                });
                return;
            }
            // Reading-password gate for a protected note: title is shown,
            // but the body is not loaded until the password is verified.
            if self.note_locked {
                self.show_note_gate(ui, ctx);
                return;
            }
            // Encrypted placeholder selected: show its raw (locked) state rather
            // than rendering an empty/encrypted body.
            if self.selected_note_encrypted {
                self.show_encrypted_placeholder(ui);
                return;
            }
            // Title row with the protect/unprotect action.
            let id = self.selected_note.clone().unwrap_or_default();
            let is_protected = self.protected.contains(&id);
            ui.horizontal(|ui| {
                ui.heading(&self.note_title);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if is_protected {
                        if ui.button("取消保护").clicked() {
                            // Go back to the gate, where the password removes
                            // the marker.
                            self.gate_open_note();
                        }
                    } else if ui.button("🔒 保护此笔记").clicked() {
                        self.protect_open_note();
                    }
                });
            });
            ui.separator();
            egui::ScrollArea::vertical()
                .auto_shrink(false)
                .show(ui, |ui| {
                    let mut cache = egui_commonmark::CommonMarkCache::default();
                    egui_commonmark::CommonMarkViewer::new().show(ui, &mut cache, &self.note_body);
                });
        });

        // Modal dialogs (rendered last so they sit above all panels).
        match self.dialog {
            Dialog::None => {}
            Dialog::ReaderPasswordSettings => self.show_reader_pw_dialog(ctx),
            Dialog::E2eePrompt => self.show_e2ee_prompt_dialog(ctx),
            #[cfg(not(target_os = "macos"))]
            Dialog::About => self.show_about_dialog(ctx),
        }
    }
}

impl App {
    // ---- note panels ----

    /// Password gate shown in the body panel for a protected note. The title
    /// is visible; the body is not loaded until the global reading password
    /// is verified. Re-shown on every selection, including after sync.
    fn show_note_gate(&mut self, ui: &mut egui::Ui, _ctx: &egui::Context) {
        ui.horizontal(|ui| {
            ui.heading(&self.note_title);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.weak("🔐 受保护");
            });
        });
        ui.separator();
        ui.vertical_centered(|ui| {
            ui.add_space(40.0);
            ui.heading("🔐");
            ui.label("此笔记受阅读密码保护");
            ui.add_space(12.0);

            let input = egui::TextEdit::singleline(&mut self.prot_input)
                .password(true)
                .hint_text("阅读密码")
                .desired_width(300.0)
                .id(ui.make_persistent_id("note_gate_password"));
            let input_response = ui.add(input);
            // Same focus rule as a modal prompt: keep the field focused, but
            // do not steal it back on the Enter frame or the surrender /
            // lost_focus submit signal is eaten.
            let enter_pressed = ui.input(|i| i.key_pressed(egui::Key::Enter));
            if !input_response.has_focus() && !enter_pressed {
                input_response.request_focus();
            }
            let submit = enter_pressed
                && (input_response.lost_focus() || !self.prot_input.is_empty());

            if let Some(err) = &self.prot_error {
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(err).color(egui::Color32::from_rgb(200, 60, 60)),
                );
            }
            ui.add_space(10.0);
            let mut unlock = false;
            let mut unprotect = false;
            ui.horizontal(|ui| {
                if ui.button("取消保护").clicked() {
                    unprotect = true;
                }
                if ui.button("解锁查看").clicked() {
                    unlock = true;
                }
            });
            if unlock || submit {
                self.try_unlock_note();
            } else if unprotect {
                self.unprotect_gated_note();
            }
        });
    }

    /// E2EE placeholder body (note downloaded but not yet decrypted).
    fn show_encrypted_placeholder(&self, ui: &mut egui::Ui) {
        ui.heading(&self.note_title);
        ui.separator();
        ui.vertical_centered(|ui| {
            ui.add_space(40.0);
            ui.heading("🔒");
            ui.label("此笔记已端到端加密，尚未解密");
            ui.label("先完成同步，再提供正确的 E2EE 密码即可查看");
        });
    }

    fn show_reader_pw_dialog(&mut self, ctx: &egui::Context) {
        let has_pw = self.has_reader_password();
        let response =
            egui::Modal::new(egui::Id::new("reader_pw_settings_modal")).show(ctx, |ui| {
                ui.set_min_width(360.0);
                ui.heading(if has_pw {
                    "修改阅读密码"
                } else {
                    "设置阅读密码"
                });
                ui.add_space(4.0);
                ui.small(
                    egui::RichText::new(
                        "阅读密码不锁定整个应用，仅用于打开被标记为“🔐 受保护”的笔记；每次打开都需输入。",
                    )
                    .weak(),
                );
                ui.add_space(6.0);

                if has_pw {
                    ui.label("当前密码:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.rp_current)
                            .password(true)
                            .desired_width(320.0),
                    );
                    ui.add_space(6.0);
                }
                ui.label("新密码:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.rp_new)
                        .password(true)
                        .desired_width(320.0),
                );
                ui.add_space(6.0);
                ui.label("确认新密码:");
                let confirm_response = ui.add(
                    egui::TextEdit::singleline(&mut self.rp_confirm)
                        .password(true)
                        .desired_width(320.0),
                );
                let submit =
                    confirm_response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

                if let Some(err) = &self.rp_error {
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new(err).color(egui::Color32::from_rgb(200, 60, 60)));
                }
                if let Some(msg) = &self.rp_message {
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new(msg).color(egui::Color32::from_rgb(40, 140, 60)));
                }

                ui.add_space(10.0);
                let mut remove = false;
                let mut save = false;
                ui.horizontal(|ui| {
                    if has_pw && ui.button("取消密码（清除所有保护）").clicked() {
                        remove = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("关闭").clicked() {
                            self.dialog = Dialog::None;
                        }
                        if ui.button("保存").clicked() {
                            save = true;
                        }
                    });
                });
                if remove {
                    self.remove_reader_pw();
                } else if save || submit {
                    self.submit_reader_pw();
                }
            });
        if response.should_close() {
            self.dialog = Dialog::None;
        }
    }

    fn show_e2ee_prompt_dialog(&mut self, ctx: &egui::Context) {
        let pending = self.e2ee_pending_count;
        let response = egui::Modal::new(egui::Id::new("e2ee_prompt_modal")).show(ctx, |ui| {
            ui.set_min_width(380.0);
            ui.heading("E2EE 密码");
            ui.add_space(6.0);
            if pending > 0 {
                ui.label(format!(
                    "同步已完成，有 {pending} 项处于加密状态。输入 E2EE 主密钥密码后回车开始解密："
                ));
            } else {
                ui.label("输入 E2EE 主密钥密码后回车开始解密：");
            }
            ui.add_space(8.0);

            let input = egui::TextEdit::singleline(&mut self.e2ee_input)
                .password(true)
                .hint_text("E2EE master key password")
                .desired_width(340.0);
            let input_response = ui.add(input);
            // Same focus rule as the lock modal: never steal focus back on
            // the Enter frame or the surrender/lost_focus signal is eaten.
            let enter_pressed = ui.input(|i| i.key_pressed(egui::Key::Enter));
            if !input_response.has_focus() && !enter_pressed {
                input_response.request_focus();
            }
            let submit =
                enter_pressed && (input_response.lost_focus() || !self.e2ee_input.is_empty());

            if let Some(err) = &self.e2ee_prompt_error {
                ui.add_space(4.0);
                ui.label(egui::RichText::new(err).color(egui::Color32::from_rgb(200, 60, 60)));
            }
            ui.add_space(10.0);
            let mut cancelled = false;
            let mut confirmed = false;
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("取消（保持加密）").clicked() {
                        cancelled = true;
                    }
                    if ui.button("开始解密").clicked() {
                        confirmed = true;
                    }
                });
            });
            if cancelled {
                self.cancel_e2ee_prompt();
            } else if confirmed || submit {
                self.submit_e2ee_prompt(ctx);
            }
        });
        if response.should_close() {
            self.cancel_e2ee_prompt();
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn show_about_dialog(&mut self, ctx: &egui::Context) {
        let response = egui::Modal::new(egui::Id::new("about_modal")).show(ctx, |ui| {
            ui.set_min_width(340.0);
            ui.vertical_centered(|ui| {
                // Lazily decode the embedded official icon into a GPU texture.
                if self.about_icon.is_none() {
                    self.about_icon =
                        eframe::icon_data::from_png_bytes(APP_ICON_PNG)
                            .ok()
                            .map(|ic| {
                                let image = egui::ColorImage::from_rgba_unmultiplied(
                                    [ic.width as usize, ic.height as usize],
                                    &ic.rgba,
                                );
                                ctx.load_texture("about_icon", image, egui::TextureOptions::LINEAR)
                            });
                }
                if let Some(tex) = &self.about_icon {
                    ui.add(
                        egui::Image::from_texture(tex).fit_to_exact_size(egui::vec2(72.0, 72.0)),
                    );
                }
                ui.add_space(8.0);
                ui.heading("joplin-rs");
                ui.label(format!("版本 {APP_VERSION}"));
                ui.add_space(4.0);
                ui.label("只读的 Joplin 客户端（WebDAV + 端到端加密）");
                ui.small(
                    egui::RichText::new("应用图标为 Joplin 官方图标，版权归 Joplin 项目所有")
                        .weak(),
                );
                ui.add_space(10.0);
                if ui.button("关闭").clicked() {
                    self.dialog = Dialog::None;
                }
            });
        });
        if response.should_close() {
            self.dialog = Dialog::None;
        }
    }
}

/// Whether a sync error string indicates the E2EE password was wrong. Used to
/// decide whether the next Sync click must prompt for the password again.
fn looks_like_wrong_e2ee_password(err: &str) -> bool {
    err.contains("wrong password") || err.contains("no master key matched the password")
}

/// Whether a stored note is still an encrypted placeholder (`encryption_applied=1`)
/// downloaded by the password-less sync and not yet decrypted.
fn is_encrypted_placeholder(item: &Item) -> bool {
    item.props
        .get("encryption_applied")
        .and_then(serde_json::Value::as_i64)
        .is_some_and(|v| v == 1)
}

/// Short, human-friendly id tail used in the placeholder list label.
fn short_id(id: &str) -> String {
    let n = id.chars().count();
    if n > 8 {
        id.chars().skip(n - 8).collect()
    } else {
        id.to_string()
    }
}

/// Extract Joplin resource ids referenced as ":/<32-hex>" in a note body.
/// Operates on bytes only (the marker and hex digits are all ASCII), so it is
/// safe on UTF-8 text containing multibyte characters - slicing the string at
/// a fixed byte offset could land inside a CJK character and panic.
fn crate_resource_ids(body: &str) -> Vec<String> {
    const HEX: &[u8] = b"0123456789abcdefABCDEF";
    let bytes = body.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 34 <= bytes.len() {
        if bytes[i] == b':' && bytes[i + 1] == b'/' {
            let start = i + 2;
            let end = start + 32;
            if bytes[start..end].iter().all(|b| HEX.contains(b)) {
                // ASCII hex, so this byte range is a valid char boundary.
                let id = body[start..end].to_string();
                if !out.contains(&id) {
                    out.push(id);
                }
                i = end;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Locate a system font that covers Chinese/Japanese/Korean glyphs. egui's
/// bundled fonts are Latin-only, so without this CJK text renders as empty
/// boxes (tofu). Search the common per-OS font locations; the
/// `JOPLIN_RS_FONT` env var may point at a specific font file instead.
fn find_cjk_font() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("JOPLIN_RS_FONT") {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Some(path);
        }
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    let push = |v: &mut Vec<PathBuf>, p: &str| v.push(PathBuf::from(p));
    #[cfg(target_os = "macos")]
    {
        for p in [
            "/System/Library/Fonts/PingFang.ttc",
            "/System/Library/Fonts/Hiragino Sans GB.ttc",
            "/System/Library/Fonts/STHeiti Light.ttc",
            "/System/Library/Fonts/STHeiti Medium.ttc",
            "/Library/Fonts/Arial Unicode.ttf",
            "/System/Library/Fonts/Supplemental/Songti.ttc",
        ] {
            push(&mut candidates, p);
        }
    }
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        for p in [
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
            "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
            "/usr/share/fonts/opentype/noto/NotoSerifCJK-Regular.ttc",
            "/usr/share/fonts/truetype/arphic/uming.ttc",
        ] {
            push(&mut candidates, p);
        }
        if let Some(home) = dirs::home_dir() {
            candidates.push(home.join(".local/share/fonts/NotoSansCJK-Regular.ttc"));
        }
    }
    #[cfg(target_os = "windows")]
    {
        for p in [
            "C:\\Windows\\Fonts\\msyh.ttc",
            "C:\\Windows\\Fonts\\msyh.ttf",
            "C:\\Windows\\Fonts\\simhei.ttf",
            "C:\\Windows\\Fonts\\simsun.ttc",
        ] {
            push(&mut candidates, p);
        }
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// Configure fonts so Latin and CJK share ONE system font.
///
/// Mixing egui's bundled Latin font with a separate CJK fallback leaves the
/// two scripts on different baselines (the fonts' ascent/descent/line-gap
/// metrics differ). Because every candidate system CJK font also contains a
/// full Latin glyph set, we make it the PRIMARY font for both the
/// proportional and monospace families; egui's bundled fonts are pushed to
/// the end purely as a last-resort fallback. All glyphs in a mixed line then
/// come from one font and share one baseline.
fn configure_fonts(ctx: &egui::Context) {
    let Some(path) = find_cjk_font() else {
        eprintln!(
            "no CJK-capable system font found; Chinese/Japanese/Korean text may not render. \
             Set JOPLIN_RS_FONT to a .ttf/.ttc font path to override."
        );
        return;
    };
    let Ok(bytes) = std::fs::read(&path) else {
        eprintln!("failed to read font {}", path.display());
        return;
    };
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "system".to_owned(),
        Arc::new(egui::FontData::from_owned(bytes)),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        // System font first (so both Latin and CJK use it), bundled fonts
        // kept behind it for any glyph it is missing.
        let list = fonts.families.entry(family).or_default();
        list.insert(0, "system".to_owned());
    }
    ctx.set_fonts(fonts);
}

/// Official Joplin app icon (512x512 PNG), embedded for the window/taskbar
/// icon and the About dialog. Sourced from joplin-dev Assets/SquareIcon512.png.
const APP_ICON_PNG: &[u8] = include_bytes!("../assets/icon.png");

fn main() -> eframe::Result {
    let icon = eframe::icon_data::from_png_bytes(APP_ICON_PNG)
        .expect("embedded app icon must be a valid PNG");
    eframe::run_native(
        "joplin-rs",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([1100.0, 720.0])
                .with_icon(std::sync::Arc::new(icon)),
            ..Default::default()
        },
        Box::new(|cc| {
            configure_fonts(&cc.egui_ctx);
            // NSApplication and the winit system menu exist by now; retarget
            // the macOS app-menu About item and set the rounded dock icon.
            #[cfg(target_os = "macos")]
            macos_about::install_native_about();
            Ok(Box::new(App::new()))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_icon_decodes_to_square_rgba() {
        // Guards both the window icon and the About dialog texture: the
        // PNG must decode to square RGBA so ColorImage::from_rgba_unmultiplied
        // never panics on a length mismatch.
        let ic = eframe::icon_data::from_png_bytes(APP_ICON_PNG).expect("icon png decodes");
        assert_eq!(ic.width, ic.height);
        assert!(ic.width >= 64);
        assert_eq!(ic.rgba.len(), ic.width as usize * ic.height as usize * 4);
    }

    #[test]
    fn encrypted_placeholder_detection_and_short_id() {
        let plain = Item {
            item_type: 1,
            props: std::collections::BTreeMap::new(),
            body: "hello".into(),
        };
        assert!(!is_encrypted_placeholder(&plain));
        assert_eq!(short_id("0123456789abcdef"), "89abcdef");
        assert_eq!(short_id("abc"), "abc");

        let mut enc_props = std::collections::BTreeMap::new();
        enc_props.insert("encryption_applied".to_string(), serde_json::Value::from(1));
        let enc = Item {
            item_type: 1,
            props: enc_props,
            body: String::new(),
        };
        assert!(is_encrypted_placeholder(&enc));
    }

    const ID: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn extracts_resource_link() {
        let body = format!("before :/{ID} after");
        assert_eq!(crate_resource_ids(&body), vec![ID.to_string()]);
    }

    #[test]
    fn dedupes_repeated_links() {
        let body = format!(":/{ID} text :/{ID}");
        assert_eq!(crate_resource_ids(&body), vec![ID.to_string()]);
    }

    #[test]
    fn ignores_non_hex_and_short_ids() {
        assert!(crate_resource_ids("/not-a-link").is_empty());
        assert!(crate_resource_ids(&format!(":/{}short", "0".repeat(31))).is_empty());
        assert_eq!(crate_resource_ids(&format!(":/{ID}")), vec![ID.to_string()]);
    }

    /// Regression for the "end byte index is not a char boundary" panic:
    /// a non-link ":/" whose 32-byte look-ahead window ends inside a
    /// multibyte CJK character must not slice the string mid-character.
    #[test]
    fn nonlink_window_inside_cjk_does_not_panic() {
        // ":/" followed by 31 ASCII bytes then a 3-byte CJK char: the old
        // fixed-offset slice [i+2 .. i+34] ended on that char's first byte.
        let body = format!(":/{}交", "a".repeat(31));
        assert!(crate_resource_ids(&body).is_empty());

        // A colon+slash embedded in a URL/path right next to Chinese text.
        let body = "参考 https:/交运医药 结尾".to_string();
        assert!(crate_resource_ids(&body).is_empty());
    }

    /// Real Chinese note with a valid resource link: id must be extracted.
    #[test]
    fn extracts_link_from_chinese_body() {
        let body = format!("交运医药商保清分工作环境：![图](:/{ID})，后续中文交运医药");
        assert_eq!(crate_resource_ids(&body), vec![ID.to_string()]);

        let body = format!("开头中文 :/{ID}交");
        assert_eq!(crate_resource_ids(&body), vec![ID.to_string()]);

        // No link, only multibyte text: must not panic.
        assert!(crate_resource_ids("交运医药商保清分工作环境交运医药").is_empty());
    }

    // ---- encrypted config secrets (CLI set-password compatibility) ----

    use joplin_rs_core::secret::LocalKey;

    #[test]
    fn decrypts_encrypted_config_secrets() {
        let key = LocalKey::from_bytes([42u8; 32]);
        let mut cfg = Config {
            webdav_url: "https://x".into(),
            username: "u".into(),
            password: Some(key.encrypt("dav-pw").unwrap()),
            e2ee_password: Some(key.encrypt("e2ee-pw").unwrap()),
            insecure_tls: false,
            ca_cert: None,
            reader_password_hash: None,
        };
        decrypt_config_secrets(&mut cfg, &key);
        assert_eq!(cfg.password.as_deref(), Some("dav-pw"));
        assert_eq!(cfg.e2ee_password.as_deref(), Some("e2ee-pw"));
    }

    #[test]
    fn plaintext_and_missing_secrets_pass_through() {
        let key = LocalKey::from_bytes([42u8; 32]);
        let mut cfg = Config {
            password: Some("legacy-plain".into()),
            e2ee_password: None,
            ..Default::default()
        };
        decrypt_config_secrets(&mut cfg, &key);
        assert_eq!(cfg.password.as_deref(), Some("legacy-plain"));
        assert!(cfg.e2ee_password.is_none());
    }

    #[test]
    fn wrong_key_drops_secret_instead_of_passing_ciphertext() {
        let enc = LocalKey::from_bytes([1u8; 32]).encrypt("dav-pw").unwrap();
        let mut cfg = Config {
            password: Some(enc),
            ..Default::default()
        };
        decrypt_config_secrets(&mut cfg, &LocalKey::from_bytes([2u8; 32]));
        assert!(cfg.password.is_none());
    }

    /// App-side writes must never downgrade CLI-encrypted passwords to
    /// plaintext: only the one key is rewritten, every other line (including
    /// the `enc:v1:` password) is preserved byte-for-byte in value.
    #[test]
    fn app_write_preserves_encrypted_password() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let key = LocalKey::from_bytes([9u8; 32]);
        let enc_dav = key.encrypt("keep-me-secret").unwrap();
        let enc_e2ee = key.encrypt("e2ee-secret").unwrap();
        std::fs::write(
            &path,
            format!(
                "webdav_url = \"https://x/dav\"\nusername = \"u\"\npassword = \"{enc_dav}\"\ne2ee_password = \"{enc_e2ee}\"\n"
            ),
        )
        .unwrap();

        write_config_string_key(&path, "reader_password_hash", Some("hash-abc")).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains(&format!("password = \"{enc_dav}\"")),
            "webdav ciphertext must be untouched: {raw}"
        );
        assert!(raw.contains(&format!("e2ee_password = \"{enc_e2ee}\"")));
        assert!(raw.contains("reader_password_hash = \"hash-abc\""));
        assert!(
            !raw.contains("keep-me-secret"),
            "plaintext leaked into config.toml"
        );

        // Removing the reader hash also keeps the ciphertext intact.
        write_config_string_key(&path, "reader_password_hash", None).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("reader_password_hash"));
        assert!(raw.contains(&format!("password = \"{enc_dav}\"")));
    }
}
