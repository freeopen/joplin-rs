//! GPUI root view for the note browser, built on gpui-kit components.
//!
//! Layout mirrors the egui UI it replaces: top toolbar (sync / status /
//! reader password / reload), search row, left sidebar (notebooks + notes
//! with lock/todo markers, tags, search snippets), central markdown panel
//! with the reading-password gate and encrypted placeholder. State lives in
//! `app::AppState` (framework agnostic); this layer only renders and wires
//! events.
//!
//! Components (gpui-component 0.7.3): single-line `Input` over persistent
//! `InputState`s (search-as-you-type via `InputEvent::Change`, submit via
//! `InputEvent::PressEnter`), modal dialogs through `WindowExt::open_dialog`
//! / `open_alert_dialog` (Esc/scrim-close built in), and note bodies through
//! `gpui_base::TextView::markdown` (GFM, tables, task lists, text selection).
//! A 150ms `spawn_in` tick mirrors `JobChannel` worker progress, consumes
//! job events, and syncs the dialog flag; search needs no polling.

use std::borrow::Cow;
use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::{
    button::{Button, ButtonVariants},
    dialog::{DialogAction, DialogFooter},
    input::{Copy, Cut, Input, InputEvent, InputState, Paste, SelectAll, Textarea, TextareaState},
    scroll::Scrollbar,
    ActiveTheme, Disableable, Sizable, WindowExt,
};
use gpui_kit::{
    div, prelude::*, px, relative, App, Bounds, Context, Entity, FocusHandle, Focusable, Image,
    ImageFormat, ImageSource, KeyBinding, Menu, MenuItem, Render, Resource, ScrollHandle,
    SharedString, SharedUri, Size, Subscription, Window, WindowBounds, WindowOptions,
};

use crate::app::{is_encrypted_placeholder, short_id, AppState, Dialog as AppDialog};
use crate::jobs;

gpui_kit::actions!(app, [Sync, Reload, OpenReaderPassword, Quit, ShowAbout]);

const NOTEBOOKS_WIDTH: f32 = 200.;
const NOTES_WIDTH: f32 = 300.;

/// Register app-level key bindings and OS menus.
fn register(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("cmd-r", Sync, None)]);
    cx.on_action(|_: &Quit, cx| cx.quit());
    // macOS shows the native About panel (rounded icon, version); other
    // platforms open the in-window About dialog via the toolbar button.
    #[cfg(target_os = "macos")]
    cx.on_action(|_: &ShowAbout, _| crate::macos_about::show_native_about());
    install_menus(cx);
}

/// Build the OS menu bar. Menu actions are dispatched app-globally, so the
/// menu only carries entries that work without a window context: Quit,
/// About (macOS native panel), and the os-action edit items, whose cocoa
/// selectors (cut:/copy:/paste:/selectAll:) hit the focused input through
/// the first-responder chain.
fn install_menus(cx: &mut App) {
    use gpui_kit::OsAction;

    #[cfg(target_os = "macos")]
    let menus = vec![
        Menu::new("joplin-rs").items(vec![
            MenuItem::action("About joplin-rs", ShowAbout),
            MenuItem::separator(),
            MenuItem::action("Quit joplin-rs", Quit),
        ]),
        Menu::new("File").items(vec![MenuItem::action("Sync", Sync)]),
        Menu::new("Edit").items(vec![
            MenuItem::os_action("Cut", Cut, OsAction::Cut),
            MenuItem::os_action("Copy", Copy, OsAction::Copy),
            MenuItem::os_action("Paste", Paste, OsAction::Paste),
            MenuItem::os_action("Select All", SelectAll, OsAction::SelectAll),
        ]),
    ];
    #[cfg(not(target_os = "macos"))]
    let menus = vec![
        Menu::new("File").items(vec![MenuItem::action("Quit", Quit)]),
        Menu::new("Edit").items(vec![
            MenuItem::os_action("Cut", Cut, OsAction::Cut),
            MenuItem::os_action("Copy", Copy, OsAction::Copy),
            MenuItem::os_action("Paste", Paste, OsAction::Paste),
            MenuItem::os_action("Select All", SelectAll, OsAction::SelectAll),
        ]),
    ];
    cx.set_menus(menus);
}

/// The root view; owns the framework-agnostic state as an entity sibling.
pub struct NoteBrowser {
    state: Entity<AppState>,
    /// Persistent input entities: `InputState::new` requires `&mut Window`,
    /// which event callbacks do not have, so every field the app ever shows
    /// is created once at startup and cleared with `set_value("")` when the
    /// owning dialog opens (mirrors the egui per-open field lifecycle).
    search_input: Entity<InputState>,
    gate_input: Entity<InputState>,
    rp_current: Entity<InputState>,
    rp_new: Entity<InputState>,
    rp_confirm: Entity<InputState>,
    e2ee_input: Entity<InputState>,
    /// Note editor fields (title single-line, body multi-line).
    title_edit: Entity<InputState>,
    note_edit: Entity<TextareaState>,
    /// New/rename notebook name field.
    nb_input: Entity<InputState>,
    /// Scroll handles backing the list scrollbars.
    notebook_scroll: ScrollHandle,
    note_scroll: ScrollHandle,
    /// Last seen `edit_epoch`: the persistent editor fields are (re)seeded
    /// into `set_value` when it changes.
    edit_epoch_seen: u64,
    /// Dialog the state machine wants shown (mirrors app::Dialog).
    pending_dialog: AppDialog,
    /// Guard: only auto-focus a dialog's first field once per opening.
    rp_dialog_focused: Rc<Cell<bool>>,
    e2ee_dialog_focused: Rc<Cell<bool>>,
    /// Last seen `prot_gate_epoch`: when AppState bumps it a protected note
    /// (re)locked, and the persistent gate field must be cleared so a
    /// re-opened gate never shows or reuses the previous password.
    gate_epoch_seen: u64,
    focus: FocusHandle,
    #[allow(dead_code)]
    subscriptions: Vec<Subscription>,
}

impl Focusable for NoteBrowser {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl NoteBrowser {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let state = cx.new(|_| AppState::new().expect("open storage"));
        state.update(cx, |s, _| s.reload_meta());

