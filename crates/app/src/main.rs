//! joplin-rs GUI: read-only note browser (egui).

use joplin_rs_core::items::Item;
use joplin_rs_core::storage::Storage;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Config {
    webdav_url: String,
    username: String,
    #[serde(default)]
    password: Option<String>,
}

fn config_path() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("joplin-rs").join("config.toml")
}

fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("JOPLIN_RS_DATA_DIR") {
        return PathBuf::from(dir);
    }
    dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("joplin-rs")
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
    status: String,
    sync_running: Arc<Mutex<bool>>,
    status_shared: Arc<Mutex<String>>,
    note_tags: std::collections::HashMap<String, Vec<String>>, // note_id -> tag titles
}

impl App {
    fn new() -> Self {
        let storage = Storage::open(&data_dir().join("joplin-rs.db")).expect("open storage");
        let config = std::fs::read_to_string(config_path())
            .ok()
            .and_then(|raw| toml::from_str(&raw).ok())
            .unwrap_or_default();
        let mut app = Self {
            storage,
            config,
            folders: Vec::new(),
            selected_folder: None,
            notes: Vec::new(),
            selected_note: None,
            note_body: String::new(),
            note_title: String::new(),
            search: String::new(),
            status: "ready".into(),
            sync_running: Arc::new(Mutex::new(false)),
            status_shared: Arc::new(Mutex::new(String::new())),
            note_tags: Default::default(),
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
            let note_id = nt.props.get("note_id").and_then(serde_json::Value::as_str).unwrap_or_default().to_string();
            let tag_id = nt.props.get("tag_id").and_then(serde_json::Value::as_str).unwrap_or_default().to_string();
            if let Some(title) = tag_titles.get(&tag_id) {
                map.entry(note_id).or_default().push(title.clone());
            }
        }
        self.note_tags = map;
    }

    fn load_notes(&mut self) {
        self.notes = if self.search.is_empty() {
            self.storage.notes(self.selected_folder.as_deref().unwrap_or(""), true).unwrap_or_default()
        } else {
            self.storage.search_notes(&self.search).unwrap_or_default()
        };
    }

    fn open_note(&mut self, id: &str) {
        if let Ok(Some(note)) = self.storage.note(id) {
            self.note_title = note.props.get("title").and_then(serde_json::Value::as_str).unwrap_or("").to_string();
            self.note_body = note.body.clone();
            self.selected_note = Some(id.to_string());
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
        self.status = "syncing...".into();

        let config = self.config.clone();
        let data_dir = data_dir();
        let status = self.status_shared.clone();
        let running = self.sync_running.clone();
        let ctx = ctx.clone();

        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let status_async = status.clone();
            let result = rt.block_on(async move {
                let storage = Storage::open(&data_dir.join("joplin-rs.db"))?;
                let client = joplin_rs_core::webdav::WebDavClient::new(
                    &config.webdav_url,
                    &config.username,
                    config.password.as_deref().unwrap_or(""),
                )?;
                let engine = joplin_rs_core::sync::SyncEngine::new(&client, &storage);
                engine
                    .sync(config.password.as_deref(), &mut |msg| {
                        *status_async.lock().unwrap() = msg.to_string();
                    })
                    .await
            });
            let final_status = match &result {
                Ok(outcome) => format!(
                    "sync done: {} downloaded, {} deleted, {} resources",
                    outcome.downloaded, outcome.deleted, outcome.resources_downloaded
                ),
                Err(e) => format!("sync failed: {e}"),
            };
            *status.lock().unwrap() = final_status;
            *running.lock().unwrap() = false;
            ctx.request_repaint();
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Top bar
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Sync").clicked() {
                    self.start_sync(ctx);
                }
                self.status = self.status_shared.lock().unwrap().clone();
                if self.status.is_empty() {
                    self.status = "ready".into();
                }
                ui.label(&self.status);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
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
        egui::SidePanel::left("left").min_width(280.0).show(ctx, |ui| {
            ui.heading("Notebooks");
            if ui.selectable_label(self.selected_folder.is_none(), "All notes").clicked() {
                self.selected_folder = None;
                self.load_notes();
            }
            egui::ScrollArea::vertical().max_height(180.0).id_salt("folders").show(ui, |ui| {
                let folders = self.folders.clone();
                for folder in &folders {
                    let id = folder.props.get("id").and_then(serde_json::Value::as_str).unwrap_or("").to_string();
                    let title = folder.props.get("title").and_then(serde_json::Value::as_str).unwrap_or("").to_string();
                    if ui.selectable_label(self.selected_folder.as_deref() == Some(id.as_str()), &title).clicked() {
                        self.selected_folder = Some(id);
                        self.load_notes();
                    }
                }
            });
            ui.separator();
            ui.heading("Notes");
            egui::ScrollArea::vertical().auto_shrink(false).id_salt("notes").show(ui, |ui| {
                let notes = self.notes.clone();
                let note_tags = self.note_tags.clone();
                for note in &notes {
                    let id = note.props.get("id").and_then(serde_json::Value::as_str).unwrap_or("").to_string();
                    let title = note.props.get("title").and_then(serde_json::Value::as_str).unwrap_or("(no title)").to_string();
                    let is_todo = note.props.get("is_todo").and_then(serde_json::Value::as_i64).unwrap_or(0) == 1;
                    let done = note.props.get("todo_completed").and_then(serde_json::Value::as_i64).unwrap_or(0) > 0;
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
                }
            });
        });

        // Right: rendered note
        egui::CentralPanel::default().show(ctx, |ui| {
            if self.selected_note.is_none() {
                ui.centered_and_justified(|ui| ui.weak("Select a note, then press Sync to download the vault."));
                return;
            }
            ui.heading(&self.note_title);
            ui.separator();
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                let mut cache = egui_commonmark::CommonMarkCache::default();
                egui_commonmark::CommonMarkViewer::new().show(ui, &mut cache, &self.note_body);
            });
        });
    }
}

fn main() -> eframe::Result {
    eframe::run_native(
        "joplin-rs",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default().with_inner_size([1100.0, 720.0]),
            ..Default::default()
        },
        Box::new(|_cc| Ok(Box::new(App::new()))),
    )
}
