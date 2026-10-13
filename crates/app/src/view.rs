//! GPUI root view for the note browser.
//!
//! Layout mirrors the egui UI it replaces: top toolbar (sync / status /
//! reader password / reload), search row, left sidebar (notebooks + notes
//! with lock/todo markers, tags, search snippets), central markdown panel
//! with the reading-password gate and encrypted placeholder. State lives in
//! `app::AppState` (framework agnostic); this layer only renders and wires
//! events. `cx.spawn` polls the job channel each frame budget.

use gpui::{
    actions, div, prelude::*, px, App, AppContext, Application, Bounds, Context, Entity,
    FocusHandle, Focusable, KeyBinding, Render, SharedString, Size,
    Window, WindowBounds, WindowOptions, black, rgb, white,
};

use crate::app::{AppState, Dialog, is_encrypted_placeholder, short_id};
use crate::jobs;
use crate::ui::dialog::{DialogAction, DialogState};
use crate::ui::input::InputState;
use crate::ui::markdown;

actions!(app, [Sync, Reload, OpenReaderPassword, ProtectNote, UnprotectNote, SubmitGate, Quit]);

const TOOLBAR_BG: u32 = 0xf4f4f5;
const BORDER: u32 = 0xd4d4d8;
const SIDEBAR_WIDTH: f32 = 280.;
const SELECTED_ROW: u32 = 0xe0e7ff;
const SNIPPET_TEXT: u32 = 0x71717a;
const ERROR_TEXT: u32 = 0xdc2626;
const OK_TEXT: u32 = 0x16a34a;

/// Register app-level key bindings.
pub fn register(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-r", Sync, None),
        KeyBinding::new("enter", SubmitGate, Some("NoteGate")),
    ]);
    crate::ui::input::register(cx);
    crate::ui::dialog::register(cx);
}

/// The root view; owns the framework-agnostic state as an entity sibling.
pub struct NoteBrowser {
    state: Entity<AppState>,
    search_input: Entity<InputState>,
    gate_input: Entity<InputState>,
    dialog: Option<Entity<DialogState>>,
    /// Dialog the state machine wants shown (mirrors app::Dialog).
    pending_dialog: Dialog,
    focus: FocusHandle,
}

impl Focusable for NoteBrowser {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl NoteBrowser {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let state = cx.new(|_| AppState::new().expect("open storage"));
        state.update(cx, |s, _| s.reload_meta());

        let search_input = cx.new(|cx| {
            InputState::new(cx)
                .placeholder("Search…")
                .on_change(|_text, _cx| {
                    // change callback runs with App only; the parent polls
                    // value() each frame, so search-as-you-type re-renders
                    // through the poll tick below.
                })
        });
        let gate_input = cx.new(|cx| InputState::new(cx).placeholder("阅读密码"));

        // Poll the job channel on a 150ms tick: worker progress lines and
        // finished events arrive through shared mutexes, the UI thread
        // applies them here and calls cx.notify(). Replaces egui's
        // per-frame handle_job_event + ctx.request_repaint loop.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(150))
                    .await;
                let this = this.upgrade();
                let Some(this) = this else { return };
                this.update(cx, |this, cx| {
                    // Mirror live worker progress into the status line.
                    this.state.update(cx, |s, _| {
                        let worker = s.jobs.worker_status();
                        if !worker.is_empty() {
                            s.status = worker;
                        }
                        let _ = s.handle_job_event();
                    });
                    // Auto-start the decrypt pass when a stored password is
                    // available (mirrors the egui flow's auto-continue).
                    this.state.update(cx, |s, _| jobs::after_sync_done(s));
                    this.sync_dialog_from_state(cx);
                    // Search-as-you-type: reload when the input changed.
                    let text = this.search_input.read(cx).value().to_string();
                    this.state.update(cx, |s, _| {
                        if s.search != text {
                            s.search = text.clone();
                            s.load_notes();
                        }
                    });
                    cx.notify();
                });
            }
        })
            .detach();

        Self {
            state,
            search_input,
            gate_input,
            dialog: None,
            pending_dialog: Dialog::None,
            focus: cx.focus_handle(),
        }
    }

    fn sync_clicked(&mut self, _: &Sync, _: &mut Window, cx: &mut Context<Self>) {
        self.state.update(cx, |s, _| jobs::start_download(s));
        cx.notify();
    }

    fn reload_clicked(&mut self, _: &Reload, _: &mut Window, cx: &mut Context<Self>) {
        self.state.update(cx, |s, _| s.reload_meta());
        cx.notify();
    }

    fn reader_password_clicked(&mut self, _: &OpenReaderPassword, _: &mut Window, cx: &mut Context<Self>) {
        self.state.update(cx, |s, _| s.open_reader_pw_settings());
        self.sync_dialog_from_state(cx);
        cx.notify();
    }

    fn sync_dialog_from_state(&mut self, cx: &mut Context<Self>) {
        let want = self.state.read(cx).dialog;
        self.pending_dialog = want;
        self.dialog = match want {
            Dialog::None => None,
            Dialog::ReaderPasswordSettings => Some(cx.new(|_cx| {
                DialogState::new("阅读密码设置", move |action, cx| match action {
                    DialogAction::Confirm => {
                        // The parent view's form fields live in AppState;
                        // confirm applies and closes.
                        // Real submission happens in the password form below.
                        let _ = cx;
                    }
                    DialogAction::Dismiss => {
                        let _ = cx;
                    }
                })
            })),
            Dialog::E2eePrompt => Some(cx.new(|_cx| {
                DialogState::new("E2EE 密码", move |_action, _cx| {})
            })),
            #[cfg(not(target_os = "macos"))]
            Dialog::About => Some(cx.new(|cx| {
                DialogState::new("About joplin-rs", move |_action, _cx| {})
            })),
        };
    }
}