        let search_input = cx.new(|cx| InputState::new(window, cx).placeholder("Search…"));
        let gate_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("阅读密码")
                .masked(true)
        });
        let rp_current = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("当前密码")
                .masked(true)
        });
        let rp_new = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("新密码")
                .masked(true)
        });
        let rp_confirm = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("确认新密码")
                .masked(true)
        });
        let e2ee_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("E2EE 主密码")
                .masked(true)
        });
        let title_edit = cx.new(|cx| InputState::new(window, cx).placeholder("标题"));
        let note_edit = cx.new(|cx| TextareaState::new(window, cx).placeholder("Markdown 正文…"));
        let nb_input = cx.new(|cx| InputState::new(window, cx).placeholder("笔记本名称"));

        let mut subscriptions = Vec::new();
        // Note title: mirror into the working copy on change; save on blur.
        subscriptions.push(cx.subscribe(
            &title_edit,
            |this: &mut Self, state, event: &InputEvent, cx| {
                match event {
                    InputEvent::Change => {
                        let text = state.read(cx).value().to_string();
                        this.state.update(cx, |s, _| {
                            s.edit_title = text;
                            s.mark_edit_dirty();
                        });
                    }
                    InputEvent::Blur => {
                        this.state.update(cx, |s, _| {
                            s.save_edit();
                        });
                    }
                    _ => return,
                }
                cx.notify();
            },
        ));
        // Note body: mirror into the working copy on change; save on blur.
        subscriptions.push(cx.subscribe(
            &note_edit,
            |this: &mut Self, state, event: &InputEvent, cx| {
                match event {
                    InputEvent::Change => {
                        let text = state.read(cx).value().to_string();
                        this.state.update(cx, |s, _| {
                            s.edit_body = text;
                            s.mark_edit_dirty();
                        });
                    }
                    InputEvent::Blur => {
                        this.state.update(cx, |s, _| {
                            s.save_edit();
                        });
                    }
                    _ => return,
                }
                cx.notify();
            },
        ));
        // New/rename notebook dialog field.
        subscriptions.push(cx.subscribe(
            &nb_input,
            |this: &mut Self, state, event: &InputEvent, cx| {
                if !matches!(event, InputEvent::Change) {
                    return;
                }
                let text = state.read(cx).value().to_string();
                this.state.update(cx, |s, _| s.nb_input = text);
            },
        ));
        // Search-as-you-type: reload the note list whenever the search text
        // changes (replaces the old per-tick value polling).
        subscriptions.push(cx.subscribe(
            &search_input,
            |this: &mut Self, state, event: &InputEvent, cx| {
                if !matches!(event, InputEvent::Change) {
                    return;
                }
                let text = state.read(cx).value().to_string();
                this.state.update(cx, |s, _| {
                    if s.search != text {
                        s.end_edit();
                        s.search = text;
                        s.load_notes();
                    }
                });
                cx.notify();
            },
        ));
        // Note-gate fields mirror into AppState on every change; unlock with
        // Enter or the button.
        subscriptions.push(cx.subscribe(
            &gate_input,
            |this: &mut Self, state, event: &InputEvent, cx| {
                match event {
                    InputEvent::Change => {
                        let text = state.read(cx).value().to_string();
                        this.state.update(cx, |s, _| s.prot_input = text);
                    }
                    InputEvent::PressEnter { .. } => {
                        this.gate_unlock(cx);
                    }
                    _ => return,
                }
                cx.notify();
            },
        ));
        // E2EE prompt: mirror the field into AppState so the submit handler
        // sees the current text even if Enter arrives before blur.
        subscriptions.push(cx.subscribe(
            &e2ee_input,
            |this: &mut Self, state, event: &InputEvent, cx| {
                if !matches!(event, InputEvent::Change) {
                    return;
                }
                let text = state.read(cx).value().to_string();
                this.state.update(cx, |s, _| s.e2ee_input = text);
            },
        ));
        // Reader-password fields mirror into AppState on change.
        for (field, which) in [
            (rp_current.clone(), 0usize),
            (rp_new.clone(), 1usize),
            (rp_confirm.clone(), 2usize),
        ] {
            subscriptions.push(cx.subscribe(
                &field,
                move |this: &mut Self, state, event: &InputEvent, cx| {
                    if !matches!(event, InputEvent::Change) {
                        return;
                    }
                    let text = state.read(cx).value().to_string();
                    this.state.update(cx, |s, _| match which {
                        0 => s.rp_current = text,
                        1 => s.rp_new = text,
                        _ => s.rp_confirm = text,
                    });
                },
            ));
        }

        // Poll the job channel on a 150ms tick: worker progress lines and
        // finished events arrive through shared mutexes, the UI thread
        // applies them here and calls cx.notify(). Replaces egui's
        // per-frame handle_job_event + ctx.request_repaint loop.
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(150))
                    .await;
                let this = this.upgrade();
                let Some(this) = this else { return };
                let _ = cx.update(|window, cx| {
                    this.update(cx, |this, cx| {
                        // Mirror live worker progress into the status line.
                        this.state.update(cx, |s, _| {
                            let worker = s.jobs.worker_status();
                            if !worker.is_empty() {
                                s.status = worker;
                            }
                            let _ = s.handle_job_event();
                        });
                        // Auto-start the decrypt pass when a not-yet-tried
                        // password is available (mirrors the egui flow's
                        // auto-continue; honours the sticky cancel flag).
                        this.state.update(cx, |s, _| jobs::auto_decrypt(s));
                        // Debounced autosave (3s after the last change).
                        this.state.update(cx, |s, _| s.autosave_tick());
                        // (Re)seed the editor fields when edit mode (re)starts;
                        // `set_value` emits no Change event, so this does not
                        // mark the note dirty.
                        let edit_epoch = this.state.read(cx).edit_epoch;
                        if edit_epoch != this.edit_epoch_seen {
                            this.edit_epoch_seen = edit_epoch;
                            let (title, body) = {
                                let s = this.state.read(cx);
                                (s.edit_title.clone(), s.edit_body.clone())
                            };
                            this.title_edit
                                .update(cx, |i, cx| i.set_value(title, window, cx));
                            this.note_edit
                                .update(cx, |i, cx| i.set_value(body, window, cx));
                        }
                        // A protected note (re)locked: clear the gate field so
                        // the re-opened gate never shows the last password.
                        // (`set_value` emits no Change event, so AppState's
                        // already-cleared `prot_input` stays in sync.)
                        let epoch = this.state.read(cx).prot_gate_epoch;
                        if epoch != this.gate_epoch_seen {
                            this.gate_epoch_seen = epoch;
                            this.gate_input
                                .update(cx, |input, cx| input.set_value("", window, cx));
                        }
                        this.sync_dialog_from_state(window, cx);
                        cx.notify();
                    });
                });
            }
        })
        .detach();

        Self {
            state,
            search_input,
            gate_input,
            rp_current,
            rp_new,
            rp_confirm,
            e2ee_input,
            title_edit,
            note_edit,
            nb_input,
            notebook_scroll: ScrollHandle::new(),
            note_scroll: ScrollHandle::new(),
            edit_epoch_seen: 0,
            pending_dialog: AppDialog::None,
            rp_dialog_focused: Rc::new(Cell::new(false)),
            e2ee_dialog_focused: Rc::new(Cell::new(false)),
            gate_epoch_seen: 0,
            focus: cx.focus_handle(),
            subscriptions,
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

    fn reader_password_clicked(
        &mut self,
        _: &OpenReaderPassword,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.state.update(cx, |s, _| s.open_reader_pw_settings());
        self.sync_dialog_from_state(window, cx);
        cx.notify();
    }

    /// Unlock the gated note from the password in `gate_input`. On failure
    /// AppState clears the input and sets the error; the field is cleared to
    /// match so a wrong attempt does not linger.
    fn gate_unlock(&mut self, cx: &mut Context<Self>) {
        let text = self.gate_input.read(cx).value().to_string();
        self.state.update(cx, |s, _| {
            s.prot_input = text;
            s.try_unlock_note();
        });
    }

    /// Reader-password dialog: collect the three fields into AppState and
    /// submit. Returns true to close the dialog.
    fn submit_rp_dialog(&mut self, cx: &mut App) -> bool {
        let read = |e: &Entity<InputState>| e.read(cx).value().to_string();
        let (cur, new, confirm) = (
            read(&self.rp_current),
            read(&self.rp_new),
            read(&self.rp_confirm),
        );
        self.state.update(cx, |s, _| {
            s.rp_current = cur;
            s.rp_new = new;
            s.rp_confirm = confirm;
            s.submit_reader_pw();
        });
        self.state.read(cx).rp_error.is_none()
    }

    /// Remove the global reading password via the dialog's 移除密码 button.
    fn remove_rp(&mut self, cx: &mut App) {
        let cur = self.rp_current.read(cx).value().to_string();
        self.state.update(cx, |s, _| {
            s.rp_current = cur;
            s.remove_reader_pw();
        });
    }

    /// E2EE prompt: submit the password and kick off decryption when the
    /// state machine accepted it.
    fn submit_e2ee_dialog(&mut self, cx: &mut App) -> bool {
        let text = self.e2ee_input.read(cx).value().to_string();
        self.state.update(cx, |s, _| {
            s.e2ee_input = text;
            s.submit_e2ee_prompt();
        });
        let accepted = self.state.read(cx).dialog == AppDialog::None;
        if accepted {
            // Run a full two-way sync: the session password now present both
            // decrypts the pending items and lets local changes be uploaded.
            self.state.update(cx, |s, _| jobs::start_download(s));
            self.pending_dialog = AppDialog::None;
        }
        accepted
    }

    /// Open/close the modal layer from the state machine's dialog flag.
    /// The state machine stays the single source of truth; every component
    /// close path (Esc, scrim, Cancel, on_ok) either returns to this sync or
    /// sets `dialog = None` itself and lets the next tick close the layer.
    fn sync_dialog_from_state(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let want = self.state.read(cx).dialog;
        if want == self.pending_dialog {
            return;
        }
        // Close whatever is open before (re)opening; active_dialogs is a
        // stack and we only ever show one dialog at a time.
        if window.has_active_dialog(cx) {
            window.close_dialog(cx);
        }
        self.pending_dialog = want;
        match want {
            AppDialog::None => {}
            AppDialog::ReaderPasswordSettings => self.open_rp_dialog(window, cx),
            AppDialog::E2eePrompt => self.open_e2ee_dialog(window, cx),
            AppDialog::NewNotebook | AppDialog::RenameNotebook => {
                self.open_notebook_dialog(window, cx, want)
            }
            AppDialog::ConfirmDeleteNote | AppDialog::ConfirmDeleteNotebook => {
                self.open_confirm_delete_dialog(window, cx, want)
            }
            #[cfg(not(target_os = "macos"))]
            AppDialog::About => self.open_about_dialog(window, cx),
        }
        window.refresh();
    }

    fn open_rp_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let browser = cx.entity().downgrade();
        let current = self.rp_current.clone();
        let new = self.rp_new.clone();
        let confirm = self.rp_confirm.clone();
        // Fresh fields per opening (egui lifecycle); set_value emits no
        // Change event, so AppState's cleared values and the fields agree.
        for input in [&current, &new, &confirm] {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        }
        self.rp_dialog_focused.set(false);
        let focused = self.rp_dialog_focused.clone();
        let has_password = self.state.read(cx).has_reader_password();
        window.open_dialog(cx, move |dialog, window, cx| {
            // Auto-focus the first field only on the builder's first
            // invocation per opening (it may run again on layer rebuilds).
            if !focused.replace(true) {
                let first = if has_password { &current } else { &new };
                let handle = first.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
            }
            let browser_ok = browser.clone();
            let browser_remove = browser.clone();
            let browser_cancel = browser.clone();
            let browser_close = browser.clone();
            let error = browser
                .update(cx, |view, cx| view.state.read(cx).rp_error.clone())
                .ok()
                .flatten();
            let message = browser
                .update(cx, |view, cx| view.state.read(cx).rp_message.clone())
                .ok()
                .flatten();
            dialog
                .title(if has_password {
                    "修改阅读密码"
                } else {
                    "设置阅读密码"
                })
                .w(px(420.))
                .content({
                    let current = current.clone();
                    let new = new.clone();
                    let confirm = confirm.clone();
                    let error = error.clone();
                    let message = message.clone();
                    move |content, _window, _cx| {
                        content
                            .child(hint_line(
                                "阅读密码不锁定整个应用，仅用于打开被标记为“🔐 受保护”的笔记；每次打开都需输入。",
                            ))
                            .when(has_password, |el| {
                                el.child(form_row("当前密码", current.clone()))
                            })
                            .child(form_row("新密码", new.clone()))
                            .child(form_row("确认新密码", confirm.clone()))
                            .when_some(error.clone(), |el, err| el.child(error_line(err)))
                            .when_some(message.clone(), |el, msg| el.child(ok_line(msg)))
                    }
                })
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("rp-remove")
                                .danger()
                                .outline()
                                .label("移除密码")
                                .disabled(!has_password)
                                .on_click(move |_, _, cx| {
                                    browser_remove
                                        .update(cx, |view, cx| {
                                            view.remove_rp(cx);
                                            if view.state.read(cx).rp_error.is_none() {
                                                view.state
                                                    .update(cx, |s, _| s.dialog = AppDialog::None);
                                                view.pending_dialog = AppDialog::None;
                                            }
                                        })
                                        .ok();
                                }),
                        )
                        .child(
                            Button::new("rp-cancel")
                                .label("取消")
                                .on_click(move |_, window, cx| {
                                    browser_cancel
                                        .update(cx, |view, cx| {
                                            view.state.update(cx, |s, _| {
                                                s.dialog = AppDialog::None;
                                            });
                                            view.pending_dialog = AppDialog::None;
                                        })
                                        .ok();
                                    window.close_dialog(cx);
                                }),
                        )
                        .child(
                            DialogAction::new().child(
                                Button::new("rp-save")
                                    .primary()
                                    .label(if has_password { "保存" } else { "设置" }),
                            ),
                        ),
                )
                .on_ok(move |_, _, cx| {
                    browser_ok
                        .update(cx, |view, cx| {
                            if view.submit_rp_dialog(cx) {
                                view.state.update(cx, |s, _| s.dialog = AppDialog::None);
                                view.pending_dialog = AppDialog::None;
                                true
                            } else {
                                cx.notify();
                                false
                            }
                        })
                        .unwrap_or(true)
                })
                .on_cancel(move |_, _, cx| {
                    browser_close
                        .update(cx, |view, cx| {
                            view.state.update(cx, |s, _| s.dialog = AppDialog::None);
                            view.pending_dialog = AppDialog::None;
                        })
                        .ok();
                    true
                })
        });
    }

    fn open_e2ee_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let browser = cx.entity().downgrade();
        let input = self.e2ee_input.clone();
        input.update(cx, |input, cx| input.set_value("", window, cx));
        self.e2ee_dialog_focused.set(false);
        let focused = self.e2ee_dialog_focused.clone();
        let pending = self.state.read(cx).e2ee_pending_count;
        window.open_dialog(cx, move |dialog, window, cx| {
            if !focused.replace(true) {
                let handle = input.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
            }
            let browser_ok = browser.clone();
            let browser_cancel = browser.clone();
            let browser_close = browser.clone();
            let error = browser
                .update(cx, |view, cx| view.state.read(cx).e2ee_prompt_error.clone())
                .ok()
                .flatten();
            let input = input.clone();
            dialog
                .title("E2EE 密码")
                .w(px(420.))
                .content({
                    let input = input.clone();
                    let error = error.clone();
                    move |content, _window, _cx| {
                        content
                            .child(hint_line(&format!(
                                "{pending} 项已加密，输入 E2EE 主密码解密（仅本次会话使用）"
                            )))
                            .child(form_row("密码", input.clone()))
                            .when_some(error.clone(), |el, err| el.child(error_line(err)))
                    }
                })
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("e2ee-cancel")
                                .label("取消（保持加密）")
                                .on_click(move |_, window, cx| {
                                    browser_cancel
                                        .update(cx, |view, cx| {
                                            view.state.update(cx, |s, _| s.cancel_e2ee_prompt());
                                            view.pending_dialog = AppDialog::None;
                                        })
                                        .ok();
                                    window.close_dialog(cx);
                                }),
                        )
                        .child(
                            DialogAction::new()
                                .child(Button::new("e2ee-ok").primary().label("开始解密")),
                        ),
                )
                .on_ok(move |_, _, cx| {
                    browser_ok
                        .update(cx, |view, cx| {
                            if view.submit_e2ee_dialog(cx) {
                                true
                            } else {
                                cx.notify();
                                false
                            }
                        })
                        .unwrap_or(true)
                })
                .on_cancel(move |_, _, cx| {
                    browser_close
                        .update(cx, |view, cx| {
                            view.state.update(cx, |s, _| s.cancel_e2ee_prompt());
                            view.pending_dialog = AppDialog::None;
                        })
                        .ok();
                    true
                })
        });
    }

    /// Modal for creating or renaming a notebook.
    fn open_notebook_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        which: AppDialog,
    ) {
        let browser = cx.entity().downgrade();
        let is_new = matches!(which, AppDialog::NewNotebook);
        let input = self.nb_input.clone();
        let initial = self.state.read(cx).nb_input.clone();
        input.update(cx, |i, cx| i.set_value(initial, window, cx));
        window.open_dialog(cx, move |dialog, window, cx| {
            let handle = input.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
            let browser_ok = browser.clone();
            let browser_cancel = browser.clone();
            let browser_close = browser.clone();
            let error = browser
                .update(cx, |view, cx| view.state.read(cx).nb_error.clone())
                .ok()
                .flatten();
            let input = input.clone();
            dialog
                .title(if is_new {
                    "新建笔记本"
                } else {
                    "重命名笔记本"
                })
                .w(px(360.))
                .content({
                    let input = input.clone();
                    let error = error.clone();
                    move |content, _window, _cx| {
                        content
                            .child(form_row("名称", input.clone()))
                            .when_some(error.clone(), |el, err| el.child(error_line(err)))
                    }
                })
                .footer(
                    DialogFooter::new()
                        .child(Button::new("nb-cancel").label("取消").on_click(
                            move |_, window, cx| {
                                browser_cancel
                                    .update(cx, |view, cx| {
                                        view.state.update(cx, |s, _| s.dialog = AppDialog::None);
                                        view.pending_dialog = AppDialog::None;
                                    })
                                    .ok();
                                window.close_dialog(cx);
                            },
                        ))
                        .child(DialogAction::new().child(
                            Button::new("nb-save").primary().label(if is_new {
                                "创建"
                            } else {
                                "保存"
                            }),
                        )),
                )
                .on_ok(move |_, _, cx| {
                    browser_ok
                        .update(cx, |view, cx| {
                            let ok = if is_new {
                                view.state.update(cx, |s, _| s.submit_new_notebook())
                            } else {
                                view.state.update(cx, |s, _| s.submit_rename_notebook())
                            };
                            if ok {
                                view.pending_dialog = AppDialog::None;
                            }
                            ok
                        })
                        .unwrap_or(true)
                })
                .on_cancel(move |_, _, cx| {
                    browser_close
                        .update(cx, |view, cx| {
                            view.state.update(cx, |s, _| s.dialog = AppDialog::None);
                            view.pending_dialog = AppDialog::None;
                        })
                        .ok();
                    true
                })
        });
    }

    /// Confirmation modal shown before deleting a note or a notebook.
    fn open_confirm_delete_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        which: AppDialog,
    ) {
        let browser = cx.entity().downgrade();
        let is_note = matches!(which, AppDialog::ConfirmDeleteNote);
        let (title, description) = if is_note {
            let note_title = self.state.read(cx).note_title.clone();
            (
                "删除笔记",
                format!("确定删除笔记「{note_title}」？下次同步时会同时删除远端文件。"),
            )
        } else {
            ("删除笔记本", "确定删除该笔记本？".to_string())
        };
        window.open_dialog(cx, move |dialog, _window, _cx| {
            let browser_ok = browser.clone();
            let browser_cancel = browser.clone();
            let browser_close = browser.clone();
            let description = description.clone();
            dialog
                .title(title)
                .w(px(400.))
                .content(move |content, _window, _cx| content.child(hint_line(&description)))
                .footer(
                    DialogFooter::new()
                        .child(Button::new("del-cancel").label("取消").on_click(
                            move |_, window, cx| {
                                browser_cancel
                                    .update(cx, |view, cx| {
                                        view.state.update(cx, |s, _| s.dialog = AppDialog::None);
                                        view.pending_dialog = AppDialog::None;
                                    })
                                    .ok();
                                window.close_dialog(cx);
                            },
                        ))
                        .child(
                            DialogAction::new().child(Button::new("del-ok").danger().label("删除")),
                        ),
                )
                .on_ok(move |_, _, cx| {
                    browser_ok
                        .update(cx, |view, cx| {
                            view.state.update(cx, |s, _| {
                                if is_note {
                                    s.delete_selected_note();
                                } else {
                                    s.delete_selected_notebook();
                                }
                            });
                            view.pending_dialog = AppDialog::None;
                            true
                        })
                        .unwrap_or(true)
                })
                .on_cancel(move |_, _, cx| {
                    browser_close
                        .update(cx, |view, cx| {
                            view.state.update(cx, |s, _| s.dialog = AppDialog::None);
                            view.pending_dialog = AppDialog::None;
                        })
                        .ok();
                    true
                })
        });
    }

    #[cfg(not(target_os = "macos"))]
    fn open_about_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let browser = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _window, _cx| {
            let browser = browser.clone();
            alert
                .title("About joplin-rs")
                .description(format!(
                    "{}\n\n只读的 Joplin 客户端（WebDAV + 端到端加密）\n应用图标为 Joplin 官方图标，版权归 Joplin 项目所有",
                    crate::APP_VERSION
                ))
                .ok_text("关闭")
                .on_ok(move |_, _, cx| {
                    browser
                        .update(cx, |view, cx| {
                            view.state.update(cx, |s, _| s.dialog = AppDialog::None);
                            view.pending_dialog = AppDialog::None;
                        })
                        .ok();
                    true
                })
        });
    }
}

