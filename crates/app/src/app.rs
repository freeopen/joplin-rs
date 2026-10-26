//! Framework-agnostic application logic for the joplin-rs GUI.
//!
//! Split out of the former egui/eframe `main.rs` so the GPUI view layer owns
//! only presentation. Everything here is plain Rust: config, storage access,
//! the note-selection state machine, and the reading-password flows. The only
//! cross-thread machinery is the `JobChannel` posted by `jobs`.

use joplin_rs_core::items::Item;
use joplin_rs_core::secret::LocalKey;
use joplin_rs_core::storage::Storage;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::read_password::{hash_password, verify_password};
use crate::vim::VimMode;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    pub webdav_url: String,
    pub username: String,
    #[serde(default)]
    pub password: Option<String>,
    /// E2EE master key password (the in-app field overrides this)
    #[serde(default)]
    pub e2ee_password: Option<String>,
    /// Accept self-signed/invalid TLS certificates
    #[serde(default)]
    pub insecure_tls: bool,
    /// Path to a PEM CA certificate to trust
    #[serde(default)]
    pub ca_cert: Option<String>,
    /// PBKDF2 envelope of the global "reading password". It does not lock the
    /// app on launch; it gates notes the user marked as protected.
    #[serde(default)]
    pub reader_password_hash: Option<String>,
    /// Default the note editor to a minimal Vim keybinding layer. On by
    /// default; added after v1.0 so existing configs get Vim too.
    #[serde(default = "default_true")]
    pub vim_mode: bool,
    /// Run a sync when the GUI launches (checks the remote and transfers only
    /// what changed). On by default.
    #[serde(default = "default_true")]
    pub sync_on_startup: bool,
    /// Sync pending local changes before quitting the GUI. On by default.
    #[serde(default = "default_true")]
    pub sync_on_quit: bool,
}

/// serde default for booleans that should be `true` when the key is absent.
pub fn default_true() -> bool {
    true
}

