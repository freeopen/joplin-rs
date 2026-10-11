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
    /// E2EE master key password (the in-app field overrides this)
    #[serde(default)]
    e2ee_password: Option<String>,
    /// Accept self-signed/invalid TLS certificates
    #[serde(default)]
    insecure_tls: bool,
    /// Path to a PEM CA certificate to trust
    #[serde(default)]
    ca_cert: Option<String>,
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

fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("JOPLIN_RS_DATA_DIR") {
        return PathBuf::from(dir);
    }
    default_base_dir()
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
    resource_dir: PathBuf,
    note_tags: std::collections::HashMap<String, Vec<String>>, // note_id -> tag titles
}

impl App {
    fn new() -> Self {
        let storage = Storage::open(&data_dir().join("joplin-rs.db")).expect("open storage");
        let config: Config = std::fs::read_to_string(config_path())
            .ok()
            .and_then(|raw| toml::from_str(&raw).ok())
            .unwrap_or_default();
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
            resource_dir: PathBuf::new(),
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

    fn start_sync(&mut self, ctx: &egui::Context) {
        {
            let running = self.sync_running.lock().unwrap();
            if *running {
                return;
            }
        }
        *self.sync_running.lock().unwrap() = true;
        self.status = "syncing...".into();

        let mut config = self.config.clone();
        // Prefer the in-app password field over config.toml. Note: config.password
        // is the WebDAV password; the GUI e2ee field maps to config.e2ee_password.
        if !self.e2ee_password.is_empty() {
            config.e2ee_password = Some(self.e2ee_password.clone());
        }
        if config.e2ee_password.is_none() {
            config.e2ee_password = config.password.clone();
        }
        let data_dir = data_dir();
        let status = self.status_shared.clone();
        let running = self.sync_running.clone();
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
                ui.label("E2EE password:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.e2ee_password)
                        .password(true)
                        .desired_width(140.0),
                );
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