impl Render for NoteBrowser {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let status = self.state.read(cx).status.clone();
        let busy = self.state.read(cx).jobs.is_busy();
        let worker = self.state.read(cx).jobs.worker_status();
        let show_notebooks = self.state.read(cx).show_notebooks;
        let show_notes = self.state.read(cx).show_notes;
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
            .bg(theme.background)
            .text_color(theme.foreground)
            // ---- toolbar ----
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .bg(theme.secondary)
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        Button::new("btn-sync")
                            .primary()
                            .small()
                            .label(if busy { "Sync…" } else { "Sync" })
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.state.update(cx, |s, _| jobs::start_download(s));
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_color(theme.muted_foreground)
                            .child(SharedString::from(status_line)),
                    )
                    .child(
                        Button::new("btn-reader-pw")
                            .small()
                            .outline()
                            .label("阅读密码")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.state.update(cx, |s, _| s.open_reader_pw_settings());
                                this.sync_dialog_from_state(window, cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("btn-reload")
                            .small()
                            .ghost()
                            .label("Reload")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.state.update(cx, |s, _| s.reload_meta());
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("toggle-notebooks")
                            .small()
                            .outline()
                            .label(if show_notebooks {
                                "隐藏笔记本"
                            } else {
                                "显示笔记本"
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.state.update(cx, |s, _| {
                                    s.show_notebooks = !s.show_notebooks;
                                });
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("toggle-notes")
                            .small()
                            .outline()
                            .label(if show_notes {
                                "隐藏笔记"
                            } else {
                                "显示笔记"
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.state.update(cx, |s, _| s.show_notes = !s.show_notes);
                                cx.notify();
                            })),
                    )
                    // macOS has the native app-menu About panel; other
                    // platforms open the in-window dialog from the toolbar.
                    .child(
                        Button::new("btn-about")
                            .small()
                            .ghost()
                            .label("About")
                            .on_click(cx.listener(|_this, _, _window, _cx| {
                                #[cfg(target_os = "macos")]
                                {
                                    crate::macos_about::show_native_about();
                                }
                                #[cfg(not(target_os = "macos"))]
                                {
                                    _this.state.update(_cx, |s, _| s.dialog = AppDialog::About);
                                    _this.sync_dialog_from_state(_window, _cx);
                                    _cx.notify();
                                }
                            })),
                    ),
            )
            // ---- search row ----
            .child(
                div()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(Input::new(&self.search_input).id("search").cleanable(true)),
            )
            // ---- body: notebooks | note preview | notes ----
            .child(
                div()
                    .flex_1()
                    .flex()
                    .min_h_0()
                    .when(show_notebooks, |el| {
                        el.child(self.render_notebooks_column(cx))
                    })
                    .child(self.render_note_panel(window, cx))
                    .when(show_notes, |el| el.child(self.render_notes_column(cx))),
            )
    }
}