impl Config {
    pub fn tls_options(&self) -> joplin_rs_core::webdav::TlsOptions {
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
pub fn save_reader_password_hash(hash: &Option<String>) -> anyhow::Result<()> {
    write_config_string_key(&config_path(), "reader_password_hash", hash.as_deref())
}

/// Render a plain string as a quoted TOML literal with minimal escaping.
fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Write a string value: `key = "value"` (or remove the line when `None`).
pub fn write_config_string_key(
    path: &std::path::Path,
    key: &str,
    value: Option<&str>,
) -> anyhow::Result<()> {
    let rendered = value.map(toml_string);
    write_config_raw_key(path, key, rendered.as_deref())
}

/// Rewrite one `key = <rendered>` line in config.toml, keeping every other
/// line byte-for-byte (the whole struct would silently downgrade on-disk
/// ciphertext to plaintext). `rendered` is the already-formatted TOML value
/// (e.g. `"text"` quoted, or a bare `true`); `None` removes the key.
pub fn write_config_raw_key(
    path: &std::path::Path,
    key: &str,
    rendered: Option<&str>,
) -> anyhow::Result<()> {
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    let mut replaced = false;
    let mut out = String::with_capacity(raw.len() + 64);
    let needle = format!("{key} =");
    for line in raw.lines() {
        if line.trim_start().starts_with(&needle) {
            replaced = true;
            if let Some(v) = rendered {
                out.push_str(&format!("{key} = {v}\n"));
            }
            // Removing the key: drop the line entirely.
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !replaced {
        if let Some(v) = rendered {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&format!("{key} = {v}\n"));
        }
    }
    std::fs::write(path, out)?;
    Ok(())
}

pub fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("JOPLIN_RS_DATA_DIR") {
        return PathBuf::from(dir);
    }
    default_base_dir()
}

/// Which modal dialog is currently open.
#[derive(Default, PartialEq, Eq, Clone, Copy, Debug)]
pub enum Dialog {
    #[default]
    None,
    /// Set / change / remove the reading password (used to open protected
    /// notes).
    ReaderPasswordSettings,
    /// Prompt for the E2EE master-key password after a sync downloaded items.
    E2eePrompt,
    /// Global search prompt (⌘⌥F): the query is typed into `search_query` and
    /// applied on Enter.
    Search,
    /// Create a new notebook (name typed into `nb_input`).
    NewNotebook,
    /// Rename the selected notebook.
    RenameNotebook,
    /// Confirm deleting the selected note.
    ConfirmDeleteNote,
    /// Confirm deleting the selected notebook (only when it has no notes).
    ConfirmDeleteNotebook,
    /// Application settings (connection, E2EE, Vim, sync toggles).
    Settings,
    /// App info: official icon, name, version. Non-macOS only: macOS uses the
    /// native system-menu About panel (see macos_about).
    #[cfg(not(target_os = "macos"))]
    About,
}

/// Stage-1 (password-less download) result handed from the worker thread.
#[derive(Clone)]
pub struct Stage1Summary {
    pub downloaded: usize,
    pub deleted: usize,
    pub resources: usize,
    /// Items uploaded to the target.
    pub uploaded: usize,
    /// Remote files physically deleted.
    pub deleted_remote: usize,
    /// Items skipped as conflicts (remote changed).
    pub conflicts: Vec<String>,
    /// Local changes that could not be uploaded (E2EE target, no password).
    pub upload_blocked: usize,
}

/// Stage-2 (E2EE decryption) result handed from the worker thread.
#[derive(Clone)]
pub struct DecryptSummary {
    pub items: usize,
    pub resources: usize,
    pub still_encrypted: usize,
}

/// One-shot result posted by the background sync/decrypt job. The worker
/// stores the latest event in a shared slot; the UI thread consumes it on its
/// next frame/poll. Framework agnostic: the GPUI layer decides when to check.
pub type JobSlot = Arc<Mutex<Option<JobEvent>>>;

pub enum JobEvent {
    /// Stage 1 (password-less download) finished. Carries the summary and the
    /// number of items still stored encrypted after it.
    SyncDone(Result<(Stage1Summary, usize), String>),
    /// Stage 2 (E2EE decryption) finished. An Err means the password was wrong
    /// or unsupported; placeholders remain in the list.
    DecryptDone(Result<DecryptSummary, String>),
}

/// Shared handles the background job needs, decoupled from any UI framework.
#[derive(Clone)]
pub struct JobChannel {
    pub running: Arc<Mutex<bool>>,
    pub status: Arc<Mutex<String>>,
    pub event: JobSlot,
}

impl JobChannel {
    pub fn new() -> Self {
        Self {
            running: Arc::new(Mutex::new(false)),
            status: Arc::new(Mutex::new(String::new())),
            event: Arc::new(Mutex::new(None)),
        }
    }

    pub fn is_busy(&self) -> bool {
        *self.running.lock().unwrap()
    }

    /// Live worker progress (download / decrypt status).
    pub fn worker_status(&self) -> String {
        self.status.lock().unwrap().clone()
    }

    /// Consume the latest job event, if any (one-shot).
    pub fn take_event(&self) -> Option<JobEvent> {
        self.event.lock().unwrap().take()
    }
}

impl Default for JobChannel {
    fn default() -> Self {
        Self::new()
    }
}

/// Framework-agnostic application state and note-selection state machine.
pub struct AppState {
    pub storage: Storage,
    pub config: Config,
    // UI state
    pub folders: Vec<Item>,
    pub selected_folder: Option<String>,
    pub notes: Vec<Item>,
    pub selected_note: Option<String>,
    /// The selected note is stored as an encrypted placeholder (no plaintext).
    pub selected_note_encrypted: bool,
    pub note_body: String,
    pub note_title: String,
    pub search: String,
    /// Search-hit snippets keyed by note id (shown under the note title).
    pub search_hits: std::collections::HashMap<String, String>,
    /// E2EE password typed in the post-sync prompt. Session-only: never
    /// persisted by the GUI.
    pub e2ee_session_password: String,
    pub status: String,
    /// Shared channel with background sync/decrypt jobs.
    pub jobs: JobChannel,
    pub resource_dir: PathBuf,
    pub note_tags: std::collections::HashMap<String, Vec<String>>, // note_id -> tag titles

    // ---- reading password (per-note protection) ----
    /// Ids of notes marked as protected by the global reading password.
    /// Local-only (stored in the kv table, never synced).
    pub protected: std::collections::HashSet<String>,
    /// The currently selected note is showing its password gate instead of
    /// its body. Reset on every selection, so re-opening a note always asks.
    pub note_locked: bool,
    pub prot_input: String,
    pub prot_error: Option<String>,

    // ---- modal dialogs ----
    pub dialog: Dialog,
    // Reader-password settings dialog fields:
    pub rp_current: String,
    pub rp_new: String,
    pub rp_confirm: String,
    pub rp_message: Option<String>,
    pub rp_error: Option<String>,
    /// E2EE prompt dialog fields:
    pub e2ee_input: String,
    pub e2ee_prompt_error: Option<String>,
    /// Number of encrypted items awaiting the password shown in the prompt.
    pub e2ee_pending_count: usize,
    /// Password the last-started decrypt job used. The auto-continue pass
    /// never restarts with this same value, which is what turned a wrong
    /// fallback password (e.g. the reused WebDAV password) into an endless
    /// fail -> prompt -> cancel -> retry loop.
    pub e2ee_job_password: String,
    /// Sticky "user declined" flag for the E2EE prompt. Set on cancel, so
    /// the periodic auto-decrypt pass stops re-opening the dialog; cleared
    /// by a fresh SyncDone or an explicit password submission.
    pub e2ee_cancelled: bool,
    /// Bumped every time a protected note re-locks (each `request_open_note`
    /// gate hit). The view layer clears the gate field when this changes,
    /// because the persistent GPUI `InputState` keeps typed text otherwise:
    /// re-opening a gated note would show and reuse the previous password.
    pub prot_gate_epoch: u64,

    // ---- note editing ----
    /// Note panel is in edit mode (title + body editable).
    pub editing: bool,
    /// Working copy mirrored from the edit inputs.
    pub edit_title: String,
    pub edit_body: String,
    /// The working copy differs from what is stored (needs saving).
    pub edit_dirty: bool,
    /// Millisecond timestamp of the last edit change (autosave debounce).
    pub edit_changed_ms: i64,
    /// Raw note body (Joplin `:/id` resource links intact) for the editor.
    pub note_raw_body: String,
    /// Bumped when edit mode (re)starts or the working copy is reseeded, so
    /// the view can push `edit_title`/`edit_body` into the persistent inputs.
    pub edit_epoch: u64,

    // ---- list keyboard navigation ----
    /// Keyboard cursor in the notebooks column: 0 = "All notes", i+1 = folders[i].
    pub folder_cursor: usize,
    /// Keyboard cursor in the notes column (index into `notes`).
    pub note_cursor: usize,

    // ---- vim editing ----
    /// Current Vim mode. Insert is the plain-editing mode; Normal and Visual
    /// only occur while editing with `config.vim_mode` on.
    pub vim_mode: VimMode,
    /// Pending operator/prefix for the Vim layer ("d", "y", "g", ":", ...).
    pub vim_pending: String,

    // ---- sync lifecycle ----
    /// Local changes waiting to be uploaded, mirrored each UI tick.
    pub local_pending: usize,
    /// A quit is waiting for an in-flight sync-before-quit to finish.
    pub quit_after_sync: bool,

    // ---- settings dialog staging ----
    pub set_webdav_url: String,
    pub set_username: String,
    pub set_password: String,
    pub set_e2ee_password: String,
    pub set_insecure_tls: bool,
    pub set_ca_cert: String,
    pub set_vim_mode: bool,
    pub set_sync_on_startup: bool,
    pub set_sync_on_quit: bool,
    pub set_message: Option<String>,
    pub set_error: Option<String>,

    // ---- list layout toggles ----
    pub show_notebooks: bool,
    pub show_notes: bool,

    // ---- notebook create/rename dialog ----
    pub nb_input: String,
    pub nb_error: Option<String>,
    /// Id targeted by the pending confirm-delete dialog.
    pub confirm_target: Option<String>,
    /// Title of the note pending deletion (used in the dialog description).
    pub confirm_note_title: String,
    /// Id targeted by the pending rename dialog.
    pub rename_target: Option<String>,
}

impl AppState {
    pub fn new() -> anyhow::Result<Self> {
        let storage = Storage::open(&data_dir().join("joplin-rs.db"))?;
        let config = load_config();
        Self::build(storage, config)
    }

    /// Single constructor body shared by [`Self::new`] and the test wiring:
    /// keeps the field list in one place.
    fn build(storage: Storage, config: Config) -> anyhow::Result<Self> {
        Ok(Self {
            storage,
            config,
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
            jobs: JobChannel::new(),
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
            e2ee_job_password: String::new(),
            e2ee_cancelled: false,
            prot_gate_epoch: 0,
            editing: false,
            edit_title: String::new(),
            edit_body: String::new(),
            edit_dirty: false,
            edit_changed_ms: 0,
            note_raw_body: String::new(),
            edit_epoch: 0,
            folder_cursor: 0,
            note_cursor: 0,
            vim_mode: VimMode::Insert,
            vim_pending: String::new(),
            local_pending: 0,
            quit_after_sync: false,
            set_webdav_url: String::new(),
            set_username: String::new(),
            set_password: String::new(),
            set_e2ee_password: String::new(),
            set_insecure_tls: false,
            set_ca_cert: String::new(),
            set_vim_mode: true,
            set_sync_on_startup: true,
            set_sync_on_quit: true,
            set_message: None,
            set_error: None,
            show_notebooks: true,
            show_notes: true,
            nb_input: String::new(),
            nb_error: None,
            confirm_target: None,
            confirm_note_title: String::new(),
            rename_target: None,
        })
    }

    /// Reload the folder/note lists, tag map and protection markers without
    /// touching the currently open note. Used after an inline save.
    pub fn refresh_lists(&mut self) {
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
        self.sync_cursors_to_selection();
    }

    pub fn reload_meta(&mut self) {
        self.refresh_lists();

        // A decrypt pass or a sync may have replaced the open note; refresh it
        // so its body/lock state updates. An in-progress edit is left alone so
        // the user does not lose unsaved text.
        if self.editing {
            return;
        }
        if let Some(id) = self.selected_note.clone() {
            // Route through the gate so a protected note re-locks after sync.
            self.request_open_note(&id);
        }
    }

    /// Apply a global search query from the search dialog (⌘⌥F). A blank query
    /// clears the filter (showing the selected notebook's notes). Runs the
    /// search immediately and closes the dialog.
    pub fn apply_search(&mut self, query: &str) {
        self.end_edit();
        self.search = query.trim().to_string();
        self.load_notes();
        self.dialog = Dialog::None;
    }

    pub fn load_notes(&mut self) {
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

    // ---- list selection & keyboard navigation ---------------------------

    /// Commit the notebooks-column cursor: select that notebook (or
    /// "All notes" at index 0), load its notes and open its recent note.
    pub fn select_folder_at_cursor(&mut self) {
        let folder = if self.folder_cursor == 0 {
            None
        } else {
            self.folders.get(self.folder_cursor - 1).and_then(|f| {
                f.props
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .map(String::from)
            })
        };
        self.select_folder(folder);
    }

    /// Select a notebook (`None` = All notes): persist the choice, reload the
    /// note list, and open the scope's most recent note.
    pub fn select_folder(&mut self, folder: Option<String>) {
        self.end_edit();
        let _ = self.storage.set_last_selected_folder(folder.as_deref());
        self.selected_folder = folder;
        self.load_notes();
        let scope = self.selected_folder.clone();
        self.open_recent_in_folder(scope.as_deref());
    }

    /// Clamp the keyboard cursors to the current selection/lists.
    pub fn sync_cursors_to_selection(&mut self) {
        self.folder_cursor = match self.selected_folder.as_deref() {
            None => 0,
            Some(id) => self
                .folders
                .iter()
                .position(|f| f.props.get("id").and_then(serde_json::Value::as_str) == Some(id))
                .map(|i| i + 1)
                .unwrap_or(0),
        };
        self.note_cursor = self
            .selected_note
            .as_deref()
            .and_then(|id| {
                self.notes
                    .iter()
                    .position(|n| n.props.get("id").and_then(serde_json::Value::as_str) == Some(id))
            })
            .unwrap_or(0);
    }

    /// Move the notebooks-column cursor by `delta` (0 = "All notes").
    pub fn move_folder_cursor(&mut self, delta: i32) {
        self.folder_cursor = clamp_step(self.folder_cursor, delta, self.folders.len());
    }

    /// Move the notes-column cursor by `delta`.
    pub fn move_note_cursor(&mut self, delta: i32) {
        if self.notes.is_empty() {
            self.note_cursor = 0;
            return;
        }
        self.note_cursor = clamp_step(self.note_cursor, delta, self.notes.len() - 1);
    }

    /// Open the note under the notes-column cursor.
    pub fn open_note_at_cursor(&mut self) {
        let Some(id) = self
            .notes
            .get(self.note_cursor)
            .and_then(|n| n.props.get("id").and_then(serde_json::Value::as_str))
            .map(String::from)
        else {
            return;
        };
        self.end_edit();
        self.request_open_note(&id);
    }

    /// Open a scope's most recently opened note, falling back to the newest
    /// note in the scope (the list is already ordered newest-first).
    pub fn open_recent_in_folder(&mut self, folder: Option<&str>) {
        let recent = self.storage.last_opened_note(folder).ok().flatten();
        let target = recent
            .filter(|id| {
                self.notes.iter().any(|n| {
                    n.props.get("id").and_then(serde_json::Value::as_str) == Some(id.as_str())
                })
            })
            .or_else(|| {
                self.notes.first().and_then(|n| {
                    n.props
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .map(String::from)
                })
            });
        if let Some(id) = target {
            self.request_open_note(&id);
        } else {
            // Empty scope: clear the open note so a stale selection never
            // lingers after switching notebooks or deleting the last note.
            self.selected_note = None;
            self.note_title.clear();
            self.note_body.clear();
            self.note_raw_body.clear();
            self.selected_note_encrypted = false;
            self.note_locked = false;
            self.editing = false;
        }
        self.sync_cursors_to_selection();
    }

    /// Restore the last session on launch: the last selected notebook (if it
    /// still exists) and that scope's most recent note.
    pub fn restore_last_session(&mut self) {
        let last_folder = self
            .storage
            .last_selected_folder()
            .ok()
            .flatten()
            .filter(|id| {
                self.folders.iter().any(|f| {
                    f.props.get("id").and_then(serde_json::Value::as_str) == Some(id.as_str())
                })
            });
        self.selected_folder = last_folder;
        self.load_notes();
        let scope = self.selected_folder.clone();
        self.open_recent_in_folder(scope.as_deref());
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
            // Raw body (Joplin `:/id` links intact) is what the editor edits;
            // `note_body` below becomes the rendered version.
            self.note_raw_body = note.body.clone();
            self.editing = false;
            self.edit_dirty = false;
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
            self.record_recent_note(id, &note);
        }
    }

    /// Persist the note as the most-recently-opened one, globally and in its
    /// own notebook, so launch / notebook clicks can restore it.
    fn record_recent_note(&self, id: &str, note: &Item) {
        let _ = self.storage.set_last_opened_note(None, id);
        if let Some(parent) = note
            .props
            .get("parent_id")
            .and_then(serde_json::Value::as_str)
            .filter(|p| !p.is_empty())
        {
            let _ = self.storage.set_last_opened_note(Some(parent), id);
        }
    }

    // ---- note editing ---------------------------------------------------

    /// Whether the open note can be edited (note selected, not locked, not an
    /// encrypted placeholder).
    pub fn can_edit(&self) -> bool {
        self.selected_note.is_some() && !self.note_locked && !self.selected_note_encrypted
    }

    /// Enter edit mode for the open note, seeding the working copy.
    pub fn begin_edit(&mut self) {
        if !self.can_edit() {
            return;
        }
        self.edit_title = self.note_title.clone();
        self.edit_body = self.note_raw_body.clone();
        self.edit_dirty = false;
        self.editing = true;
        // Vim editing starts in Normal mode when enabled (like vim itself).
        self.vim_mode = if self.config.vim_mode {
            VimMode::Normal
        } else {
            VimMode::Insert
        };
        self.vim_pending.clear();
        self.edit_epoch = self.edit_epoch.wrapping_add(1);
    }

    /// Leave edit mode, saving any pending change first.
    pub fn end_edit(&mut self) {
        self.save_edit();
        self.editing = false;
        self.vim_mode = VimMode::Insert;
        self.vim_pending.clear();
    }

    /// Mirror a change in the working copy and arm the autosave debounce.
    pub fn mark_edit_dirty(&mut self) {
        if !self.editing {
            return;
        }
        self.edit_dirty = true;
        self.edit_changed_ms = joplin_rs_core::time_ms();
    }

    /// Persist the working copy if it changed. Returns true on a write.
    pub fn save_edit(&mut self) -> bool {
        if !self.editing || !self.edit_dirty {
            return false;
        }
        let Some(id) = self.selected_note.clone() else {
            return false;
        };
        match self
            .storage
            .update_note(&id, &self.edit_title, &self.edit_body)
        {
            Ok(()) => {
                self.edit_dirty = false;
                // Refresh the rendered preview (resource links re-resolved);
                // open_note clears edit mode, so restore it afterwards.
                self.open_note(&id);
                self.editing = true;
                self.refresh_lists();
                true
            }
            Err(e) => {
                self.set_status(format!("保存失败: {e}"));
                false
            }
        }
    }

    /// Autosave after 3 seconds without a change (called from the UI tick).
    pub fn autosave_tick(&mut self) {
        const DEBOUNCE_MS: i64 = 3000;
        if self.editing
            && self.edit_dirty
            && joplin_rs_core::time_ms() - self.edit_changed_ms >= DEBOUNCE_MS
        {
            self.save_edit();
        }
    }

    /// Create a note in the selected notebook and open it for editing.
    pub fn new_note(&mut self) {
        let Some(folder) = self.selected_folder.clone() else {
            self.set_status("请先选择一个笔记本，再新建笔记".into());
            return;
        };
        self.new_note_in(&folder);
    }

    /// Create a note in a specific notebook and open it for editing.
    pub fn new_note_in(&mut self, folder_id: &str) {
        match self.storage.create_note(folder_id, "新建笔记") {
            Ok(id) => {
                self.selected_folder = Some(folder_id.to_string());
                let _ = self.storage.set_last_selected_folder(Some(folder_id));
                self.refresh_lists();
                self.request_open_note(&id);
                self.begin_edit();
            }
            Err(e) => self.set_status(format!("新建笔记失败: {e}")),
        }
    }

    /// Move a note to another notebook.
    pub fn move_note_to(&mut self, note_id: &str, folder_id: &str) {
        match self.storage.move_note(note_id, folder_id) {
            Ok(()) => {
                self.refresh_lists();
                self.set_status("已移动笔记".into());
            }
            Err(e) => self.set_status(format!("移动笔记失败: {e}")),
        }
    }

    fn delete_note(&mut self, id: &str) {
        let was_open = self.selected_note.as_deref() == Some(id);
        self.editing = false;
        self.edit_dirty = false;
        match self.storage.delete_item(id) {
            Ok(()) => {
                if was_open {
                    self.selected_note = None;
                    self.note_body.clear();
                    self.note_raw_body.clear();
                    self.note_title.clear();
                    self.selected_note_encrypted = false;
                    self.note_locked = false;
                }
                self.refresh_lists();
                self.set_status("已删除笔记".into());
            }
            Err(e) => self.set_status(format!("删除笔记失败: {e}")),
        }
    }

    /// Ask for confirmation before deleting a specific note.
    pub fn request_delete_note_of(&mut self, id: &str) {
        self.confirm_note_title = self
            .storage
            .note(id)
            .ok()
            .flatten()
            .and_then(|n| {
                n.props
                    .get("title")
                    .and_then(serde_json::Value::as_str)
                    .map(String::from)
            })
            .unwrap_or_default();
        self.confirm_target = Some(id.to_string());
        self.dialog = Dialog::ConfirmDeleteNote;
    }

    /// Ask for confirmation before deleting a specific notebook. Refused
    /// (with a status message) while the notebook still holds notes.
    pub fn request_delete_notebook_of(&mut self, id: &str) {
        match self.storage.notes_in_folder(id) {
            Ok(n) if n > 0 => {
                self.set_status(format!("笔记本中还有 {n} 条笔记，请先删除这些笔记"));
            }
            Ok(_) => {
                self.confirm_target = Some(id.to_string());
                self.dialog = Dialog::ConfirmDeleteNotebook;
            }
            Err(e) => self.set_status(format!("删除笔记本失败: {e}")),
        }
    }

    /// Apply the delete requested by the open confirmation dialog.
    pub fn confirm_delete(&mut self) {
        let Some(id) = self.confirm_target.clone() else {
            self.dialog = Dialog::None;
            return;
        };
        match self.dialog {
            Dialog::ConfirmDeleteNote => self.delete_note(&id),
            Dialog::ConfirmDeleteNotebook => self.delete_notebook(&id),
            _ => {}
        }
        self.confirm_target = None;
        self.dialog = Dialog::None;
    }

    fn delete_notebook(&mut self, id: &str) {
        match self.storage.notes_in_folder(id) {
            Ok(n) if n > 0 => {
                self.set_status(format!("笔记本中还有 {n} 条笔记，请先删除这些笔记"));
            }
            Ok(_) => match self.storage.delete_item(id) {
                Ok(()) => {
                    if self.selected_folder.as_deref() == Some(id) {
                        self.selected_folder = None;
                        let _ = self.storage.set_last_selected_folder(None);
                    }
                    self.refresh_lists();
                    self.set_status("已删除笔记本".into());
                }
                Err(e) => self.set_status(format!("删除笔记本失败: {e}")),
            },
            Err(e) => self.set_status(format!("删除笔记本失败: {e}")),
        }
    }

    // ---- notebooks (create / rename / delete) ---------------------------

    pub fn open_new_notebook(&mut self) {
        self.nb_input.clear();
        self.nb_error = None;
        self.dialog = Dialog::NewNotebook;
    }

    /// Open the rename dialog for a specific notebook.
    pub fn open_rename_notebook_of(&mut self, id: &str) {
        let title = self
            .folders
            .iter()
            .find(|f| f.props.get("id").and_then(serde_json::Value::as_str) == Some(id))
            .and_then(|f| f.props.get("title"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        self.nb_input = title;
        self.nb_error = None;
        self.rename_target = Some(id.to_string());
        self.dialog = Dialog::RenameNotebook;
    }

    /// Create a notebook from the dialog input; returns true to close.
    pub fn submit_new_notebook(&mut self) -> bool {
        let name = self.nb_input.trim().to_string();
        if name.is_empty() {
            self.nb_error = Some("名称不能为空".into());
            return false;
        }
        match self.storage.create_folder(&name) {
            Ok(id) => {
                self.selected_folder = Some(id.clone());
                let _ = self.storage.set_last_selected_folder(Some(&id));
                self.refresh_lists();
                self.dialog = Dialog::None;
                true
            }
            Err(e) => {
                self.nb_error = Some(format!("创建失败: {e}"));
                false
            }
        }
    }

    /// Rename the notebook targeted by the dialog.
    pub fn submit_rename_notebook(&mut self) -> bool {
        let Some(id) = self.rename_target.clone() else {
            return false;
        };
        let name = self.nb_input.trim().to_string();
        if name.is_empty() {
            self.nb_error = Some("名称不能为空".into());
            return false;
        }
        match self.storage.rename_folder(&id, &name) {
            Ok(()) => {
                self.refresh_lists();
                self.dialog = Dialog::None;
                true
            }
            Err(e) => {
                self.nb_error = Some(format!("重命名失败: {e}"));
                false
            }
        }
    }

    // ---- reading password (per-note protection) ----

    pub fn has_reader_password(&self) -> bool {
        self.config.reader_password_hash.is_some()
    }

    /// Open a note, routing protected notes through the password gate. This
    /// is the only UI path that loads a note body, so every (re)selection of
    /// a protected note asks again; nothing is cached for the session.
    pub fn request_open_note(&mut self, id: &str) {
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
                self.record_recent_note(id, &note);
            }
            self.selected_note = Some(id.to_string());
            self.note_locked = true;
            self.note_body.clear();
            self.note_raw_body.clear();
            self.editing = false;
            self.edit_dirty = false;
            self.prot_input.clear();
            self.prot_error = None;
            // Signal the view layer to clear the gate input field: a
            // protected note re-locking must not reuse the last password.
            self.prot_gate_epoch = self.prot_gate_epoch.wrapping_add(1);
            return;
        }
        self.note_locked = false;
        self.open_note(id);
    }

    /// Verify the gate input against the global reading-password hash and,
    /// on success, load the gated note.
    pub fn try_unlock_note(&mut self) {
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
    pub fn unprotect_gated_note(&mut self) {
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
    pub fn protect_open_note(&mut self) -> bool {
        if !self.has_reader_password() {
            self.open_reader_pw_settings();
            self.set_status("请先设置阅读密码，再保护笔记".into());
            return false;
        }
        let Some(id) = self.selected_note.clone() else {
            return false;
        };
        match self.storage.set_note_protected(&id) {
            Ok(()) => {
                self.protected.insert(id.clone());
                // Route through the gate immediately.
                self.request_open_note(&id);
                true
            }
            Err(e) => {
                self.set_status(format!("保护失败: {e}"));
                false
            }
        }
    }

    /// Toggle protection of a note from the note-list context menu.
    ///
    /// Locking reuses [`Self::protect_open_note`] (which requires a global
    /// reading password, else opens its settings). Unlocking must verify the
    /// password, so the note is routed to its gate where the password can be
    /// entered and the gate's "取消保护" button removes the lock.
    pub fn toggle_protect_note(&mut self, id: &str) {
        self.end_edit();
        if self.protected.contains(id) {
            self.request_open_note(id);
            self.set_status("输入阅读密码后点击「取消保护」".into());
        } else {
            self.selected_note = Some(id.to_string());
            self.protect_open_note();
        }
    }

    pub fn open_reader_pw_settings(&mut self) {
        self.rp_current.clear();
        self.rp_new.clear();
        self.rp_confirm.clear();
        self.rp_message = None;
        self.rp_error = None;
        self.dialog = Dialog::ReaderPasswordSettings;
    }

    /// Apply the set/change form: when no password is set the new field
    /// establishes one; when one is set the current password must match.
    pub fn submit_reader_pw(&mut self) {
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
                self.rp_message = Some("阅读密码已保存，对受保护的笔记立即生效".into());
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
    pub fn remove_reader_pw(&mut self) {
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
    pub fn stored_e2ee_password(&self) -> Option<String> {
        self.config
            .e2ee_password
            .clone()
            .filter(|p| !p.is_empty())
            .or_else(|| self.config.password.clone().filter(|p| !p.is_empty()))
    }

    /// Password to use for the decrypt pass: session prompt first, then the
    /// stored/fallback value.
    pub fn e2ee_password_for_decrypt(&self) -> Option<String> {
        if !self.e2ee_session_password.is_empty() {
            return Some(self.e2ee_session_password.clone());
        }
        self.stored_e2ee_password()
    }

    /// Password used for the combined two-way sync (which needs key material to
    /// ENCRYPT local items for upload). Session prompt first, then the dedicated
    /// `config.e2ee_password`.
    ///
    /// Deliberately does NOT fall back to the WebDAV password: a wrong password
    /// makes `SyncEngine::sync` fail hard (`unlock_master_key` errors) and would
    /// abort the whole sync including downloads. Users who only reuse the WebDAV
    /// password are instead prompted (see `handle_job_event`), and the submitted
    /// value lands in `e2ee_session_password`.
    pub fn resolve_upload_password(&self) -> Option<String> {
        if !self.e2ee_session_password.is_empty() {
            return Some(self.e2ee_session_password.clone());
        }
        self.config.e2ee_password.clone().filter(|p| !p.is_empty())
    }

    /// Accept the E2EE prompt shown after a download and run decryption.
    pub fn submit_e2ee_prompt(&mut self) {
        if self.e2ee_input.is_empty() {
            self.e2ee_prompt_error = Some("密码不能为空".into());
            return;
        }
        self.e2ee_session_password = self.e2ee_input.clone();
        self.e2ee_input.clear();
        self.e2ee_prompt_error = None;
        // An explicit submission overrides a previous decline: the user is
        // actively asking for decryption to run.
        self.e2ee_cancelled = false;
        self.dialog = Dialog::None;
    }

    /// Cancel the post-sync E2EE prompt: placeholders stay in the list. The
    /// decision is sticky: without this flag the periodic auto-decrypt pass
    /// restarts with the same (wrong) stored/fallback password and the
    /// failure re-opens the dialog forever.
    pub fn cancel_e2ee_prompt(&mut self) {
        self.e2ee_input.clear();
        self.e2ee_prompt_error = None;
        self.e2ee_cancelled = true;
        if self.e2ee_pending_count > 0 {
            self.set_status(format!(
                "已取消解密：{} 项保持加密状态（重新同步可重试）",
                self.e2ee_pending_count
            ));
        }
        self.dialog = Dialog::None;
    }

    /// Consume the background-job event (called from the UI layer each frame
    /// or poll tick). Returns true if state changed and the view should
    /// refresh. `auto_start_decrypt` lets the caller hook its own job
    /// starter; here we only mutate state.
    pub fn handle_job_event(&mut self) -> bool {
        let Some(event) = self.jobs.take_event() else {
            return false;
        };
        match event {
            JobEvent::SyncDone(result) => match result {
                Ok((summary, pending)) => {
                    self.reload_meta();
                    self.e2ee_pending_count = pending;
                    // A fresh batch resets the prompt slate: a previous
                    // decline and the record of which password the last
                    // decrypt pass tried no longer apply.
                    self.e2ee_cancelled = false;
                    self.e2ee_job_password.clear();
                    let mut base = format!(
                        "同步完成：上传 {}，下载 {}，远端删除 {}，本地删除 {}，资源 {}",
                        summary.uploaded,
                        summary.downloaded,
                        summary.deleted_remote,
                        summary.deleted,
                        summary.resources
                    );
                    if !summary.conflicts.is_empty() {
                        base.push_str(&format!(
                            "；{} 项冲突已跳过（保留远端）",
                            summary.conflicts.len()
                        ));
                    }
                    let blocked = summary.upload_blocked;
                    if blocked > 0 {
                        base.push_str(&format!("；{blocked} 项本地修改待上传（需 E2EE 密码）"));
                    }
                    // Both a download that needs decrypting and a blocked upload
                    // need the E2EE password; prompting once serves either and
                    // the submitted value lands in `e2ee_session_password`.
                    let needs_password = (pending > 0
                        && self.e2ee_password_for_decrypt().is_none())
                        || (blocked > 0 && self.resolve_upload_password().is_none());
                    if pending > 0 && self.e2ee_password_for_decrypt().is_some() {
                        // The caller (jobs layer) starts the decrypt pass.
                        self.set_status(format!("{base}；正在解密 {pending} 项…"));
                    } else if needs_password {
                        self.set_status(format!("{base}；等待 E2EE 密码"));
                        self.e2ee_input.clear();
                        self.e2ee_prompt_error = None;
                        self.dialog = Dialog::E2eePrompt;
                    } else {
                        self.set_status(base);
                    }
                    true
                }
                Err(e) => {
                    self.set_status(format!("同步失败: {e}"));
                    true
                }
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
                        // Wrong/unsupported E2EE password: placeholders remain
                        // in the list. The session password is kept: clearing
                        // it here would wipe a password the user submitted
                        // while this job was still running.
                        let looks_wrong = looks_like_wrong_e2ee_password(&e);
                        self.set_status(format!("E2EE 解密失败: {e}"));
                        let user_supplied_new = e2ee_retry_supersedes_prompt(
                            &self.e2ee_session_password,
                            &self.e2ee_job_password,
                        );
                        if user_supplied_new {
                            // A password submitted while the failed job was
                            // still running: the auto-continue pass picks it
                            // up (it differs from the password just tried).
                            self.e2ee_input.clear();
                            self.e2ee_prompt_error = None;
                            self.dialog = Dialog::None;
                        } else if self.e2ee_pending_count > 0 && !self.e2ee_cancelled {
                            self.e2ee_input.clear();
                            self.e2ee_prompt_error = Some(if looks_wrong {
                                "E2EE 密码错误，未解密笔记保持加密状态".into()
                            } else {
                                format!("解密失败: {e}")
                            });
                            self.dialog = Dialog::E2eePrompt;
                        }
                    }
                }
                true
            }
        }
    }

    // ---- settings (mirrors the CLI config commands) ---------------------

    /// Stage the current config into the settings dialog fields.
    pub fn open_settings(&mut self) {
        self.set_webdav_url = self.config.webdav_url.clone();
        self.set_username = self.config.username.clone();
        self.set_password.clear();
        self.set_e2ee_password.clear();
        self.set_insecure_tls = self.config.insecure_tls;
        self.set_ca_cert = self.config.ca_cert.clone().unwrap_or_default();
        self.set_vim_mode = self.config.vim_mode;
        self.set_sync_on_startup = self.config.sync_on_startup;
        self.set_sync_on_quit = self.config.sync_on_quit;
        self.set_message = None;
        self.set_error = None;
        self.dialog = Dialog::Settings;
    }

    /// Persist the settings form. Each key is rewritten on its own so already-
    /// encrypted secrets stay byte-for-byte; new passwords are encrypted at
    /// rest exactly like `CLI set-password`. Returns true when the dialog can
    /// close (no write errors).
    pub fn submit_settings(&mut self) -> bool {
        let path = config_path();
        let mut errors: Vec<String> = Vec::new();
        macro_rules! put {
            ($key:expr, $rendered:expr) => {
                if let Err(e) = write_config_raw_key(&path, $key, $rendered.as_deref()) {
                    errors.push(format!("{}: {e}", $key));
                }
            };
        }

        put!("webdav_url", Some(toml_string(&self.set_webdav_url)));
        put!("username", Some(toml_string(&self.set_username)));
        put!("insecure_tls", Some(self.set_insecure_tls.to_string()));
        put!("vim_mode", Some(self.set_vim_mode.to_string()));
        put!(
            "sync_on_startup",
            Some(self.set_sync_on_startup.to_string())
        );
        put!("sync_on_quit", Some(self.set_sync_on_quit.to_string()));
        let ca = self.set_ca_cert.trim().to_string();
        put!(
            "ca_cert",
            if ca.is_empty() {
                None
            } else {
                Some(toml_string(&ca))
            }
        );

        // Passwords are only rewritten when a new value was typed into the form.
        let staged = [
            ("password", self.set_password.clone()),
            ("e2ee_password", self.set_e2ee_password.clone()),
        ];
        for (key, plain) in staged {
            if plain.is_empty() {
                continue;
            }
            match LocalKey::load_or_create(&data_dir().join("secret.key"))
                .map_err(|e| e.to_string())
                .and_then(|k| k.encrypt(&plain).map_err(|e| e.to_string()))
            {
                Ok(enc) => put!(key, Some(toml_string(&enc))),
                Err(e) => errors.push(format!("{key}: {e}")),
            }
        }

        if !errors.is_empty() {
            self.set_error = Some(errors.join("; "));
            return false;
        }

        // Apply to the in-memory config so the next sync uses the new values.
        self.config.webdav_url = self.set_webdav_url.clone();
        self.config.username = self.set_username.clone();
        self.config.insecure_tls = self.set_insecure_tls;
        self.config.ca_cert = if ca.is_empty() { None } else { Some(ca) };
        self.config.vim_mode = self.set_vim_mode;
        self.config.sync_on_startup = self.set_sync_on_startup;
        self.config.sync_on_quit = self.set_sync_on_quit;
        if !self.set_password.is_empty() {
            self.config.password = Some(self.set_password.clone());
        }
        if !self.set_e2ee_password.is_empty() {
            self.config.e2ee_password = Some(self.set_e2ee_password.clone());
        }
        self.set_password.clear();
        self.set_e2ee_password.clear();
        self.set_message = Some("设置已保存".into());
        true
    }

    pub fn set_status(&mut self, text: String) {
        self.status = text.clone();
        *self.jobs.status.lock().unwrap() = text;
    }
}

/// True when an error string looks like a wrong E2EE password (vs an
/// unsupported key type or a network failure). Mirrors the heuristic used by
/// the CLI.
pub fn looks_like_wrong_e2ee_password(err: &str) -> bool {
    let e = err.to_lowercase();
    e.contains("decrypt")
        || e.contains("password")
        || e.contains("aes-gcm")
        || e.contains("aead")
        || e.contains("密钥")
        || e.contains("密码")
}

/// Pure decision core for the automatic decrypt continuation run every
/// UI tick. Starts only when items are pending, no job is running, the user
/// has not declined, and a password is available that the last job has not
/// already tried (retrying the same wrong fallback password is the endless
/// cancel-loop bug). User-submitted retries bypass this via the prompt's
/// explicit `start_decrypt`.
pub fn e2ee_auto_start_pending(
    pending: usize,
    busy: bool,
    cancelled: bool,
    password: Option<&str>,
    last_tried: &str,
) -> bool {
    pending > 0
        && !busy
        && !cancelled
        && password.is_some_and(|pw| !pw.is_empty() && pw != last_tried)
}

/// True when a user-submitted password supersedes the one the failed decrypt
/// job just tried: instead of re-prompting with an error, the auto-continue
/// pass should immediately retry with the newer password.
pub fn e2ee_retry_supersedes_prompt(session: &str, tried: &str) -> bool {
    !session.is_empty() && session != tried
}

pub fn is_encrypted_placeholder(item: &Item) -> bool {
    // An item is an (undecrypted) placeholder iff it is flagged as encrypted,
    // or still carries cipher text (envelope whose flag failed to parse as 1).
    // Note: a normal plaintext note with an empty body and encryption_applied
    // = 0 must NOT be treated as encrypted.
    let applied = item
        .props
        .get("encryption_applied")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);
    if applied == 1 {
        return true;
    }
    item.props
        .get("encryption_cipher_text")
        .and_then(serde_json::Value::as_str)
        .map(|s| !s.is_empty())
        .unwrap_or(false)
}

/// Shorten an id for display: Joplin ids are 32-hex; show the last 8 chars.
pub fn short_id(id: &str) -> String {
    if id.len() > 8 {
        id[id.len() - 8..].to_string()
    } else {
        id.to_string()
    }
}

/// Clamp `current + delta` into `0..=max`.
fn clamp_step(current: usize, delta: i32, max: usize) -> usize {
    (current as i64 + delta as i64).clamp(0, max as i64) as usize
}

const HEX: &[u8] = b"0123456789abcdefABCDEF";

/// Extract Joplin resource ids (`:/32-hex`) from a note body, deduplicated,
/// order-preserving. Scans by bytes with char-boundary safety: the 32-byte
/// look-ahead window is only treated as a link when every byte is ASCII hex,
/// so a window ending inside a multibyte CJK character is skipped instead of
/// slicing mid-character (which would panic).
pub fn crate_resource_ids(body: &str) -> Vec<String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use joplin_rs_core::secret::LocalKey;

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
        let body = format!(":/{} text :/{ID}", ID);
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
        let body = format!(":/{}交", "a".repeat(31));
        assert!(crate_resource_ids(&body).is_empty());

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

        assert!(crate_resource_ids("交运医药商保清分工作环境交运医药").is_empty());
    }

    /// Regression: cancelling the E2EE prompt must stop the auto-decrypt
    /// pass from restarting it. The old loop: cancel -> pending stays > 0 ->
    /// the tick restarted decrypt with the same wrong stored/fallback
    /// password -> failure re-opened the prompt, forever.
    #[test]
    fn e2ee_auto_start_honours_cancel_and_tried_passwords() {
        // Fresh sync batch: nothing tried, password available -> start.
        assert!(e2ee_auto_start_pending(5, false, false, Some("pw"), ""));
        // No password at all -> the prompt dialog path, not an auto start.
        assert!(!e2ee_auto_start_pending(5, false, false, None, ""));
        assert!(!e2ee_auto_start_pending(5, false, false, Some(""), ""));
        // A job is already running -> wait.
        assert!(!e2ee_auto_start_pending(5, true, false, Some("pw"), ""));
        // Nothing pending -> no-op.
        assert!(!e2ee_auto_start_pending(0, false, false, Some("pw"), ""));

        // The user declined -> never auto-start again, even though the same
        // password is still available and items are still encrypted.
        assert!(!e2ee_auto_start_pending(5, false, true, Some("pw"), ""));

        // The exact password the last job tried must not auto-retry: this is
        // the fallback-password loop (cancel -> retry same wrong password).
        assert!(!e2ee_auto_start_pending(
            5,
            false,
            false,
            Some("dav-pw"),
            "dav-pw"
        ));
        // A different (newly submitted) password does auto-start.
        assert!(e2ee_auto_start_pending(
            5,
            false,
            false,
            Some("e2ee"),
            "dav-pw"
        ));
    }

    /// The combined sync needs key material to ENCRYPT uploads. The password is
    /// the session value (user prompt) first, else the dedicated config E2EE
    /// password; the WebDAV password is never used as a fallback because a wrong
    /// value would hard-fail the whole sync.
    #[test]
    fn resolve_upload_password_session_then_dedicated_config() {
        let mut state =
            AppState::build(Storage::open_in_memory().unwrap(), Config::default()).unwrap();
        assert_eq!(state.resolve_upload_password(), None);

        state.config.e2ee_password = Some("config-pw".into());
        assert_eq!(
            state.resolve_upload_password().as_deref(),
            Some("config-pw")
        );

        // A session value (user prompt) wins over the stored dedicated value.
        state.e2ee_session_password = "session-pw".into();
        assert_eq!(
            state.resolve_upload_password().as_deref(),
            Some("session-pw")
        );

        // Empty stored value is ignored.
        state.e2ee_session_password.clear();
        state.config.e2ee_password = Some(String::new());
        assert_eq!(state.resolve_upload_password(), None);

        // The WebDAV password is NOT an upload fallback.
        state.config.password = Some("dav-pw".into());
        assert_eq!(state.resolve_upload_password(), None);
    }

    /// The ⌘⌥F search dialog applies the query through `apply_search`: a
    /// non-empty query filters the note list, a blank one clears it, and the
    /// dialog flag is reset either way.
    #[test]
    fn apply_search_filters_and_clears() {
        let mut state =
            AppState::build(Storage::open_in_memory().unwrap(), Config::default()).unwrap();
        let folder = state.storage.create_folder("F").unwrap();
        let a = state.storage.create_note(&folder, "alpha apple").unwrap();
        let b = state.storage.create_note(&folder, "beta banana").unwrap();
        state
            .storage
            .update_note(&a, "alpha apple", "apple pie")
            .unwrap();
        state
            .storage
            .update_note(&b, "beta banana", "banana bread")
            .unwrap();
        state.selected_folder = Some(folder.clone());
        state.refresh_lists();

        state.dialog = Dialog::Search;
        state.apply_search("apple");
        assert_eq!(state.dialog, Dialog::None, "dialog closes after applying");
        let ids = |s: &AppState| -> Vec<String> {
            s.notes
                .iter()
                .filter_map(|n| n.props["id"].as_str().map(str::to_string))
                .collect()
        };
        let shown = ids(&state);
        assert!(shown.contains(&a), "matching note shown: {shown:?}");
        assert!(!shown.contains(&b), "non-matching note hidden: {shown:?}");
        assert!(state.search_hits.contains_key(&a), "snippet recorded");

        state.apply_search("   ");
        assert!(state.search.is_empty(), "blank query clears the filter");
        let shown = ids(&state);
        assert!(
            shown.contains(&a) && shown.contains(&b),
            "all notes shown after clearing: {shown:?}"
        );
    }

    /// Context-menu protection toggle: without a reading password it opens the
    /// password settings instead of locking; with one it locks the note and
    /// routes it to the gate (removing the lock also goes through the gate).
    #[test]
    fn toggle_protect_note_locks_and_routes_to_gate() {
        let mut state =
            AppState::build(Storage::open_in_memory().unwrap(), Config::default()).unwrap();
        let folder = state.storage.create_folder("F").unwrap();
        let note = state.storage.create_note(&folder, "N").unwrap();
        state.refresh_lists();
        state.request_open_note(&note);

        // No reading password: refused, and the settings dialog opens instead.
        state.toggle_protect_note(&note);
        assert_eq!(state.dialog, Dialog::ReaderPasswordSettings);
        assert!(
            !state.protected.contains(&note),
            "not locked without a password"
        );

        // With a reading password the note is locked and routed to its gate.
        state.config.reader_password_hash = Some(hash_password("pw"));
        state.dialog = Dialog::None;
        state.toggle_protect_note(&note);
        assert!(state.protected.contains(&note), "note is protected");
        assert!(state.note_locked, "protected note routes through the gate");
        assert_eq!(state.selected_note.as_deref(), Some(note.as_str()));

        // Toggling a protected note again goes straight to the gate.
        state.note_locked = false;
        state.toggle_protect_note(&note);
        assert!(state.note_locked, "unlock path lands on the gate");
        assert!(
            state.protected.contains(&note),
            "still protected until the password verifies"
        );
    }

    /// Regression: submitting the correct E2EE password while a stale job
    /// (started with the wrong fallback password) was still running lost the
    /// submission - the old DecryptDone(Err) handler cleared the session
    /// password and re-prompted instead of retrying with it.
    #[test]
    fn e2ee_new_submission_supersedes_failed_job() {
        // Session password differs from the password the failed job tried:
        // retry with it instead of showing the prompt again.
        assert!(e2ee_retry_supersedes_prompt("correct-pw", "dav-pw"));
        // Session password is the same one that just failed: re-prompt so
        // the user can correct it.
        assert!(!e2ee_retry_supersedes_prompt("wrong-pw", "wrong-pw"));
        // Nothing submitted: always re-prompt.
        assert!(!e2ee_retry_supersedes_prompt("", "dav-pw"));
    }

    /// Full state-machine regression for the reported E2EE flows, simulating
    /// the view tick: consume job events, then run the auto-decrypt pass.
    /// Uses an in-memory storage so nothing touches the user's data dir.
    fn tick(state: &mut AppState) {
        let _ = state.handle_job_event();
        crate::jobs::auto_decrypt(state);
    }

    /// The reported cancel loop: sync leaves items encrypted, the fallback
    /// password fails, the user cancels -> the prompt must stay closed and no
    /// further decrypt job may auto-start.
    #[test]
    fn e2ee_cancel_stops_the_prompt_loop_end_to_end() {
        let mut state =
            AppState::build(Storage::open_in_memory().unwrap(), Config::default()).unwrap();
        state.config.password = Some("dav-pw".into()); // reused fallback

        // SyncDone with pending items: the tick auto-starts decrypt with the
        // fallback password (busy until the simulated job finishes).
        state.e2ee_pending_count = 3;
        let _ = state.jobs.take_event();
        state.jobs.running = std::sync::Arc::new(std::sync::Mutex::new(false));
        let event = JobEvent::SyncDone(Ok((
            Stage1Summary {
                downloaded: 3,
                deleted: 0,
                resources: 0,
                uploaded: 0,
                deleted_remote: 0,
                conflicts: Vec::new(),
                upload_blocked: 0,
            },
            3,
        )));
        *state.jobs.event.lock().unwrap() = Some(event);
        tick(&mut state);
        assert_eq!(state.e2ee_job_password, "dav-pw");

        // The fallback password fails (simulated DecryptDone): the worker
        // clears the busy flag before posting the event, as jobs.rs does.
        state.jobs.running = std::sync::Arc::new(std::sync::Mutex::new(false));
        *state.jobs.event.lock().unwrap() = Some(JobEvent::DecryptDone(Err(
            "aes-gcm: password mismatch".into(),
        )));
        tick(&mut state);
        assert_eq!(state.dialog, Dialog::E2eePrompt);

        // User cancels: dialog closes and the decision is sticky.
        state.cancel_e2ee_prompt();
        assert_eq!(state.dialog, Dialog::None);
        assert!(state.e2ee_cancelled);

        // Ticks keep running (as in the real UI): no job restart, no dialog
        // re-open, even though pending stays > 0 and the password is there.
        for _ in 0..5 {
            tick(&mut state);
            assert_eq!(state.dialog, Dialog::None, "prompt must stay closed");
            assert!(!state.jobs.is_busy(), "no decrypt job may restart");
        }
    }

    /// The reported lost submission: the user types the correct password
    /// while a stale (fallback-password) job is still running. The stale job
    /// then fails; the submitted password must survive and be retried.
    #[test]
    fn e2ee_submission_during_stale_job_is_retried_end_to_end() {
        let mut state =
            AppState::build(Storage::open_in_memory().unwrap(), Config::default()).unwrap();
        state.config.password = Some("dav-pw".into());

        // SyncDone -> auto-start with the fallback password.
        state.e2ee_pending_count = 2;
        state.jobs.running = std::sync::Arc::new(std::sync::Mutex::new(false));
        *state.jobs.event.lock().unwrap() = Some(JobEvent::SyncDone(Ok((
            Stage1Summary {
                downloaded: 2,
                deleted: 0,
                resources: 0,
                uploaded: 0,
                deleted_remote: 0,
                conflicts: Vec::new(),
                upload_blocked: 0,
            },
            2,
        ))));
        tick(&mut state);
        assert_eq!(state.e2ee_job_password, "dav-pw");

        // Mid-job the user submits the correct password (the view accepted
        // it; the job slot is still busy, mirroring start_decrypt's guard).
        state.e2ee_input = "correct-pw".into();
        state.submit_e2ee_prompt();
        assert_eq!(state.e2ee_session_password, "correct-pw");

        // The stale job fails: the worker clears the busy flag before
        // posting the event (as jobs.rs does). The submission must NOT be
        // wiped, and the next tick must retry with the correct password
        // instead of just re-prompting.
        state.jobs.running = std::sync::Arc::new(std::sync::Mutex::new(false));
        *state.jobs.event.lock().unwrap() = Some(JobEvent::DecryptDone(Err(
            "aes-gcm: password mismatch".into(),
        )));
        tick(&mut state);
        assert_eq!(state.e2ee_session_password, "correct-pw");
        assert_eq!(state.e2ee_job_password, "correct-pw");
        assert_eq!(state.dialog, Dialog::None);
    }

    /// Bug 1 end-to-end on the state machine: every re-lock of a protected
    /// note bumps the gate epoch so the view layer clears the field, and the
    /// mirror input is always reset.
    #[test]
    fn gate_relock_bumps_epoch_and_clears_input() {
        let mut state =
            AppState::build(Storage::open_in_memory().unwrap(), Config::default()).unwrap();
        let note = Item {
            item_type: 1,
            props: {
                let mut m = std::collections::BTreeMap::new();
                m.insert("id".to_string(), serde_json::Value::from(ID));
                m.insert("title".to_string(), serde_json::Value::from("secret"));
                m
            },
            body: "confidential".into(),
        };
        state.storage.upsert_item(&note).unwrap();
        state.storage.set_note_protected(ID).unwrap();

        state.reload_meta();
        assert!(state.protected.contains(ID));

        // First open: gate locks, epoch advances from 0.
        state.request_open_note(ID);
        assert!(state.note_locked);
        assert_eq!(state.prot_gate_epoch, 1);
        let first_epoch = state.prot_gate_epoch;

        // Simulate the user typing (the view mirrors the field into state).
        state.prot_input = "old-password".into();

        // Focus lost, note re-opened: the epoch must advance again and the
        // mirrored input reset, so the view knows to clear the field.
        state.request_open_note(ID);
        assert!(state.note_locked);
        assert_eq!(state.prot_gate_epoch, first_epoch + 1);
        assert!(state.prot_input.is_empty());
    }

    // ---- note editing / CRUD -------------------------------------------

    #[test]
    fn edit_save_and_delete_roundtrip() {
        let mut state =
            AppState::build(Storage::open_in_memory().unwrap(), Config::default()).unwrap();
        let folder = state.storage.create_folder("F").unwrap();
        let note = state.storage.create_note(&folder, "T").unwrap();
        state.refresh_lists();
        state.request_open_note(&note);
        assert!(state.can_edit(), "a plain note is editable");

        state.begin_edit();
        assert!(state.editing);
        state.edit_title = "T2".into();
        state.edit_body = "body text".into();
        state.mark_edit_dirty();
        assert!(state.edit_dirty);
        assert!(state.save_edit());
        assert!(!state.edit_dirty);

        let saved = state.storage.note(&note).unwrap().unwrap();
        assert_eq!(saved.props["title"].as_str(), Some("T2"));
        assert_eq!(saved.body, "body text");
        assert!(
            state
                .storage
                .items_needing_sync()
                .unwrap()
                .iter()
                .any(|i| i.props["id"].as_str() == Some(note.as_str())),
            "an edited note must be queued for upload"
        );

        state.request_delete_note_of(&note);
        state.confirm_delete();
        assert!(state.selected_note.is_none());
        assert!(state.storage.note(&note).unwrap().is_none());
        assert_eq!(state.storage.pending_deletes().unwrap(), vec![note]);
    }

    #[test]
    fn edit_disabled_for_protected_note() {
        let mut state =
            AppState::build(Storage::open_in_memory().unwrap(), Config::default()).unwrap();
        let folder = state.storage.create_folder("F").unwrap();
        let note = state.storage.create_note(&folder, "T").unwrap();
        state.storage.set_note_protected(&note).unwrap();
        state.refresh_lists();
        state.request_open_note(&note);
        assert!(state.note_locked);
        assert!(!state.can_edit(), "a locked note cannot be edited");
    }

    #[test]
    fn notebook_delete_blocked_while_notes_exist() {
        let mut state =
            AppState::build(Storage::open_in_memory().unwrap(), Config::default()).unwrap();
        let folder = state.storage.create_folder("F").unwrap();
        let note = state.storage.create_note(&folder, "N").unwrap();
        state.refresh_lists();
        state.selected_folder = Some(folder.clone());

        state.request_delete_notebook_of(&folder);
        state.confirm_delete();
        assert_eq!(
            state.storage.folders().unwrap().len(),
            1,
            "a notebook with notes must not be deleted"
        );

        state.storage.delete_item(&note).unwrap();
        state.selected_folder = Some(folder.clone());
        state.request_delete_notebook_of(&folder);
        state.confirm_delete();
        assert!(state.storage.folders().unwrap().is_empty());
        assert!(state.selected_folder.is_none());
    }

    #[test]
    fn delete_requests_open_confirm_dialog() {
        let mut state =
            AppState::build(Storage::open_in_memory().unwrap(), Config::default()).unwrap();
        let folder = state.storage.create_folder("F").unwrap();
        let note = state.storage.create_note(&folder, "N").unwrap();
        state.refresh_lists();
        state.selected_folder = Some(folder.clone());
        state.request_open_note(&note);

        // Delete note: opens the confirmation dialog, does not delete yet.
        state.request_delete_note_of(&note);
        assert_eq!(state.dialog, Dialog::ConfirmDeleteNote);
        assert_eq!(state.confirm_note_title, "N");
        assert!(state.storage.note(&note).unwrap().is_some());

        // Confirming deletes and closes the dialog (no reopen).
        state.confirm_delete();
        assert_eq!(state.dialog, Dialog::None);
        assert!(state.storage.note(&note).unwrap().is_none());

        // Notebook delete is refused outright while it holds notes.
        let note2 = state.storage.create_note(&folder, "N2").unwrap();
        state.refresh_lists();
        state.request_delete_notebook_of(&folder);
        assert_eq!(state.dialog, Dialog::None);
        assert_eq!(state.storage.folders().unwrap().len(), 1);

        // Empty notebook: opens the confirmation dialog, then deletes.
        state.storage.delete_item(&note2).unwrap();
        state.request_delete_notebook_of(&folder);
        assert_eq!(state.dialog, Dialog::ConfirmDeleteNotebook);
        state.confirm_delete();
        assert!(state.storage.folders().unwrap().is_empty());
    }

    #[test]
    fn move_note_to_another_notebook() {
        let mut state =
            AppState::build(Storage::open_in_memory().unwrap(), Config::default()).unwrap();
        let f1 = state.storage.create_folder("F1").unwrap();
        let f2 = state.storage.create_folder("F2").unwrap();
        let note = state.storage.create_note(&f1, "N").unwrap();
        state.refresh_lists();

        state.move_note_to(&note, &f2);
        assert_eq!(
            state.storage.note(&note).unwrap().unwrap().props["parent_id"].as_str(),
            Some(f2.as_str())
        );
        assert_eq!(state.storage.notes_in_folder(&f2).unwrap(), 1);
        assert_eq!(state.storage.notes_in_folder(&f1).unwrap(), 0);
        assert!(
            state
                .storage
                .items_needing_sync()
                .unwrap()
                .iter()
                .any(|i| i.props["id"].as_str() == Some(note.as_str())),
            "a moved note must be queued for upload"
        );
    }

    // ---- recent-note restore & navigation --------------------------------

    #[test]
    fn clamp_step_bounds() {
        assert_eq!(clamp_step(0, 1, 3), 1);
        assert_eq!(clamp_step(0, -1, 3), 0);
        assert_eq!(clamp_step(3, 1, 3), 3);
        assert_eq!(clamp_step(2, -5, 3), 0);
    }

    #[test]
    fn select_folder_opens_recent_then_newest() {
        let mut state =
            AppState::build(Storage::open_in_memory().unwrap(), Config::default()).unwrap();
        let f1 = state.storage.create_folder("F1").unwrap();
        let f2 = state.storage.create_folder("F2").unwrap();
        let n1 = state.storage.create_note(&f1, "A").unwrap();
        let n2 = state.storage.create_note(&f2, "B").unwrap();
        state.refresh_lists();

        // No recency yet: the newest note of the folder opens.
        state.select_folder(Some(f2.clone()));
        assert_eq!(state.selected_note.as_deref(), Some(n2.as_str()));
        assert_eq!(
            state.storage.last_selected_folder().unwrap().as_deref(),
            Some(f2.as_str())
        );

        // Opening n1 records recency; reselecting f1 restores it.
        state.request_open_note(&n1);
        state.select_folder(Some(f1.clone()));
        assert_eq!(state.selected_note.as_deref(), Some(n1.as_str()));

        // A vanished recent note falls back (here f1 becomes empty).
        state.storage.delete_item(&n1).unwrap();
        state.refresh_lists();
        state.select_folder(Some(f1.clone()));
        assert!(state.selected_note.is_none());
    }

    #[test]
    fn restore_last_session_restores_folder_and_recent_note() {
        let mut state =
            AppState::build(Storage::open_in_memory().unwrap(), Config::default()).unwrap();
        let f1 = state.storage.create_folder("F1").unwrap();
        let _f2 = state.storage.create_folder("F2").unwrap();
        let n1 = state.storage.create_note(&f1, "A").unwrap();
        let _n2 = state.storage.create_note(&f1, "B").unwrap();
        state.refresh_lists();
        state.storage.set_last_selected_folder(Some(&f1)).unwrap();
        state.storage.set_last_opened_note(Some(&f1), &n1).unwrap();
        state.selected_folder = None;
        state.selected_note = None;

        state.restore_last_session();
        assert_eq!(state.selected_folder.as_deref(), Some(f1.as_str()));
        assert_eq!(state.selected_note.as_deref(), Some(n1.as_str()));
    }

    #[test]
    fn raw_writer_sets_bool_and_preserves_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let key = LocalKey::from_bytes([5u8; 32]);
        let enc = key.encrypt("secret").unwrap();
        std::fs::write(&path, format!("password = \"{enc}\"\n")).unwrap();

        write_config_raw_key(&path, "vim_mode", Some("false")).unwrap();
        write_config_raw_key(&path, "webdav_url", Some(&toml_string("https://x/dav"))).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("vim_mode = false"), "{raw}");
        assert!(raw.contains("webdav_url = \"https://x/dav\""), "{raw}");
        assert!(
            raw.contains(&format!("password = \"{enc}\"")),
            "secret must stay byte-for-byte: {raw}"
        );

        write_config_raw_key(&path, "vim_mode", None).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("vim_mode"), "{raw}");
    }

    // ---- encrypted config secrets (CLI set-password compatibility) ----

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
            vim_mode: true,
            sync_on_startup: true,
            sync_on_quit: true,
        };
        decrypt_config_secrets(&mut cfg, &key);
        assert_eq!(cfg.password.as_deref(), Some("dav-pw"));
        assert_eq!(cfg.e2ee_password.as_deref(), Some("e2ee-pw"));
    }

    #[test]
    fn plaintext_and_missing_secrets_pass_through() {
        let key = LocalKey::from_bytes([1u8; 32]);
        let enc = LocalKey::from_bytes([1u8; 32]).encrypt("dav-pw").unwrap();
        let mut cfg = Config {
            password: Some("legacy-plain".into()),
            e2ee_password: None,
            ..Default::default()
        };
        decrypt_config_secrets(&mut cfg, &key);
        assert_eq!(cfg.password.as_deref(), Some("legacy-plain"));
        let _ = enc;
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
