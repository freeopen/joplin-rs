//! joplin-rs GUI: read-only note browser (egui).

mod read_password;

use joplin_rs_core::items::Item;
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
    /// PBKDF2 envelope of the "reading password" that locks the GUI on launch.
    /// `None` means the app opens without a password prompt.
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
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|raw| toml::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Persist the config, creating ~/.joplin if needed.
fn save_config(config: &Config) -> anyhow::Result<()> {
    let path = config_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let raw = toml::to_string_pretty(config)?;
    std::fs::write(&path, raw)?;
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
    /// Set / change / remove the reading password.
    ReaderPasswordSettings,
    /// Prompt for the E2EE master-key password before a sync.
    E2eePrompt,
}

struct App {
    storage: Storage,
    config: Config,
    // UI state
    folders: Vec<Item>,
    selected_folder: Option<String>,
    notes: Vec<Item>,
    selected_note: Option<String>,
    note_body: String,
    note_title: String,
    search: String,
    /// Search-hit snippets keyed by note id (shown under the note title).
    search_hits: std::collections::HashMap<String, String>,
    e2ee_password: String,
    status: String,
    sync_running: Arc<Mutex<bool>>,
    status_shared: Arc<Mutex<String>>,
    /// Last sync error text; used to spot a wrong E2EE password so the next
    /// Sync click can re-prompt instead of silently failing again.
    sync_error: Arc<Mutex<Option<String>>>,
    resource_dir: PathBuf,
    note_tags: std::collections::HashMap<String, Vec<String>>, // note_id -> tag titles

    // ---- reading password (launch lock) ----
    /// False until the launch-lock password has been entered correctly (or no
    /// reading password is configured). While locked, only the lock screen is
    /// shown and all note data stays hidden.
    unlocked: bool,
    lock_input: String,
    lock_error: Option<String>,

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
    /// A sync click opened the E2EE prompt; once the password is submitted the
    /// sync should start automatically.
    e2ee_pending_sync: bool,
    /// A previous sync proved the WebDAV-password fallback is NOT the E2EE
    /// password. Until restart, stop falling back so Sync re-prompts instead
    /// of failing with the wrong password forever.
    e2ee_webdav_fallback_rejected: bool,
}