impl NoteBrowser {
    fn render_notebooks_column(&self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let theme = cx.theme();
        let state = self.state.read(cx);
        let folders = state.folders.clone();
        let selected_folder = state.selected_folder.clone();
        let has_selection = selected_folder.is_some();

        div()
            .w(px(NOTEBOOKS_WIDTH))
            .flex_shrink_0()
            .border_r_1()
            .border_color(theme.border)
            .flex()
            .flex_col()
            .bg(theme.muted)
            // Header: title + actions (new / rename / delete / hide).
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_2()
                    .py_1()
                    .child(
                        div()
                            .font_weight(gpui_kit::FontWeight::BOLD)
                            .child("Notebooks"),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_0p5()
                            .child(Button::new("nb-new").ghost().small().label("＋").on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.state.update(cx, |s, _| s.open_new_notebook());
                                    this.sync_dialog_from_state(window, cx);
                                    cx.notify();
                                }),
                            ))
                            .child(
                                Button::new("nb-rename")
                                    .ghost()
                                    .small()
                                    .label("✎")
                                    .disabled(!has_selection)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.state.update(cx, |s, _| s.open_rename_notebook());
                                        this.sync_dialog_from_state(window, cx);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("nb-delete")
                                    .ghost()
                                    .small()
                                    .label("🗑")
                                    .disabled(!has_selection)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.state.update(cx, |s, _| s.request_delete_notebook());
                                        this.sync_dialog_from_state(window, cx);
                                        cx.notify();
                                    })),
                            )
                            .child(Button::new("nb-hide").ghost().small().label("⯇").on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.state.update(cx, |s, _| s.show_notebooks = false);
                                    cx.notify();
                                }),
                            )),
                    ),
            )
            .child(
                div()
                    .id("folder-all")
                    .px_3()
                    .py_1()
                    .cursor_pointer()
                    .when(selected_folder.is_none(), |el| {
                        el.bg(theme.selection.opacity(0.35))
                    })
                    .hover(|s| s.bg(theme.selection.opacity(0.15)))
                    .child("All notes")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.state.update(cx, |s, _| {
                            s.end_edit();
                            s.selected_folder = None;
                            s.load_notes();
                        });
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .id("folder-list-wrap")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .id("folder-list")
                            .size_full()
                            .track_scroll(&self.notebook_scroll)
                            .overflow_y_scroll()
                            .children(folders.iter().map(|folder| {
                                let id = folder
                                    .props
                                    .get("id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let title = folder
                                    .props
                                    .get("title")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let selected = selected_folder.as_deref() == Some(id.as_str());
                                div()
                                    .id(SharedString::from(format!("folder-{id}")))
                                    .px_3()
                                    .py_1()
                                    .cursor_pointer()
                                    .when(selected, |el| el.bg(theme.selection.opacity(0.35)))
                                    .hover(|s| s.bg(theme.selection.opacity(0.15)))
                                    .child(SharedString::from(title))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.state.update(cx, |s, _| {
                                            s.end_edit();
                                            s.selected_folder = Some(id.clone());
                                            s.load_notes();
                                        });
                                        cx.notify();
                                    }))
                            })),
                    )
                    .child(Scrollbar::vertical(&self.notebook_scroll)),
            )
            .into_any_element()
    }

    fn render_notes_column(&self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let theme = cx.theme();
        let state = self.state.read(cx);
        let notes = state.notes.clone();
        let note_tags = state.note_tags.clone();
        let search_hits = state.search_hits.clone();
        let protected = state.protected.clone();
        let selected_note = state.selected_note.clone();
        let has_folder = state.selected_folder.is_some();
        let has_note = selected_note.is_some();

        div()
            .w(px(NOTES_WIDTH))
            .flex_shrink_0()
            .border_l_1()
            .border_color(theme.border)
            .flex()
            .flex_col()
            .bg(theme.muted)
            // Header: title + actions (new / delete / hide).
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_2()
                    .py_1()
                    .child(div().font_weight(gpui_kit::FontWeight::BOLD).child("Notes"))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_0p5()
                            .child(
                                Button::new("note-new")
                                    .ghost()
                                    .small()
                                    .label("＋")
                                    .disabled(!has_folder)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.state.update(cx, |s, _| s.new_note());
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("note-delete")
                                    .ghost()
                                    .small()
                                    .label("🗑")
                                    .disabled(!has_note)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.state.update(cx, |s, _| s.request_delete_note());
                                        this.sync_dialog_from_state(window, cx);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("note-hide")
                                    .ghost()
                                    .small()
                                    .label("⯈")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.state.update(cx, |s, _| s.show_notes = false);
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .child(
                div()
                    .id("note-list-wrap")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .id("note-list")
                            .size_full()
                            .track_scroll(&self.note_scroll)
                            .overflow_y_scroll()
                            .children(notes.iter().map(|note| {
                                let id = note
                                    .props
                                    .get("id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let encrypted = is_encrypted_placeholder(note);
                                let is_protected = protected.contains(&id);
                                let raw_title = note
                                    .props
                                    .get("title")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
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
                                let is_todo = note
                                    .props
                                    .get("is_todo")
                                    .and_then(|v| v.as_i64())
                                    .unwrap_or(0)
                                    == 1;
                                let done = note
                                    .props
                                    .get("todo_completed")
                                    .and_then(|v| v.as_i64())
                                    .unwrap_or(0)
                                    > 0;
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
                                    .when(selected, |el| el.bg(theme.selection.opacity(0.35)))
                                    .hover(|s| s.bg(theme.selection.opacity(0.15)))
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
                                                .text_color(theme.muted_foreground)
                                                .children(
                                                    tags.iter().map(|t| {
                                                        SharedString::from(format!("#{t}"))
                                                    }),
                                                ),
                                        )
                                    })
                                    .when(snippet.is_some() && !encrypted && !is_protected, |el| {
                                        el.child(
                                            div()
                                                .text_size(px(11.))
                                                .text_color(theme.muted_foreground)
                                                .child(SharedString::from(
                                                    snippet.unwrap_or_default(),
                                                )),
                                        )
                                    })
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        let note_id = id.clone();
                                        this.state.update(cx, |s, _| {
                                            // Save any pending edit before switching.
                                            s.end_edit();
                                            s.request_open_note(&note_id);
                                        });
                                        // A protected note (re)locks here: drop any
                                        // password still sitting in the gate field so
                                        // re-opening never reuses it.
                                        if this.state.read(cx).note_locked {
                                            this.gate_input.update(cx, |input, cx| {
                                                input.set_value("", window, cx)
                                            });
                                            this.gate_epoch_seen =
                                                this.state.read(cx).prot_gate_epoch;
                                        }
                                        cx.notify();
                                    }))
                            })),
                    )
                    .child(Scrollbar::vertical(&self.note_scroll)),
            )
            .into_any_element()
    }

    fn render_note_panel(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let theme = cx.theme();
        let state = self.state.read(cx);
        if state.selected_note.is_none() {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.muted_foreground)
                .child("Select a note, then press Sync to download the vault.")
                .into_any_element();
        }
        // Reading-password gate: title visible, body gated.
        if state.note_locked {
            let error = state.prot_error.clone();
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
                        .font_weight(gpui_kit::FontWeight::BOLD)
                        .child(SharedString::from(state.note_title.clone())),
                )
                .child(
                    div()
                        .w(px(300.))
                        .child(Input::new(&self.gate_input).id("gate")),
                )
                .children(error.map(error_line))
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(
                            Button::new("btn-unprotect")
                                .outline()
                                .label("取消保护")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    let text = this.gate_input.read(cx).value().to_string();
                                    this.state.update(cx, |s, _| {
                                        s.prot_input = text;
                                        s.unprotect_gated_note();
                                    });
                                    cx.notify();
                                })),
                        )
                        .child(Button::new("btn-unlock").primary().label("解锁").on_click(
                            cx.listener(|this, _, _, cx| {
                                this.gate_unlock(cx);
                                cx.notify();
                            }),
                        )),
                )
                .into_any_element();
        }
        // Encrypted placeholder selected.
        if state.selected_note_encrypted {
            return div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .text_color(theme.muted_foreground)
                .child(div().text_size(px(28.)).child("🔒"))
                .child("此笔记已端到端加密，尚未解密")
                .child(SharedString::from(format!(
                    "先完成同步，再提供正确的 E2EE 密码即可查看（{}）",
                    short_id(state.selected_note.as_deref().unwrap_or(""))
                )))
                .into_any_element();
        }
        let title = state.note_title.clone();
        let body = state.note_body.clone();
        let editing = state.editing;
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
                    .gap_2()
                    .px_4()
                    .py_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(if editing {
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&self.title_edit).id("note-title"))
                    } else {
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(16.))
                            .font_weight(gpui_kit::FontWeight::BOLD)
                            .child(SharedString::from(title))
                    })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .flex_shrink_0()
                            .child(
                                Button::new("btn-edit-toggle")
                                    .outline()
                                    .small()
                                    .label(if editing { "预览" } else { "编辑" })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.state.update(cx, |s, _| {
                                            if s.editing {
                                                s.end_edit();
                                            } else {
                                                s.begin_edit();
                                            }
                                        });
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("btn-delete-note")
                                    .outline()
                                    .small()
                                    .danger()
                                    .label("删除笔记")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.state.update(cx, |s, _| s.request_delete_note());
                                        this.sync_dialog_from_state(window, cx);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("btn-protect")
                                    .outline()
                                    .small()
                                    .label(if is_protected {
                                        "取消保护"
                                    } else {
                                        "🔒 保护此笔记"
                                    })
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.state.update(cx, |s, _| {
                                            if s.protected
                                                .contains(s.selected_note.as_deref().unwrap_or(""))
                                            {
                                                s.gate_open_note();
                                            } else {
                                                s.protect_open_note();
                                            }
                                        });
                                        this.sync_dialog_from_state(window, cx);
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .child(if editing {
                div()
                    .flex_1()
                    .min_h_0()
                    .px_4()
                    .py_3()
                    .child(Textarea::new(&self.note_edit).h(relative(1.)))
                    .into_any_element()
            } else {
                div()
                    .id("note-body-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_4()
                    .py_3()
                    .child(render_markdown(&body, &resource_dir))
                    .into_any_element()
            })
            .into_any_element()
    }
}