impl Render for NoteBrowser {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        let status = self.state.read(cx).status.clone();
        let busy = self.state.read(cx).jobs.is_busy();
        let worker = self.state.read(cx).jobs.worker_status();
        let status_line = if !worker.is_empty() {
            worker
        } else if status.is_empty() {
            "ready".to_string()
        } else {
            status
        };

        div()
            .id("root")
            .key_context("NoteBrowser")
            .on_action(cx.listener(Self::sync_clicked))
            .on_action(cx.listener(Self::reload_clicked))
            .on_action(cx.listener(Self::reader_password_clicked))
            .size_full()
            .flex()
            .flex_col()
            .bg(white())
            .text_color(black())
            // ---- toolbar ----
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .bg(rgb(TOOLBAR_BG))
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .id("btn-sync")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .border_1()
                            .border_color(rgb(BORDER))
                            .bg(white())
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(0xe4e4e7)))
                            .child(if busy { "Sync…" } else { "Sync" })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.state.update(cx, |s, _| jobs::start_download(s));
                                cx.notify();
                            })),
                    )
                    .child(div().flex_1().text_color(rgb(0x52525b)).child(SharedString::from(status_line)))
                    .child(
                        div()
                            .id("btn-reader-pw")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(0xe4e4e7)))
                            .child("阅读密码")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.state.update(cx, |s, _| s.open_reader_pw_settings());
                                this.sync_dialog_from_state(cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id("btn-reload")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(0xe4e4e7)))
                            .child("Reload")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.state.update(cx, |s, _| s.reload_meta());
                                cx.notify();
                            })),
                    ),
            )
            // ---- search row ----
            .child(
                div()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(self.search_input.clone()),
            )
            // ---- body: sidebar + note panel ----
            .child(
                div()
                    .flex_1()
                    .flex()
                    .min_h_0()
                    .child(self.render_sidebar(cx))
                    .child(self.render_note_panel(cx)),
            )
            // ---- modal dialogs ----
            .children(self.dialog.clone())
    }
}