impl App {
    fn new() -> Self {
        let storage = Storage::open(&data_dir().join("joplin-rs.db")).expect("open storage");
        let config: Config = load_config();
        // A reading password is configured: launch locked. Reloading note
        // metadata is deferred until after unlock so the list/titles are not
        // populated behind the lock screen.
        let has_reader_pw = config.reader_password_hash.is_some();
        let mut app = Self {
            storage,
            config: config.clone(),
            folders: Vec::new(),
            selected_folder: None,
            notes: Vec::new(),
            selected_note: None,
            note_body: String::new(),
            note_title: String::new(),
            search: String::new(),
            search_hits: Default::default(),
            e2ee_password: config
                .e2ee_password
                .clone()
                .or_else(|| config.password.clone())
                .unwrap_or_default(),
            status: "ready".into(),
            sync_running: Arc::new(Mutex::new(false)),
            status_shared: Arc::new(Mutex::new(String::new())),
            sync_error: Arc::new(Mutex::new(None)),
            resource_dir: PathBuf::new(),
            note_tags: Default::default(),
            unlocked: !has_reader_pw,
            lock_input: String::new(),
            lock_error: None,
            dialog: Dialog::None,
            rp_current: String::new(),
            rp_new: String::new(),
            rp_confirm: String::new(),
            rp_message: None,
            rp_error: None,
            e2ee_input: String::new(),
            e2ee_prompt_error: None,
            e2ee_pending_sync: false,
            e2ee_webdav_fallback_rejected: false,
        };
        if app.unlocked {
            app.reload_meta();
        }
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

    // ---- reading password (launch lock) ----

    fn has_reader_password(&self) -> bool {
        self.config.reader_password_hash.is_some()
    }

    /// Verify the launch-lock input against the stored hash.
    fn try_unlock(&mut self) {
        let Some(envelope) = self.config.reader_password_hash.as_deref() else {
            self.unlocked = true;
            return;
        };
        if verify_password(&self.lock_input, envelope) {
            self.unlocked = true;
            self.lock_input.clear();
            self.lock_error = None;
            self.reload_meta();
        } else {
            self.lock_error = Some("密码错误，请重试".into());
            self.lock_input.clear();
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
        match save_config(&self.config) {
            Ok(()) => {
                self.rp_current.clear();
                self.rp_new.clear();
                self.rp_confirm.clear();
                self.rp_message = Some("阅读密码已保存，下次打开时生效".into());
            }
            Err(e) => {
                self.config.reader_password_hash = previous;
                self.rp_error = Some(format!("保存失败: {e}"));
            }
        }
    }

    /// Remove the launch lock (requires the current password).
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
        match save_config(&self.config) {
            Ok(()) => {
                self.rp_current.clear();
                self.rp_new.clear();
                self.rp_confirm.clear();
                self.rp_message = Some("阅读密码已取消".into());
            }
            Err(e) => {
                self.config.reader_password_hash = previous;
                self.rp_error = Some(format!("保存失败: {e}"));
            }
        }
    }

    // ---- E2EE password prompt before sync ----

    /// Effective E2EE password available for the next sync: the in-app field,
    /// then config.e2ee_password, then the WebDAV password fallback.
    fn effective_e2ee_password(&self) -> Option<String> {
        if !self.e2ee_password.is_empty() {
            return Some(self.e2ee_password.clone());
        }
        self.config
            .e2ee_password
            .clone()
            .filter(|p| !p.is_empty())
            .or_else(|| {
                if self.e2ee_webdav_fallback_rejected {
                    None
                } else {
                    self.config.password.clone().filter(|p| !p.is_empty())
                }
            })
    }

    /// Sync button entry point. If no E2EE password is known (or the previous
    /// sync failed because the stored one was wrong), prompt before syncing.
    fn request_sync(&mut self, ctx: &egui::Context) {
        let previous_wrong = self
            .sync_error
            .lock()
            .unwrap()
            .as_deref()
            .is_some_and(looks_like_wrong_e2ee_password);
        if previous_wrong {
            // Forget the rejected password so the prompt is required again.
            self.e2ee_password.clear();
            self.config.e2ee_password = None;
            // The wrong value may have come from the WebDAV-password fallback;
            // suppress that fallback for the rest of the session.
            self.e2ee_webdav_fallback_rejected = true;
            *self.sync_error.lock().unwrap() = None;
        }
        if self.effective_e2ee_password().is_some() {
            self.start_sync(ctx);
        } else {
            self.e2ee_input.clear();
            self.e2ee_prompt_error = None;
            self.e2ee_pending_sync = true;
            self.dialog = Dialog::E2eePrompt;
        }
    }

    /// Accept the E2EE prompt input: remember it for this session and, if the
    /// prompt was opened by a Sync click, start syncing immediately.
    fn submit_e2ee_prompt(&mut self, ctx: &egui::Context) {
        if self.e2ee_input.is_empty() {
            self.e2ee_prompt_error = Some("密码不能为空".into());
            return;
        }
        self.e2ee_password = self.e2ee_input.clone();
        self.e2ee_input.clear();
        self.e2ee_prompt_error = None;
        let start = self.e2ee_pending_sync;
        self.e2ee_pending_sync = false;
        self.dialog = Dialog::None;
        if start {
            self.start_sync(ctx);
        }
    }

    fn start_sync(&mut self, ctx: &egui::Context) {
        {
            let running = self.sync_running.lock().unwrap();
            if *running {
                return;
            }
        }
        *self.sync_running.lock().unwrap() = true;
        *self.sync_error.lock().unwrap() = None;
        self.status = "syncing...".into();

        let mut config = self.config.clone();
        // Single source of truth: the in-app/prompt field, then config, then
        // the WebDAV-password fallback (see effective_e2ee_password).
        config.e2ee_password = self.effective_e2ee_password();
        let data_dir = data_dir();
        let status = self.status_shared.clone();
        let running = self.sync_running.clone();
        let sync_error = self.sync_error.clone();
        let ctx = ctx.clone();

        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let status_async = status.clone();
            let result = rt.block_on(async move {
                let storage = Storage::open(&data_dir.join("joplin-rs.db"))?;
                let client = joplin_rs_core::webdav::WebDavClient::new_with_tls(
                    &config.webdav_url,
                    &config.username,
                    config.password.as_deref().unwrap_or(""),
                    &config.tls_options(),
                )?;
                let engine = joplin_rs_core::sync::SyncEngine::new(&client, &storage);
                let e2ee_pw = config.e2ee_password.as_deref();
                engine
                    .sync(e2ee_pw, &mut |msg| {
                        *status_async.lock().unwrap() = msg.to_string();
                    })
                    .await
            });
            let final_status = match &result {
                Ok(outcome) => format!(
                    "sync done: {} downloaded, {} deleted, {} resources",
                    outcome.downloaded, outcome.deleted, outcome.resources_downloaded
                ),
                Err(e) => {
                    *sync_error.lock().unwrap() = Some(e.to_string());
                    format!("sync failed: {e}")
                }
            };
            *status.lock().unwrap() = final_status;
            *running.lock().unwrap() = false;
            ctx.request_repaint();
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Launch lock: until the reading password is verified, show only the
        // lock modal and none of the note panels (note lists are not even
        // loaded yet, so titles/bodies cannot leak behind the lock).
        if !self.unlocked {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.weak("joplin-rs");
                });
            });
            self.show_lock_modal(ctx);
            return;
        }

        // Top bar
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("E2EE password:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.e2ee_password)
                        .password(true)
                        .desired_width(140.0),
                );
                if ui.button("Sync").clicked() {
                    self.request_sync(ctx);
                }
                self.status = self.status_shared.lock().unwrap().clone();
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
                            let title = note
                                .props
                                .get("title")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("(no title)")
                                .to_string();
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
                                self.open_note(&id);
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
                                ui.small(egui::RichText::new(snippet).weak().size(11.0));
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
            ui.heading(&self.note_title);
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
        }
    }
}