/// One labeled form row for dialogs (label left, field right).
fn form_row(label: &str, field: Entity<InputState>) -> gpui_kit::AnyElement {
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .w(px(96.))
                .flex_shrink_0()
                .child(SharedString::from(label.to_string())),
        )
        .child(div().flex_1().child(Input::new(&field)))
        .into_any_element()
}

/// Muted hint line inside a dialog form.
fn hint_line(text: &str) -> gpui_kit::AnyElement {
    div()
        .text_size(px(12.))
        .child(SharedString::from(text.to_string()))
        .into_any_element()
}

/// Error line for dialog validation failures.
fn error_line(err: String) -> gpui_kit::AnyElement {
    div()
        .text_size(px(12.))
        .child(SharedString::from(err))
        .into_any_element()
}

/// Success line for dialog confirmations.
fn ok_line(msg: String) -> gpui_kit::AnyElement {
    div()
        .text_size(px(12.))
        .child(SharedString::from(msg))
        .into_any_element()
}

/// Render the note body as Markdown. Local Joplin resources were rewritten
/// by app.rs to absolute paths, so `image_source` routes those through
/// `Resource::Path`; `data:` URLs decode inline; http(s) URLs keep the
/// document's own URI handling (and degrade to links when the load fails).
fn render_markdown(body: &str, resource_dir: &Path) -> gpui_kit::AnyElement {
    let dir = resource_dir.to_path_buf();
    gpui_kit::base::TextView::markdown("note-body", body.to_string())
        .image_source(move |uri: &SharedUri| -> ImageSource { image_source_for(uri, &dir) })
        .on_link_click(|url, _, _, cx| {
            cx.open_url(url);
        })
        .into_any_element()
}