impl NoteBrowser {
    fn render_sidebar(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let state = self.state.read(cx);
        let folders = state.folders.clone();
        let selected_folder = state.selected_folder.clone();
        let notes = state.notes.clone();
        let note_tags = state.note_tags.clone();
        let search_hits = state.search_hits.clone();
        let protected = state.protected.clone();
        let selected_note = state.selected_note.clone();

        div()
            .w(px(SIDEBAR_WIDTH))
            .flex_shrink_0()
            .border_r_1()
            .border_color(rgb(BORDER))
            .flex()
            .flex_col()
            .bg(rgb(0xfafafa))
            .child(
                div().px_3().py_2().font_weight(gpui::FontWeight::BOLD).child("Notebooks"),
            )
            .child(
                div()
                    .id("folder-all")
                    .px_3()
                    .py_1()
                    .cursor_pointer()
                    .when(selected_folder.is_none(), |el| el.bg(rgb(SELECTED_ROW)))
                    .hover(|s| s.bg(rgb(0xf0f0f1)))
                    .child("All notes")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.state.update(cx, |s, _| {
                            s.selected_folder = None;
                            s.load_notes();
                        });
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .id("folder-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .children(folders.iter().map(|folder| {
                        let id = folder.props.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let title = folder.props.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let selected = selected_folder.as_deref() == Some(id.as_str());
                        div()
                            .id(SharedString::from(format!("folder-{id}")))
                            .px_3()
                            .py_1()
                            .cursor_pointer()
                            .when(selected, |el| el.bg(rgb(SELECTED_ROW)))
                            .hover(|s| s.bg(rgb(0xf0f0f1)))
                            .child(SharedString::from(title))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.state.update(cx, |s, _| {
                                    s.selected_folder = Some(id.clone());
                                    s.load_notes();
                                });
                                cx.notify();
                            }))
                    })),
            )
            .child(
                div()
                    .px_3()
                    .py_2()
                    .font_weight(gpui::FontWeight::BOLD)
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .child("Notes"),
            )
            .child(
                div()
                    .id("note-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(notes.iter().map(|note| {
                        let id = note.props.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let encrypted = is_encrypted_placeholder(note);
                        let is_protected = protected.contains(&id);
                        let raw_title = note.props.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let mut title = if encrypted {
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
                        if is_protected {
                            title = format!("🔐 {title}");
                        }
                        let is_todo = note.props.get("is_todo").and_then(|v| v.as_i64()).unwrap_or(0) == 1;
                        let done = note.props.get("todo_completed").and_then(|v| v.as_i64()).unwrap_or(0) > 0;
                        let label = if is_todo {
                            format!("{} {}", if done { "☑" } else { "☐" }, title)
                        } else {
                            title
                        };
                        let selected = selected_note.as_deref() == Some(id.as_str());
                        let tags = note_tags.get(&id).cloned().unwrap_or_default();
                        let snippet = search_hits.get(&id).cloned();

                        div()
                            .id(SharedString::from(format!("note-{id}")))
                            .px_3()
                            .py_1()
                            .cursor_pointer()
                            .when(selected, |el| el.bg(rgb(SELECTED_ROW)))
                            .hover(|s| s.bg(rgb(0xf0f0f1)))
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .child(SharedString::from(label))
                            .when(!tags.is_empty(), |el| {
                                el.child(
                                    div()
                                        .flex()
                                        .gap_1()
                                        .text_size(px(11.))
                                        .text_color(rgb(SNIPPET_TEXT))
                                        .children(tags.iter().map(|t| SharedString::from(format!("#{t}")))),
                                )
                            })
                            .when(snippet.is_some() && !encrypted && !is_protected, |el| {
                                el.child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(rgb(SNIPPET_TEXT))
                                        .child(SharedString::from(snippet.unwrap_or_default())),
                                )
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let note_id = id.clone();
                                this.state.update(cx, |s, _| s.request_open_note(&note_id));
                                cx.notify();
                            }))
                    })),
            )
            .into_any_element()
    }

    fn render_note_panel(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let state = self.state.read(cx);
        if state.selected_note.is_none() {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(rgb(0x71717a))
                .child("Select a note, then press Sync to download the vault.")
                .into_any_element();
        }
        // Reading-password gate: title visible, body gated.
        if state.note_locked {
            let error = state.prot_error.clone();
            let gate = self.gate_input.clone();
            return div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .key_context("NoteGate")
                .child(
                    div()
                        .text_size(px(16.))
                        .font_weight(gpui::FontWeight::BOLD)
                        .child(SharedString::from(state.note_title.clone())),
                )
                .child(gate)
                .children(error.map(|e| {
                    div().text_color(rgb(ERROR_TEXT)).child(SharedString::from(e))
                }))
                .child(
                    div()
                        .id("btn-unlock")
                        .px_3()
                        .py_1()
                        .rounded_sm()
                        .bg(rgb(0x2563eb))
                        .text_color(white())
                        .cursor_pointer()
                        .child("解锁")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.state.update(cx, |s, _| s.try_unlock_note());
                            cx.notify();
                        })),
                )
                .into_any_element();
        }
        // Encrypted placeholder selected.
        if state.selected_note_encrypted {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(rgb(0x71717a))
                .child(SharedString::from(format!(
                    "🔒 此笔记已加密，等待 E2EE 解密（{}）",
                    short_id(state.selected_note.as_deref().unwrap_or(""))
                )))
                .into_any_element();
        }
        let title = state.note_title.clone();
        let body = state.note_body.clone();
        let is_protected = state
            .selected_note
            .as_deref()
            .map(|id| state.protected.contains(id))
            .unwrap_or(false);
        let resource_dir = state.resource_dir.clone();

        div()
            .flex_1()
            .flex()
            .flex_col()
            .min_w_0()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_4()
                    .py_2()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_weight(gpui::FontWeight::BOLD)
                            .child(SharedString::from(title)),
                    )
                    .child(
                        div()
                            .id("btn-protect")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .border_1()
                            .border_color(rgb(BORDER))
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(0xf4f4f5)))
                            .child(if is_protected { "取消保护" } else { "🔒 保护此笔记" })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.state.update(cx, |s, _| {
                                    if s.protected.contains(s.selected_note.as_deref().unwrap_or("")) {
                                        s.gate_open_note();
                                    } else {
                                        s.protect_open_note();
                                    }
                                });
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .id("note-body-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_4()
                    .py_3()
                    .child(markdown::render_markdown(&body, &resource_dir)),
            )
            .into_any_element()
    }
}

impl AppState {
    /// Placeholder hook kept for on_action wiring; real logic lives in
    /// jobs::start_download which takes &mut AppState directly.
    pub(crate) fn request_sync_marker(&mut self) {}
}

/// GPUI entry point behind the `gpui-view` feature. The default binary
/// still runs the egui UI (`main` in main.rs); this mirrors it one-for-one
/// until the egui frontend is retired.
pub fn gpui_main() {
    Application::new().run(|cx: &mut App| {
        register(cx);
        let bounds = Bounds::centered(None, Size::new(px(1100.), px(720.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_window, cx| cx.new(NoteBrowser::new),
        )
        .expect("open main window");
        cx.activate(true);
    });
}