impl App {
    // ---- modal rendering ----

    /// Launch-lock password prompt. Cannot be dismissed without the password:
    /// backdrop clicks and Escape are intentionally ignored.
    fn show_lock_modal(&mut self, ctx: &egui::Context) {
        egui::Modal::new(egui::Id::new("reader_lock_modal"))
            .show(ctx, |ui| {
                ui.set_min_width(320.0);
                ui.heading("阅读密码");
                ui.add_space(4.0);
                ui.label("请输入阅读密码以打开 joplin-rs");
                ui.add_space(8.0);

                let input = egui::TextEdit::singleline(&mut self.lock_input)
                    .password(true)
                    .hint_text("阅读密码")
                    .desired_width(280.0);
                let input_response = ui.add(input);
                // Keep focus on the password field (including after a failed
                // attempt clears it) so typing and Enter always land here.
                if !input_response.has_focus() {
                    input_response.request_focus();
                }
                // egui's recommended singleline submit pattern: Enter surrenders
                // focus on the same frame, so lost_focus + Enter fires once.
                let submit =
                    input_response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

                if let Some(err) = &self.lock_error {
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(err).color(egui::Color32::from_rgb(200, 60, 60)),
                    );
                }
                ui.add_space(10.0);
                ui.vertical_centered(|ui| {
                    if ui.button("解锁").clicked() || submit {
                        self.try_unlock();
                    }
                });
            });
        // Swallow Escape/backdrop-close: the lock may only be cleared by the
        // correct password. Modal::should_close would otherwise dismiss it.
        ctx.input_mut(|i| {
            i.consume_key(egui::Modifiers::NONE, egui::Key::Escape);
        });
    }

    fn show_reader_pw_dialog(&mut self, ctx: &egui::Context) {
        let has_pw = self.has_reader_password();
        let response = egui::Modal::new(egui::Id::new("reader_pw_settings_modal"))
            .show(ctx, |ui| {
                ui.set_min_width(360.0);
                ui.heading(if has_pw { "修改阅读密码" } else { "设置阅读密码" });
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
                let submit = confirm_response.lost_focus()
                    && ui.input(|i| i.key_pressed(egui::Key::Enter));

                if let Some(err) = &self.rp_error {
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(err).color(egui::Color32::from_rgb(200, 60, 60)),
                    );
                }
                if let Some(msg) = &self.rp_message {
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(msg).color(egui::Color32::from_rgb(40, 140, 60)),
                    );
                }

                ui.add_space(10.0);
                let mut remove = false;
                let mut save = false;
                ui.horizontal(|ui| {
                    if has_pw && ui.button("取消密码").clicked() {
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
        let response = egui::Modal::new(egui::Id::new("e2ee_prompt_modal"))
            .show(ctx, |ui| {
                ui.set_min_width(360.0);
                ui.heading("E2EE 密码");
                ui.add_space(6.0);
                ui.label("同步需要 E2EE 主密钥密码，请输入后回车开始同步:");
                ui.add_space(8.0);

                let input = egui::TextEdit::singleline(&mut self.e2ee_input)
                    .password(true)
                    .hint_text("E2EE master key password")
                    .desired_width(320.0);
                let input_response = ui.add(input);
                if !input_response.has_focus() {
                    input_response.request_focus();
                }
                let submit = input_response.lost_focus()
                    && ui.input(|i| i.key_pressed(egui::Key::Enter));

                if let Some(err) = &self.e2ee_prompt_error {
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(err).color(egui::Color32::from_rgb(200, 60, 60)),
                    );
                }
                ui.add_space(10.0);
                let mut cancelled = false;
                let mut confirmed = false;
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("取消").clicked() {
                            cancelled = true;
                        }
                        if ui.button("开始同步").clicked() {
                            confirmed = true;
                        }
                    });
                });
                if cancelled {
                    self.e2ee_pending_sync = false;
                    self.dialog = Dialog::None;
                } else if confirmed || submit {
                    self.submit_e2ee_prompt(ctx);
                }
            });
        if response.should_close() {
            self.e2ee_pending_sync = false;
            self.dialog = Dialog::None;
        }
    }
}

/// Whether a sync error string indicates the E2EE password was wrong. Used to
/// decide whether the next Sync click must prompt for the password again.
fn looks_like_wrong_e2ee_password(err: &str) -> bool {
    err.contains("wrong password") || err.contains("no master key matched the password")
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

fn main() -> eframe::Result {
    eframe::run_native(
        "joplin-rs",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default().with_inner_size([1100.0, 720.0]),
            ..Default::default()
        },
        Box::new(|cc| {
            configure_fonts(&cc.egui_ctx);
            Ok(Box::new(App::new()))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::crate_resource_ids;

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
}