/// Resolve one document image URL to an [`ImageSource`].
///
/// - `data:` URLs are decoded inline (the default handler only embeds
///   `data:` when the parser saw them; an explicit override makes the
///   behaviour uniform).
/// - Remote URLs (http/https) stay URI-backed so the platform loader can
///   fetch them; failures degrade to the alt/link rendering.
/// - Anything else is treated as a local path. Joplin resource links were
///   already rewritten to absolute decrypted paths by `app.rs`; a bare
///   relative name is resolved against the resource directory.
fn image_source_for(uri: &SharedUri, resource_dir: &std::path::Path) -> ImageSource {
    let raw: &str = uri.as_ref();
    if let Some(image) = decode_data_url_image(raw) {
        return ImageSource::Image(Arc::new(image));
    }
    if raw.starts_with("http://") || raw.starts_with("https://") {
        return ImageSource::Resource(Resource::Uri(uri.clone()));
    }
    if let Some(path) = raw.strip_prefix("file://") {
        return ImageSource::Resource(Resource::Path(PathBuf::from(path).into()));
    }
    let path = PathBuf::from(raw);
    let path = if path.is_absolute() {
        path
    } else {
        resource_dir.join(path)
    };
    ImageSource::Resource(Resource::Path(path.into()))
}

/// Decode a `data:image/...;base64,...` URL into a [`gpui_kit::Image`].
/// Returns `None` for non-data URLs or undecodable payloads, letting the
/// caller fall back to the URI-backed loader.
fn decode_data_url_image(url: &str) -> Option<Image> {
    let rest = url.strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    let mime = meta.split(';').next()?;
    let format = ImageFormat::from_mime_type(mime)?;
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .ok()?;
    Some(Image::from_bytes(format, bytes))
}

/// Initialise the CJK font for the app. GPUI panics when text is laid out in
/// a family it cannot find; the theme probes `.SystemUIFont`/mono defaults at
/// startup, but a user-supplied `JOPLIN_RS_FONT` file is registered here and
/// named on the theme directly (family name taken from the font tables).
fn init_fonts(cx: &mut App) {
    let Ok(path) = std::env::var("JOPLIN_RS_FONT") else {
        return;
    };
    let path = PathBuf::from(path);
    let Ok(bytes) = std::fs::read(&path) else {
        tracing::warn!("JOPLIN_RS_FONT: cannot read {}", path.display());
        return;
    };
    let Some(family) = first_family_name(&bytes) else {
        tracing::warn!(
            "JOPLIN_RS_FONT: cannot determine family name of {}",
            path.display()
        );
        return;
    };
    if let Err(e) = cx.text_system().add_fonts(vec![Cow::Owned(bytes)]) {
        tracing::warn!("JOPLIN_RS_FONT: cannot register font: {e}");
        return;
    }
    let family: SharedString = family.into();
    gpui_kit::component::theme::Theme::global_mut(cx).font_family = family.clone();
    gpui_kit::component::theme::Theme::global_mut(cx).mono_font_family = family;
}

/// Read the first family name from a .ttf/.otf/.ttc font file without a
/// parser dependency: walk the name table for name-id 1 (family) and decode
/// the UTF-16BE (platform 3) or MacRoman (platform 1) string.
fn first_family_name(bytes: &[u8]) -> Option<String> {
    // An sfnt offset table is 12 bytes: tag/scaler (4) + numTables (2) +
    // searchRange/entrySelector/rangeShift (6), with the table records from
    // offset 12. A .ttc starts with 'ttcf' + version (4) + numFonts (4) +
    // one u32 file offset per member font, so slice to the first member and
    // parse it as plain sfnt instead of assuming it follows the header.
    let font: &[u8] = if bytes.get(..4) == Some(b"ttcf") {
        let off = u32::from_be_bytes(bytes.get(12..16)?.try_into().ok()?) as usize;
        bytes.get(off..)?
    } else {
        bytes
    };
    let num_tables = u16::from_be_bytes(font.get(4..6)?.try_into().ok()?) as usize;
    let mut dir = 12;
    for _ in 0..num_tables {
        let record = font.get(dir..dir + 16)?;
        let name_offset = u32::from_be_bytes(record.get(8..12)?.try_into().ok()?) as usize;
        if &record[0..4] == b"name" {
            return family_from_name_table(font.get(name_offset..)?);
        }
        dir += 16;
    }
    None
}

fn family_from_name_table(table: &[u8]) -> Option<String> {
    let count = u16::from_be_bytes(table.get(2..4)?.try_into().ok()?) as usize;
    let string_offset = u16::from_be_bytes(table.get(4..6)?.try_into().ok()?) as usize;
    let mut best: Option<String> = None;
    for i in 0..count {
        let rec = table.get(6 + i * 12..6 + i * 12 + 12)?;
        let platform = u16::from_be_bytes(rec.get(0..2)?.try_into().ok()?);
        let name_id = u16::from_be_bytes(rec.get(6..8)?.try_into().ok()?);
        if name_id != 1 {
            continue;
        }
        let length = u16::from_be_bytes(rec.get(8..10)?.try_into().ok()?) as usize;
        let offset = u16::from_be_bytes(rec.get(10..12)?.try_into().ok()?) as usize;
        let raw = table.get(string_offset + offset..string_offset + offset + length)?;
        match platform {
            // Windows: UTF-16BE. Prefer it over the Mac name.
            3 => {
                let name: String = raw
                    .chunks_exact(2)
                    .map(|c| u16::from_be_bytes([c[0], c[1]]))
                    .map(|c| char::from_u32(c as u32).unwrap_or_default())
                    .collect();
                return Some(name);
            }
            // Macintosh roman: keep as a fallback.
            1 => {
                best = Some(raw.iter().map(|&b| b as char).collect());
            }
            _ => {}
        }
    }
    best
}

/// GPUI entry point.
pub fn gpui_main() {
    gpui_kit::application().run(|cx: &mut App| {
        gpui_kit::init(cx);
        init_fonts(cx);
        register(cx);
        let bounds = Bounds::centered(None, Size::new(px(1100.), px(720.)), cx);
        let icon =
            image::load_from_memory_with_format(crate::APP_ICON_PNG, image::ImageFormat::Png)
                .ok()
                .map(|img| Arc::new(img.to_rgba8()));
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui_kit::TitlebarOptions {
                    title: Some("joplin-rs".into()),
                    ..Default::default()
                }),
                icon,
                ..Default::default()
            },
            cx,
            |window, cx| cx.new(|cx| NoteBrowser::new(window, cx)),
        )
        .expect("open main window");
        cx.activate(true);
        // The GPUI app menu exists now; retarget its About item to the
        // custom panel and set the rounded dock icon.
        #[cfg(target_os = "macos")]
        crate::macos_about::install_native_about();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal single-font sfnt blob whose `name` table carries one
    /// Windows (platform 3) family name.
    fn synth_sfnt(family: &str) -> Vec<u8> {
        // name table: format(2) + count(2) + stringOffset(2) + one record(12)
        // + UTF-16BE string.
        let raw: Vec<u8> = family.encode_utf16().flat_map(u16::to_be_bytes).collect();
        let name_table_len = 6 + 12 + raw.len();
        let mut name = vec![0u8, 0];
        name.extend_from_slice(&1u16.to_be_bytes()); // count
        name.extend_from_slice(&(6 + 12u16).to_be_bytes()); // string offset
                                                            // record: platform 3, encoding 1, language 0, name-id 1
        name.extend_from_slice(&3u16.to_be_bytes());
        name.extend_from_slice(&1u16.to_be_bytes());
        name.extend_from_slice(&0u16.to_be_bytes());
        name.extend_from_slice(&1u16.to_be_bytes());
        name.extend_from_slice(&(raw.len() as u16).to_be_bytes());
        name.extend_from_slice(&0u16.to_be_bytes());
        name.extend_from_slice(&raw);

        // Offset table (12) + one table record (16) + name table, 4-aligned.
        let name_offset = 12 + 16;
        let mut blob = Vec::with_capacity(name_offset + name_table_len);
        blob.extend_from_slice(&[0x00, 0x01, 0x00, 0x00]); // sfnt version
        blob.extend_from_slice(&1u16.to_be_bytes()); // numTables
        blob.extend_from_slice(&[0; 6]); // searchRange etc.
        blob.extend_from_slice(b"name");
        blob.extend_from_slice(&[0; 4]); // checksum
        blob.extend_from_slice(&(name_offset as u32).to_be_bytes());
        blob.extend_from_slice(&(name_table_len as u32).to_be_bytes());
        blob.extend_from_slice(&name);
        blob
    }

    /// Wrap a single-font blob in a .ttc header whose first member sits at
    /// a non-adjacent offset, exercising the member-font lookup.
    fn wrap_ttc(first_member_offset: u32) -> impl Fn(Vec<u8>) -> Vec<u8> {
        move |font| {
            let mut ttc = Vec::new();
            ttc.extend_from_slice(b"ttcf");
            ttc.extend_from_slice(&[0, 1, 0, 0]); // major.minor
            ttc.extend_from_slice(&1u32.to_be_bytes()); // numFonts
            ttc.extend_from_slice(&first_member_offset.to_be_bytes());
            // Padding between the header and the first member font.
            while ttc.len() < first_member_offset as usize {
                ttc.push(0);
            }
            ttc.extend_from_slice(&font);
            ttc
        }
    }

    #[test]
    fn family_from_plain_ttf() {
        assert_eq!(
            first_family_name(&synth_sfnt("Test Sans")),
            Some("Test Sans".into())
        );
    }

    #[test]
    fn family_from_ttc_member_offset() {
        let wrap = wrap_ttc(32);
        assert_eq!(
            first_family_name(&wrap(synth_sfnt("Songti SC"))),
            Some("Songti SC".into())
        );
    }

    #[test]
    fn family_from_ttc_min_offset() {
        // The offset table starts right after the 12-byte header, so the
        // smallest legal member offset is 16.
        let wrap = wrap_ttc(16);
        assert_eq!(
            first_family_name(&wrap(synth_sfnt("Hiragino"))),
            Some("Hiragino".into())
        );
    }

    #[test]
    fn family_none_on_garbage() {
        assert_eq!(first_family_name(b"nope"), None);
        assert_eq!(first_family_name(b""), None);
        // ttcf header truncated before the member offset.
        assert_eq!(first_family_name(b"ttcf\x00\x01\x00\x00"), None);
    }
}
