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
    input::{
        Copy, Cut, Delete, Input, InputEvent, InputState, MoveDown, MoveLeft, MovePageDown,
        MovePageUp, MoveRight, MoveToEnd, MoveToStart, MoveUp, Paste, Position, Redo, Search,
        SelectAll, Textarea, TextareaState, Undo,
    },
    menu::{ContextMenuExt, PopupMenuItem},
    scroll::Scrollbar,
    ActiveTheme, Disableable, Sizable, WindowExt,
};
use gpui_kit::{canvas, point};
use gpui_kit::{
    div, prelude::*, px, relative, App, Bounds, Context, Entity, FocusHandle, Focusable, Image,
    ImageFormat, ImageSource, KeyBinding, KeyDownEvent, Menu, MenuItem, Render, Resource,
    ScrollHandle, SharedString, SharedUri, Size, Subscription, Window, WindowBounds, WindowOptions,
};

use crate::app::{is_encrypted_placeholder, short_id, AppState, Dialog as AppDialog};
use crate::jobs;
use crate::vim::{self, MoveKind, VimAction, VimMode};

gpui_kit::actions!(
    app,
    [
        Sync,
        OpenReaderPassword,
        OpenSettings,
        ToggleNotebooks,
        ToggleNotes,
        FocusNotebooks,
        FocusNotes,
        FocusBody,
        FindInNote,
        GlobalSearch,
        ToggleEdit,
        Quit,
        ShowAbout
    ]
);

const NOTEBOOKS_WIDTH: f32 = 200.;
const NOTES_WIDTH: f32 = 300.;

/// Register app-level key bindings and OS menus.
fn register(cx: &mut App) {
    // macOS uses Cmd; other platforms mirror the layout with Ctrl.
    #[cfg(target_os = "macos")]
    let bindings = [
        KeyBinding::new("cmd-r", Sync, None),
        KeyBinding::new("cmd-b", ToggleNotes, None),
        KeyBinding::new("cmd-alt-b", ToggleNotebooks, None),
        KeyBinding::new("cmd-1", FocusNotebooks, None),
        KeyBinding::new("cmd-2", FocusBody, None),
        KeyBinding::new("cmd-3", FocusNotes, None),
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-f", FindInNote, None),
        KeyBinding::new("cmd-e", ToggleEdit, None),
        KeyBinding::new("cmd-alt-f", GlobalSearch, None),
        KeyBinding::new("cmd-,", OpenSettings, None),
    ];
    #[cfg(not(target_os = "macos"))]
    let bindings = [
        KeyBinding::new("ctrl-r", Sync, None),
        KeyBinding::new("ctrl-b", ToggleNotes, None),
        KeyBinding::new("ctrl-alt-b", ToggleNotebooks, None),
        KeyBinding::new("ctrl-1", FocusNotebooks, None),
        KeyBinding::new("ctrl-2", FocusBody, None),
        KeyBinding::new("ctrl-3", FocusNotes, None),
        KeyBinding::new("ctrl-q", Quit, None),
        KeyBinding::new("ctrl-f", FindInNote, None),
        KeyBinding::new("ctrl-e", ToggleEdit, None),
        KeyBinding::new("ctrl-alt-f", GlobalSearch, None),
        KeyBinding::new("ctrl-,", OpenSettings, None),
    ];
    cx.bind_keys(bindings);
    // Fallback quit for when no view handles the action (the view normally
    // intercepts Quit to sync pending changes first and stops propagation).
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
            MenuItem::action("Settings…", OpenSettings),
            MenuItem::separator(),
            MenuItem::action("Quit joplin-rs", Quit),
        ]),
        Menu::new("File").items(vec![MenuItem::action("Sync", Sync)]),
        Menu::new("Edit").items(vec![
            MenuItem::os_action("Cut", Cut, OsAction::Cut),
            MenuItem::os_action("Copy", Copy, OsAction::Copy),
            MenuItem::os_action("Paste", Paste, OsAction::Paste),
            MenuItem::os_action("Select All", SelectAll, OsAction::SelectAll),
            MenuItem::separator(),
            MenuItem::action("Find in Note", FindInNote),
            MenuItem::action("Search All Notes", GlobalSearch),
        ]),
        Menu::new("View").items(vec![
            MenuItem::action("Toggle Notebooks", ToggleNotebooks),
            MenuItem::action("Toggle Notes", ToggleNotes),
            MenuItem::separator(),
            MenuItem::action("Toggle Edit/Preview", ToggleEdit),
        ]),
    ];
    #[cfg(not(target_os = "macos"))]
    let menus = vec![
        Menu::new("File").items(vec![
            MenuItem::action("Quit", Quit),
            MenuItem::action("Settings…", OpenSettings),
            MenuItem::action("Sync", Sync),
        ]),
        Menu::new("Edit").items(vec![
            MenuItem::os_action("Cut", Cut, OsAction::Cut),
            MenuItem::os_action("Copy", Copy, OsAction::Copy),
            MenuItem::os_action("Paste", Paste, OsAction::Paste),
            MenuItem::os_action("Select All", SelectAll, OsAction::SelectAll),
            MenuItem::separator(),
            MenuItem::action("Find in Note", FindInNote),
            MenuItem::action("Search All Notes", GlobalSearch),
        ]),
        Menu::new("View").items(vec![
            MenuItem::action("Toggle Notebooks", ToggleNotebooks),
            MenuItem::action("Toggle Notes", ToggleNotes),
            MenuItem::separator(),
            MenuItem::action("Toggle Edit/Preview", ToggleEdit),
        ]),
        Menu::new("Help").items(vec![MenuItem::action("About joplin-rs", ShowAbout)]),
    ];
    cx.set_menus(menus);
}

/// One yank/delete register for the minimal vim layer. `linewise` text has no
/// trailing newline; `p`/`P` insert it as whole lines instead of inline.
#[derive(Debug, Clone, Default)]
struct VimRegister {
    text: String,
    linewise: bool,
    /// Rectangular yank: one string per selected line, pasted by column.
    blockwise: bool,
    /// Whether anything was yanked/deleted yet (an empty register can still
    /// hold an empty line, which pastes as a newline).
    set: bool,
}

/// The last change `.` can replay. Insert-mode edits replay only the inserted
/// text (multi-cursor/IME replacements are not reproduced exactly).
#[derive(Debug, Clone, PartialEq, Eq)]
enum RepeatableChange {
    DeleteChar(usize),
    DeleteLines(usize),
    DeleteWord(usize),
    DeleteToLineEnd(usize),
    Paste { before: bool, count: usize },
    Insert(String),
}

/// Last visual selection, restored by `gv`.
#[derive(Debug, Clone, Copy)]
struct VisualSnapshot {
    mode: VimMode,
    anchor: usize,
    cursor: usize,
}

/// Cursor and scroll position captured when edit mode is left, so returning
/// to edit puts the caret back where the user left it.
#[derive(Debug, Clone)]
struct EditorMemory {
    note_id: String,
    line: u32,
    column: u32,
    scroll_y: f32,
}

/// Source-line anchor used to place the preview at the line the caret was on.
/// Markdown cannot be anchored per source line, so the line ratio is an
/// approximation; `screen_fraction` keeps the caret at the same vertical
/// position inside the viewport.
#[derive(Debug, Clone)]
struct PreviewAnchor {
    cursor_line: f32,
    total_lines: f32,
    screen_fraction: f32,
}

/// The root view; owns the framework-agnostic state as an entity sibling.
pub struct NoteBrowser {
    state: Entity<AppState>,
    /// Persistent input entities: `InputState::new` requires `&mut Window`,
    /// which event callbacks do not have, so every field the app ever shows
    /// is created once at startup and cleared with `set_value("")` when the
    /// owning dialog opens (mirrors the egui per-open field lifecycle).
    search_query: Entity<InputState>,
    gate_input: Entity<InputState>,
    rp_current: Entity<InputState>,
    rp_new: Entity<InputState>,
    rp_confirm: Entity<InputState>,
    e2ee_input: Entity<InputState>,
    /// Note editor fields (title single-line, body multi-line).
    title_edit: Entity<InputState>,
    note_edit: Entity<TextareaState>,
    /// ⌘F was pressed from preview mode: the editor element only renders on the
    /// next frame, so the focus + `Search` dispatch is deferred to the tick
    /// (a synchronous dispatch lands before the textarea exists and is dropped).
    pending_find: bool,
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
    /// Focus handles for the two list panels (j/k/Enter navigation).
    notebooks_focus: FocusHandle,
    notes_focus: FocusHandle,
    /// Focus handle for the note body in preview/reading mode.
    body_focus: FocusHandle,
    /// Vim register (linewise or characterwise).
    vim_register: VimRegister,
    /// Visual-mode selection anchor (byte offset).
    vim_anchor: Option<usize>,
    /// Blockwise Visual-mode anchor (byte offset). Kept apart from
    /// `vim_anchor` because a block has no linear native selection.
    vim_block_anchor: Option<usize>,
    /// Desired display column while moving vertically in V-BLOCK. Kept when
    /// the cursor lands on a shorter line so moving back restores it.
    vim_block_col: Option<usize>,
    /// Last visual selection (`gv`).
    vim_last_visual: Option<VisualSnapshot>,
    /// Start of a blockwise `I`/`A`/`c` multi-cursor insert, restored on Esc.
    vim_block_insert_start: Option<usize>,
    /// Enter arrived through the textarea's Input action while a vim command
    /// or search line was open; commit it on the next tick (window access).
    vim_commit_enter: bool,
    /// Last `/` or `?` search: query and direction.
    vim_last_search: Option<(String, bool)>,
    /// Last change replayed by `.`.
    vim_last_change: Option<RepeatableChange>,
    /// Editor text when Insert mode was entered, used to recover the text
    /// typed during the insert for `.`.
    vim_insert_snapshot: Option<String>,
    /// Preview scroll handle (`#note-body-scroll`), used to restore the scroll
    /// position after leaving edit mode.
    preview_scroll: ScrollHandle,
    /// Cursor line anchor the preview should scroll to, set by
    /// [`Self::capture_preview_anchor`] and applied by the tick once the
    /// preview has been laid out.
    pending_preview_anchor: Option<PreviewAnchor>,
    /// Note id the preview scroll belongs to; switching notes resets it.
    preview_note: Option<String>,
    /// Ticks spent waiting for the preview layout before giving up.
    preview_retry: u32,
    /// Caret position captured when edit mode was left; consumed when edit
    /// mode starts again for the same note.
    edit_memory: Option<EditorMemory>,
    /// The preview element mounts one frame after edit mode ends. Focus is
    /// requested immediately and retried once from the tick if the mount race
    /// lost the focus.
    pending_body_focus: bool,
    /// Settings dialog input fields.
    set_url: Entity<InputState>,
    set_user: Entity<InputState>,
    set_pw: Entity<InputState>,
    set_e2ee: Entity<InputState>,
    set_ca: Entity<InputState>,
    focus: FocusHandle,
    #[allow(dead_code)]
    subscriptions: Vec<Subscription>,
}

impl Focusable for NoteBrowser {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// `o` / `O` text edit. Kept as a free function so view tests can drive the
/// exact same TextareaState sequence without building a whole NoteBrowser.
fn insert_vim_line(
    state: &mut TextareaState,
    text: &str,
    cursor: usize,
    above: bool,
    window: &mut Window,
    cx: &mut Context<TextareaState>,
) {
    let (at, after) = vim::open_line_offsets(text, cursor, above);
    state.set_selected_range(at..at, cx);
    state.insert("\n", window, cx);
    let after = after.min(state.text().len());
    state.set_selected_range(after..after, cx);
}

/// Caret target for `I` / `A`. Kept separate from the action match so the
/// key-to-offset wiring is unit-testable without a NoteBrowser.
fn vim_insert_entry_at(text: &str, cursor: usize, action: &VimAction) -> Option<usize> {
    match action {
        VimAction::InsertAtLineStart => Some(vim::first_non_blank_offset(text, cursor)),
        VimAction::InsertAtLineEnd => Some(vim::line_end_offset(text, cursor)),
        _ => None,
    }
}

/// Caret target for `0` / `$`.
fn vim_line_move_at(text: &str, cursor: usize, kind: MoveKind) -> Option<usize> {
    match kind {
        MoveKind::LineStart => Some(vim::line_start_offset(text, cursor)),
        MoveKind::LineEnd => Some(vim::line_last_char_offset(text, cursor)),
        _ => None,
    }
}

impl NoteBrowser {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let state = cx.new(|_| AppState::new().expect("open storage"));
        state.update(cx, |s, _| {
            s.reload_meta();
            // Reopen the last session: last notebook + its most recent note.
            s.restore_last_session();
        });

        let search_query = cx.new(|cx| InputState::new(window, cx).placeholder("搜索全部笔记…"));
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
        let note_edit = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Markdown 正文…")
                // Enables the in-editor find panel opened by ⌘F (FindInNote).
                .searchable(true)
        });
        let nb_input = cx.new(|cx| InputState::new(window, cx).placeholder("笔记本名称"));
        // Settings dialog fields (webdav url / username / passwords / CA cert).
        let set_url =
            cx.new(|cx| InputState::new(window, cx).placeholder("https://host/dav/joplin"));
        let set_user = cx.new(|cx| InputState::new(window, cx).placeholder("用户名"));
        let set_pw = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("留空则不修改")
                .masked(true)
        });
        let set_e2ee = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("留空则不修改")
                .masked(true)
        });
        let set_ca = cx.new(|cx| InputState::new(window, cx).placeholder("PEM 证书路径（可选）"));

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
                    InputEvent::PressEnter { .. } => {
                        let pending = this.state.read(cx).vim_pending.clone();
                        if pending.starts_with(':')
                            || pending.starts_with('/')
                            || pending.starts_with('?')
                        {
                            this.vim_commit_enter = true;
                            cx.notify();
                        }
                        return;
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
        // Settings fields mirror into AppState on change.
        for (field, which) in [
            (set_url.clone(), 0usize),
            (set_user.clone(), 1usize),
            (set_pw.clone(), 2usize),
            (set_e2ee.clone(), 3usize),
            (set_ca.clone(), 4usize),
        ] {
            subscriptions.push(cx.subscribe(
                &field,
                move |this: &mut Self, state, event: &InputEvent, cx| {
                    if !matches!(event, InputEvent::Change) {
                        return;
                    }
                    let text = state.read(cx).value().to_string();
                    this.state.update(cx, |s, _| match which {
                        0 => s.set_webdav_url = text,
                        1 => s.set_username = text,
                        2 => s.set_password = text,
                        3 => s.set_e2ee_password = text,
                        _ => s.set_ca_cert = text,
                    });
                },
            ));
        }

        // Intercept the window close button so pending local changes sync first
        // (mirrors the Quit action path). Returning false cancels the close.
        {
            let browser = cx.entity().downgrade();
            window.on_window_should_close(cx, move |_window, cx| {
                let deferred = browser
                    .update(cx, |this, cx| this.try_defer_quit(cx))
                    .unwrap_or(false);
                !deferred
            });
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
                            // Refresh the pending-local-change count (status
                            // bar + sync-before-quit decision).
                            s.local_pending = s.storage.local_pending_count().unwrap_or(0);
                        });
                        // Auto-start the decrypt pass when a not-yet-tried
                        // password is available (mirrors the egui flow's
                        // auto-continue; honours the sticky cancel flag).
                        this.state.update(cx, |s, _| jobs::auto_decrypt(s));
                        // Debounced autosave (3s after the last change).
                        this.state.update(cx, |s, _| s.autosave_tick());
                        // A command/search Enter arrives as InputEvent, not a
                        // capture-phase key; commit it here where the window
                        // is available.
                        this.flush_vim_enter(window, cx);
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
                            // `set_value` collapses the caret to 0..0; put it
                            // back where the user left edit mode.
                            this.restore_editor_position(window, cx);
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
                        // Deferred in-note find (⌘F from preview mode): the
                        // editor is mounted by now, so the panel can open.
                        // Skip if edit mode was toggled off in the meantime
                        // (⌘F then ⌘E within one tick): focusing the unmounted
                        // editor would leave the window with a dead focus path.
                        if this.pending_find {
                            this.pending_find = false;
                            if this.state.read(cx).editing {
                                let handle = this.note_edit.read(cx).focus_handle(cx);
                                window.focus(&handle, cx);
                                window.dispatch_action(Box::new(Search), cx);
                            }
                            cx.notify();
                        }
                        // Restore the preview scroll position captured when
                        // edit mode was left. The preview only gets its
                        // layout after the next frame, so retry a few ticks.
                        let note_id = this.state.read(cx).selected_note.clone();
                        if note_id != this.preview_note {
                            this.preview_note = note_id;
                            this.pending_preview_anchor = None;
                            this.preview_retry = 0;
                            this.edit_memory = None;
                            this.preview_scroll.set_offset(point(px(0.), px(0.)));
                        }
                        // The preview was just mounted; focus may have been
                        // lost while the editor element unmounted. Retry once
                        // (the editor gets focus back if editing resumed).
                        if this.pending_body_focus {
                            this.pending_body_focus = false;
                            let can_focus_body = !this.state.read(cx).editing
                                && matches!(this.state.read(cx).dialog, AppDialog::None);
                            if can_focus_body && !this.body_focus.is_focused(window) {
                                window.focus(&this.body_focus, cx);
                            }
                        }
                        if let Some(anchor) = this.pending_preview_anchor.clone() {
                            if !this.state.read(cx).editing {
                                let max = this.preview_scroll.max_offset();
                                let viewport = this.preview_scroll.bounds().size.height;
                                if max.y > px(0.) && viewport > px(0.) {
                                    let offset = preview_offset_for_cursor(
                                        anchor.cursor_line,
                                        anchor.total_lines,
                                        anchor.screen_fraction,
                                        f32::from(max.y),
                                        f32::from(viewport),
                                    );
                                    this.preview_scroll.set_offset(point(px(0.), px(offset)));
                                    this.pending_preview_anchor = None;
                                } else if viewport > px(0.) {
                                    // Laid out and the document fits: there is
                                    // nothing to scroll to.
                                    this.pending_preview_anchor = None;
                                } else {
                                    this.preview_retry = this.preview_retry.saturating_add(1);
                                    if this.preview_retry > 20 {
                                        this.pending_preview_anchor = None;
                                    }
                                }
                            }
                        }
                        // Quit-after-sync: leave once the sync job finished.
                        let quit_now = {
                            let s = this.state.read(cx);
                            s.quit_after_sync && !s.jobs.is_busy()
                        };
                        if quit_now {
                            cx.quit();
                            return;
                        }
                        cx.notify();
                    });
                });
            }
        })
        .detach();

        // Sync on launch (checks the remote; transfers only what changed).
        let start_sync = {
            let s = state.read(cx);
            s.config.sync_on_startup && !s.config.webdav_url.is_empty()
        };
        if start_sync {
            state.update(cx, |s, _| jobs::start_download(s));
        }

        let this = Self {
            state,
            search_query,
            gate_input,
            rp_current,
            rp_new,
            rp_confirm,
            e2ee_input,
            title_edit,
            note_edit,
            pending_find: false,
            nb_input,
            notebook_scroll: ScrollHandle::new(),
            note_scroll: ScrollHandle::new(),
            edit_epoch_seen: 0,
            pending_dialog: AppDialog::None,
            rp_dialog_focused: Rc::new(Cell::new(false)),
            e2ee_dialog_focused: Rc::new(Cell::new(false)),
            gate_epoch_seen: 0,
            notebooks_focus: cx.focus_handle(),
            notes_focus: cx.focus_handle(),
            body_focus: cx.focus_handle(),
            vim_register: VimRegister::default(),
            vim_anchor: None,
            vim_block_anchor: None,
            vim_block_col: None,
            vim_last_visual: None,
            vim_block_insert_start: None,
            vim_commit_enter: false,
            vim_last_search: None,
            vim_last_change: None,
            vim_insert_snapshot: None,
            preview_scroll: ScrollHandle::new(),
            pending_preview_anchor: None,
            preview_note: None,
            preview_retry: 0,
            edit_memory: None,
            pending_body_focus: false,
            set_url,
            set_user,
            set_pw,
            set_e2ee,
            set_ca,
            focus: cx.focus_handle(),
            subscriptions,
        };
        // Give the root (key_context "NoteBrowser") focus at startup: with no
        // focused element GPUI dispatches actions only to the window root node,
        // so the root's action listeners never fire and every shortcut is dead
        // until the user clicks a list.
        window.focus(&this.focus, cx);
        this
    }

    fn sync_clicked(&mut self, _: &Sync, _: &mut Window, cx: &mut Context<Self>) {
        self.state.update(cx, |s, _| jobs::start_download(s));
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

    // ---- global shortcuts ------------------------------------------------

    fn quit_action(&mut self, _: &Quit, _: &mut Window, cx: &mut Context<Self>) {
        if !self.try_defer_quit(cx) {
            cx.quit();
        }
    }

    /// Decide whether a quit must wait for a sync. Returns true when the quit
    /// is deferred: a sync was started (or is already running) and the tick
    /// will call `quit` once the job finishes.
    fn try_defer_quit(&mut self, cx: &mut Context<Self>) -> bool {
        let (sync_on_quit, pending, busy) = {
            let s = self.state.read(cx);
            (s.config.sync_on_quit, s.local_pending, s.jobs.is_busy())
        };
        if !sync_on_quit || pending == 0 {
            return false;
        }
        if !busy {
            self.state.update(cx, |s, _| jobs::start_download(s));
        }
        self.state.update(cx, |s, _| s.quit_after_sync = true);
        self.state.update(cx, |s, _| {
            s.set_status(format!("退出前同步 {pending} 项更改…"))
        });
        cx.notify();
        true
    }

    fn toggle_notebooks(
        &mut self,
        _: &ToggleNotebooks,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let showing = self.state.read(cx).show_notebooks;
        self.state.update(cx, |s, _| s.show_notebooks = !showing);
        if showing {
            if self.notebooks_focus.is_focused(window) {
                self.focus_body_focus(window, cx);
            }
        } else {
            self.state.update(cx, |s, _| s.sync_cursors_to_selection());
            window.focus(&self.notebooks_focus, cx);
        }
        cx.notify();
    }

    fn toggle_notes(&mut self, _: &ToggleNotes, window: &mut Window, cx: &mut Context<Self>) {
        let showing = self.state.read(cx).show_notes;
        self.state.update(cx, |s, _| s.show_notes = !showing);
        if showing {
            if self.notes_focus.is_focused(window) {
                self.focus_body_focus(window, cx);
            }
        } else {
            self.state.update(cx, |s, _| s.sync_cursors_to_selection());
            window.focus(&self.notes_focus, cx);
        }
        cx.notify();
    }

    fn focus_notebooks(&mut self, _: &FocusNotebooks, window: &mut Window, cx: &mut Context<Self>) {
        self.state.update(cx, |s, _| {
            s.show_notebooks = true;
            s.sync_cursors_to_selection();
        });
        window.focus(&self.notebooks_focus, cx);
        cx.notify();
    }

    fn focus_notes(&mut self, _: &FocusNotes, window: &mut Window, cx: &mut Context<Self>) {
        self.state.update(cx, |s, _| {
            s.show_notes = true;
            s.sync_cursors_to_selection();
        });
        window.focus(&self.notes_focus, cx);
        cx.notify();
    }

    fn focus_body(&mut self, _: &FocusBody, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_body_focus(window, cx);
        cx.notify();
    }

    /// Focus the body: the editor while editing, otherwise the preview.
    fn focus_body_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.state.read(cx).editing {
            self.focus_note_editor(window, cx);
        } else {
            window.focus(&self.body_focus, cx);
        }
    }

    /// `j`/`k` scroll the preview by half its viewport height. Markdown line
    /// heights vary, so half a viewport is the stable approximation.
    fn body_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "j" => self.scroll_preview(true),
            "k" => self.scroll_preview(false),
            _ => return,
        }
        // The user took over scrolling; do not let the pending edit->preview
        // anchor snap the view back on the next tick.
        self.pending_preview_anchor = None;
        cx.notify();
    }

    fn scroll_preview(&mut self, down: bool) {
        let viewport = self.preview_scroll.bounds().size.height;
        let target = preview_scroll_target(
            f32::from(self.preview_scroll.offset().y),
            f32::from(self.preview_scroll.max_offset().y),
            f32::from(viewport),
            down,
        );
        self.preview_scroll.set_offset(point(px(0.), px(target)));
    }

    /// Toggle edit/preview for the open note (⌘E / ⌃E). `begin_edit` guards on
    /// `can_edit` (protected/encrypted notes stay read-only); `end_edit` saves
    /// any pending change.
    fn toggle_edit(&mut self, _: &ToggleEdit, window: &mut Window, cx: &mut Context<Self>) {
        let was_editing = self.state.read(cx).editing;
        if was_editing {
            self.finish_edit(window, cx);
        } else {
            self.state.update(cx, |s, _| s.begin_edit());
            self.focus_note_editor(window, cx);
        }
        cx.notify();
    }

    /// Put keyboard focus on the note body so Vim motions work immediately
    /// after entering edit mode.
    fn focus_note_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.note_edit.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
    }

    /// Restore keyboard focus after the editor unmounts.
    ///
    /// Leaving edit mode drops the textarea (and the in-editor find panel)
    /// from the element tree while they may hold focus. A focused element that
    /// disappears takes the key-dispatch path with it, so every shortcut goes
    /// dead until the user clicks something (repro: ⌘F then ⌘E). Focus the
    /// preview body instead of the root so `j`/`k` work immediately; the tick
    /// retries once if the preview element was not mounted yet.
    fn refocus_after_editor_closed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pending_body_focus = true;
        self.focus_body_focus(window, cx);
        // The preview element is created on the next render. Repeat the focus
        // request then (the tick still verifies and retries if needed).
        cx.on_next_frame(window, |this, window, cx| {
            let can_focus_body = !this.state.read(cx).editing
                && matches!(this.state.read(cx).dialog, AppDialog::None);
            if can_focus_body {
                window.focus(&this.body_focus, cx);
            }
        });
    }

    /// Open the global search dialog (⌘⌥F): type a query, Enter runs it.
    fn global_search(&mut self, _: &GlobalSearch, window: &mut Window, cx: &mut Context<Self>) {
        self.state.update(cx, |s, _| s.dialog = AppDialog::Search);
        self.sync_dialog_from_state(window, cx);
        cx.notify();
    }

    /// Find within the current note. In edit mode the textarea's own built-in
    /// find panel opens on the “Search” action; from preview we switch to
    /// edit mode first (per the agreed design).
    fn find_in_note(&mut self, _: &FindInNote, window: &mut Window, cx: &mut Context<Self>) {
        if !self.state.read(cx).can_edit() {
            return;
        }
        if !self.state.read(cx).editing {
            self.state.update(cx, |s, _| s.begin_edit());
            let (title, body) = {
                let s = self.state.read(cx);
                (s.edit_title.clone(), s.edit_body.clone())
            };
            self.title_edit
                .update(cx, |i, cx| i.set_value(title, window, cx));
            self.note_edit
                .update(cx, |i, cx| i.set_value(body, window, cx));
            self.edit_epoch_seen = self.state.read(cx).edit_epoch;
            self.restore_editor_position(window, cx);
            // The editor element is only rendered on the next frame; defer the
            // focus + Search dispatch to the tick, otherwise the action is
            // dropped and the find panel never opens on the first ⌘F.
            self.pending_find = true;
            cx.notify();
            return;
        }
        let handle = self.note_edit.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        window.dispatch_action(Box::new(Search), cx);
        cx.notify();
    }

    fn settings_clicked(&mut self, _: &OpenSettings, window: &mut Window, cx: &mut Context<Self>) {
        self.state.update(cx, |s, _| s.open_settings());
        let (url, user, ca) = {
            let s = self.state.read(cx);
            (
                s.set_webdav_url.clone(),
                s.set_username.clone(),
                s.set_ca_cert.clone(),
            )
        };
        self.set_url
            .update(cx, |i, cx| i.set_value(url, window, cx));
        self.set_user
            .update(cx, |i, cx| i.set_value(user, window, cx));
        self.set_ca.update(cx, |i, cx| i.set_value(ca, window, cx));
        self.set_pw.update(cx, |i, cx| i.set_value("", window, cx));
        self.set_e2ee
            .update(cx, |i, cx| i.set_value("", window, cx));
        self.sync_dialog_from_state(window, cx);
        cx.notify();
    }

    /// Submit the settings dialog from its fields. Returns true to close.
    fn submit_settings_dialog(&mut self, cx: &mut Context<Self>) -> bool {
        let read = |e: &Entity<InputState>| e.read(cx).value().to_string();
        let (url, user, pw, e2ee, ca) = (
            read(&self.set_url),
            read(&self.set_user),
            read(&self.set_pw),
            read(&self.set_e2ee),
            read(&self.set_ca),
        );
        self.state.update(cx, |s, _| {
            s.set_webdav_url = url;
            s.set_username = user;
            s.set_password = pw;
            s.set_e2ee_password = e2ee;
            s.set_ca_cert = ca;
            s.submit_settings()
        })
    }

    // ---- list panel keyboard navigation (j/k/Enter) ----------------------

    fn notebooks_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "j" | "k" => {
                let delta = if event.keystroke.key == "j" { 1 } else { -1 };
                self.state.update(cx, |s, _| {
                    s.move_folder_cursor(delta);
                    // Match mouse selection: switch notebook, reload the
                    // note list, and open that notebook's recent note.
                    s.select_folder_at_cursor();
                });
            }
            "enter" => {
                self.state.update(cx, |s, _| s.select_folder_at_cursor());
                window.focus(&self.notes_focus, cx);
            }
            _ => return,
        }
        cx.notify();
    }

    fn notes_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "j" => self.state.update(cx, |s, _| s.move_note_cursor(1)),
            "k" => self.state.update(cx, |s, _| s.move_note_cursor(-1)),
            "enter" => {
                self.state.update(cx, |s, _| s.open_note_at_cursor());
                self.focus_body_focus(window, cx);
            }
            _ => return,
        }
        cx.notify();
    }

    // ---- vim editing layer ----------------------------------------------

    /// Capture-phase key handler over the note body editor. Active only while
    /// editing with `config.vim_mode` on; consumes keys in Normal/Visual mode
    /// so they do not reach the textarea.
    fn editor_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &event.keystroke;
        // Let Cmd/Ctrl application shortcuts through untouched.
        // On non-macOS `platform` *is* Ctrl, which Vim also uses for
        // Ctrl-r/f/b/d/u, so those keys still reach the vim layer.
        let vim_ctrl = ks.modifiers.control
            && !ks.modifiers.alt
            && !ks.modifiers.shift
            && matches!(ks.key.as_str(), "r" | "f" | "b" | "d" | "u" | "v" | "q");
        if ks.modifiers.platform && !vim_ctrl {
            return;
        }
        // The in-editor find panel (⌘F) renders inside this capture container
        // but owns its own input; when it holds focus the vim layer must not
        // swallow its keystrokes.
        if !self.note_edit.read(cx).focus_handle(cx).is_focused(window) {
            return;
        }
        let (enabled, editing, mode) = {
            let s = self.state.read(cx);
            (s.config.vim_mode, s.editing, s.vim_mode)
        };
        if !enabled || !editing {
            return;
        }
        // gpui reports the physical key ("v", ";", ".") plus the typed
        // character ("V", ":", ">"). Vim commands need the latter.
        let key = if !ks.modifiers.control && !ks.modifiers.alt && !ks.modifiers.platform {
            ks.key_char
                .clone()
                .filter(|key_char| {
                    key_char != &ks.key
                        && key_char.chars().count() == 1
                        && ks.key.chars().count() == 1
                })
                .unwrap_or_else(|| ks.key.clone())
        } else {
            ks.key.clone()
        };
        let ctrl = ks.modifiers.control;
        let action = {
            let mut pending = self.state.read(cx).vim_pending.clone();
            let action = vim::translate(mode, &mut pending, &key, ctrl);
            self.state.update(cx, |s, _| s.vim_pending = pending);
            action
        };
        self.sync_vim_enter_mode(cx);
        if action == VimAction::PassThrough {
            return;
        }
        cx.stop_propagation();
        self.apply_vim_action(action, window, cx);
        cx.notify();
    }

    /// The textarea's own Enter binding runs before `capture_key_down`, so a
    /// command line would insert a newline instead of submitting. Switch the
    /// textarea into submit-on-Enter mode while `:`/`/`/`?` is open.
    fn sync_vim_enter_mode(&mut self, cx: &mut Context<Self>) {
        let pending = self.state.read(cx).vim_pending.clone();
        let command =
            pending.starts_with(':') || pending.starts_with('/') || pending.starts_with('?');
        self.note_edit
            .update(cx, |state, cx| state.set_submit_on_enter(command, cx));
    }

    /// Commit a command/search line after the textarea emitted PressEnter.
    fn flush_vim_enter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.vim_commit_enter {
            return;
        }
        self.vim_commit_enter = false;
        let mode = self.state.read(cx).vim_mode;
        let mut pending = self.state.read(cx).vim_pending.clone();
        if !(pending.starts_with(':') || pending.starts_with('/') || pending.starts_with('?')) {
            self.sync_vim_enter_mode(cx);
            return;
        }
        let action = vim::translate(mode, &mut pending, "enter", false);
        self.state.update(cx, |s, _| s.vim_pending = pending);
        self.note_edit
            .update(cx, |state, cx| state.set_submit_on_enter(false, cx));
        self.apply_vim_action(action, window, cx);
        cx.notify();
    }

    fn vim_text(&self, cx: &App) -> String {
        self.note_edit.read(cx).value().to_string()
    }

    fn set_vim_mode(&mut self, mode: VimMode, cx: &mut Context<Self>) {
        let previous = self.state.read(cx).vim_mode;
        if previous.is_visual() && !mode.is_visual() {
            self.remember_visual(previous, cx);
        }
        self.state.update(cx, |s, _| {
            s.vim_mode = mode;
            s.vim_pending.clear();
        });
        if !matches!(mode, VimMode::Visual | VimMode::VisualLine) {
            self.vim_anchor = None;
        }
        if mode != VimMode::VisualBlock {
            self.vim_block_anchor = None;
            self.vim_block_col = None;
        }
        if mode == VimMode::Normal {
            self.vim_block_insert_start = None;
        }
        self.sync_vim_enter_mode(cx);
    }

    /// Remember the active visual selection for `gv`.
    fn remember_visual(&mut self, mode: VimMode, cx: &App) {
        let cursor = self.note_edit.read(cx).cursor();
        let anchor = if mode == VimMode::VisualBlock {
            self.vim_block_anchor.unwrap_or(cursor)
        } else {
            self.vim_anchor.unwrap_or(cursor)
        };
        self.vim_last_visual = Some(VisualSnapshot {
            mode,
            anchor,
            cursor,
        });
    }

    fn visual_anchor(&self, mode: VimMode) -> Option<usize> {
        if mode == VimMode::VisualBlock {
            self.vim_block_anchor
        } else {
            self.vim_anchor
        }
    }

    /// Collapse the native selection to the active cursor. Visual highlights
    /// are painted by the overlay, so the textarea must not own a linear
    /// selection (which cannot represent a reversed endpoint).
    fn refresh_visual(&mut self, cx: &mut Context<Self>) {
        if !self.state.read(cx).vim_mode.is_visual() {
            return;
        }
        let cursor = self.note_edit.read(cx).cursor();
        self.note_edit
            .update(cx, |s, cx| s.set_selected_range(cursor..cursor, cx));
    }

    /// Record the last change so `.` can replay it.
    fn record_change(&mut self, change: RepeatableChange) {
        self.vim_last_change = Some(change);
    }

    /// Run a Vim motion `count` times through the textarea's own actions.
    fn vim_move(
        &mut self,
        kind: MoveKind,
        count: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = count.max(1);
        let half_page = |this: &Self, cx: &App| -> usize {
            this.note_edit
                .read(cx)
                .visible_row_range()
                .map(|r| (r.len() / 2).max(1))
                .unwrap_or(10)
        };
        match kind {
            MoveKind::Left | MoveKind::Right | MoveKind::Up | MoveKind::Down => {
                for _ in 0..count {
                    match kind {
                        MoveKind::Left => {
                            window.dispatch_action(Box::new(MoveLeft), cx);
                        }
                        MoveKind::Right => {
                            window.dispatch_action(Box::new(MoveRight), cx);
                        }
                        MoveKind::Up => {
                            window.dispatch_action(Box::new(MoveUp), cx);
                        }
                        _ => {
                            window.dispatch_action(Box::new(MoveDown), cx);
                        }
                    }
                }
            }
            MoveKind::WordForward | MoveKind::WordBackward => {
                let text = self.vim_text(cx);
                let cursor = self.note_edit.read(cx).cursor();
                let to = if kind == MoveKind::WordForward {
                    vim::word_forward_offset(&text, cursor, count)
                } else {
                    vim::word_backward_offset(&text, cursor, count)
                };
                self.note_edit
                    .update(cx, |s, cx| s.set_selected_range(to..to, cx));
            }
            MoveKind::LineStart | MoveKind::LineEnd => {
                let text = self.vim_text(cx);
                let cursor = self.note_edit.read(cx).cursor();
                if let Some(at) = vim_line_move_at(&text, cursor, kind) {
                    self.note_edit
                        .update(cx, |s, cx| s.set_selected_range(at..at, cx));
                }
            }
            MoveKind::DocStart => {
                window.dispatch_action(Box::new(MoveToStart), cx);
            }
            MoveKind::DocEnd => {
                window.dispatch_action(Box::new(MoveToEnd), cx);
            }
            MoveKind::PageDown => {
                for _ in 0..count {
                    window.dispatch_action(Box::new(MovePageDown), cx);
                }
            }
            MoveKind::PageUp => {
                for _ in 0..count {
                    window.dispatch_action(Box::new(MovePageUp), cx);
                }
            }
            MoveKind::HalfPageDown => {
                let lines = half_page(self, cx);
                for _ in 0..lines {
                    window.dispatch_action(Box::new(MoveDown), cx);
                }
            }
            MoveKind::HalfPageUp => {
                let lines = half_page(self, cx);
                for _ in 0..lines {
                    window.dispatch_action(Box::new(MoveUp), cx);
                }
            }
        }
    }

    /// Place the caret at an absolute offset and enter Insert, snapshotting
    /// the text so `.` can record what was typed. Used by `I` and `A`, which
    /// must not depend on gpui line-movement actions (the textarea does not
    /// bind `MoveToStartOfLine` / `MoveToEndOfLine`).
    fn vim_insert_at(&mut self, at: usize, cx: &mut Context<Self>) {
        self.note_edit
            .update(cx, |s, cx| s.set_selected_range(at..at, cx));
        self.vim_insert_snapshot = Some(self.vim_text(cx));
        self.set_vim_mode(VimMode::Insert, cx);
    }

    /// `o` / `O`: insert a newline at the line boundary, move the caret onto
    /// the new empty line, then enter Insert.
    fn vim_open_line(&mut self, above: bool, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.vim_text(cx);
        let cursor = self.note_edit.read(cx).cursor();
        self.note_edit.update(cx, |s, cx| {
            insert_vim_line(s, &text, cursor, above, window, cx)
        });
        self.vim_insert_snapshot = Some(self.vim_text(cx));
        self.set_vim_mode(VimMode::Insert, cx);
    }

    /// Delete `range`, storing it in the register and recording the change.
    /// Dispatching `Delete` keeps undo and the change/autosave path intact.
    fn vim_delete_range(
        &mut self,
        range: std::ops::Range<usize>,
        linewise: bool,
        change: RepeatableChange,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if range.start >= range.end {
            return;
        }
        let text = self.vim_text(cx);
        let Some(slice) = text.get(range.clone()) else {
            return;
        };
        self.vim_register = VimRegister {
            text: if linewise {
                slice.trim_matches('\n').to_string()
            } else {
                slice.to_string()
            },
            linewise,
            blockwise: false,
            set: true,
        };
        self.record_change(change);
        self.note_edit
            .update(cx, |s, cx| s.set_selected_range(range, cx));
        window.dispatch_action(Box::new(Delete), cx);
    }

    /// Paste the register `count` times. Linewise text goes on its own line
    /// (`p` below, `P` above the current one); characterwise text is inserted
    /// after or before the cursor.
    fn vim_paste(
        &mut self,
        before: bool,
        count: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.vim_register.set {
            return;
        }
        let reg = self.vim_register.clone();
        let text = self.vim_text(cx);
        let cursor = self.note_edit.read(cx).cursor();
        if reg.blockwise {
            let lines: Vec<String> = reg.text.split('\n').map(str::to_string).collect();
            let (new_text, new_cursor) = vim::block_insert(&text, cursor, &lines, count);
            self.replace_document(new_text, new_cursor, window, cx);
            return;
        }
        if reg.linewise {
            let repeat = count.max(1);
            let block = std::iter::repeat(reg.text.as_str())
                .take(repeat)
                .collect::<Vec<_>>()
                .join("\n");
            let (insert_at, payload) = if before {
                (vim::line_start_offset(&text, cursor), format!("{block}\n"))
            } else {
                (vim::line_end_offset(&text, cursor), format!("\n{block}"))
            };
            self.note_edit.update(cx, |s, cx| {
                s.set_selected_range(insert_at..insert_at, cx);
                s.insert(payload, window, cx);
            });
        } else {
            let payload = reg.text.repeat(count.max(1));
            let at = if before {
                cursor
            } else {
                let mut at = cursor;
                if let Some(c) = text[cursor..].chars().next() {
                    at += c.len_utf8();
                }
                at
            };
            self.note_edit.update(cx, |s, cx| {
                s.set_selected_range(at..at, cx);
                s.insert(payload, window, cx);
            });
        }
        self.record_change(RepeatableChange::Paste { before, count });
    }

    /// Replace the whole document in one edit. Used by block operations so
    /// undo, Change events, dirty tracking and autosave stay on the normal
    /// path while cursor/scroll are restored afterwards.
    fn replace_document(
        &mut self,
        new_text: String,
        cursor: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.note_edit.update(cx, |s, cx| {
            let len = s.text().len();
            s.set_selected_range(0..len, cx);
            s.replace(new_text, window, cx);
            let cursor = cursor.min(s.text().len());
            s.set_selected_range(cursor..cursor, cx);
        });
    }

    /// Active visual mode, anchor, cursor and a snapshot of the text.
    fn visual_parts(&self, cx: &App) -> Option<(VimMode, usize, usize, String)> {
        let mode = self.state.read(cx).vim_mode;
        if !mode.is_visual() {
            return None;
        }
        let anchor = self.visual_anchor(mode)?;
        let cursor = self.note_edit.read(cx).cursor();
        Some((mode, anchor, cursor, self.vim_text(cx)))
    }

    fn visual_line_span(&self, text: &str, anchor: usize, cursor: usize) -> (usize, usize) {
        let (a, _) = vim::offset_to_line_display_col(text, anchor);
        let (c, _) = vim::offset_to_line_display_col(text, cursor);
        (a.min(c), a.max(c))
    }

    fn enter_insert_mode(&mut self, cx: &mut Context<Self>) {
        self.vim_insert_snapshot = Some(self.vim_text(cx));
        self.set_vim_mode(VimMode::Insert, cx);
    }

    fn delete_visual_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((mode, anchor, cursor, text)) = self.visual_parts(cx) else {
            return;
        };
        match mode {
            VimMode::VisualBlock => {
                self.vim_register = VimRegister {
                    text: vim::block_text(&text, anchor, cursor),
                    linewise: false,
                    blockwise: true,
                    set: true,
                };
                let (new_text, new_cursor) = vim::block_delete(&text, anchor, cursor);
                self.replace_document(new_text, new_cursor, window, cx);
            }
            VimMode::VisualLine => {
                let range = vim::linewise_range(&text, anchor, cursor);
                let (first, last) = self.visual_line_span(&text, anchor, cursor);
                self.vim_delete_range(
                    range,
                    true,
                    RepeatableChange::DeleteLines(last - first + 1),
                    window,
                    cx,
                );
            }
            _ => {
                let range = vim::charwise_range(&text, anchor, cursor);
                let chars = text[range.clone()].chars().count().max(1);
                self.vim_delete_range(
                    range,
                    false,
                    RepeatableChange::DeleteChar(chars),
                    window,
                    cx,
                );
            }
        }
        self.set_vim_mode(VimMode::Normal, cx);
    }

    fn yank_visual_selection(&mut self, cx: &mut Context<Self>) {
        let Some((mode, anchor, cursor, text)) = self.visual_parts(cx) else {
            return;
        };
        let (range, linewise, blockwise, cursor_at) = match mode {
            VimMode::VisualBlock => {
                let selection = vim::block_selection(&text, anchor, cursor);
                (
                    vim::block_ranges(&text, anchor, cursor),
                    false,
                    true,
                    vim::offset_for_display_col(&text, selection.top_line, selection.left_col),
                )
            }
            VimMode::VisualLine => {
                let range = vim::linewise_range(&text, anchor, cursor);
                let cursor_at = range.start;
                (vec![range], true, false, cursor_at)
            }
            _ => {
                let range = vim::charwise_range(&text, anchor, cursor);
                let cursor_at = range.start;
                (vec![range], false, false, cursor_at)
            }
        };
        let mut text_out = String::new();
        for r in &range {
            if let Some(slice) = text.get(r.clone()) {
                if !text_out.is_empty() {
                    text_out.push('\n');
                }
                text_out.push_str(slice);
            }
        }
        if linewise {
            text_out = text_out.trim_end_matches(['\n', '\r']).to_string();
        }
        self.vim_register = VimRegister {
            text: text_out,
            linewise,
            blockwise,
            set: true,
        };
        self.note_edit
            .update(cx, |s, cx| s.set_selected_range(cursor_at..cursor_at, cx));
        self.set_vim_mode(VimMode::Normal, cx);
    }

    fn change_visual_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((mode, anchor, cursor, text)) = self.visual_parts(cx) else {
            return;
        };
        if mode == VimMode::VisualBlock {
            self.block_change(window, cx);
            return;
        }
        let (range, linewise) = if mode == VimMode::VisualLine {
            (vim::linewise_range(&text, anchor, cursor), true)
        } else {
            (vim::charwise_range(&text, anchor, cursor), false)
        };
        let start = range.start;
        if linewise {
            let (first, last) = self.visual_line_span(&text, anchor, cursor);
            self.vim_delete_range(
                range,
                true,
                RepeatableChange::DeleteLines(last - first + 1),
                window,
                cx,
            );
        } else {
            let chars = text[range.clone()].chars().count().max(1);
            self.vim_delete_range(
                range,
                false,
                RepeatableChange::DeleteChar(chars),
                window,
                cx,
            );
        }
        self.note_edit
            .update(cx, |s, cx| s.set_selected_range(start..start, cx));
        self.enter_insert_mode(cx);
    }

    fn indent_visual_selection(
        &mut self,
        increase: bool,
        count: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((_mode, anchor, cursor, text)) = self.visual_parts(cx) else {
            return;
        };
        let (first, last) = self.visual_line_span(&text, anchor, cursor);
        let new_text = vim::indent_lines(&text, first, last, increase, count);
        let cursor = vim::line_starts(&text).get(first).copied().unwrap_or(0);
        self.replace_document(new_text, cursor, window, cx);
        self.set_vim_mode(VimMode::Normal, cx);
    }

    fn toggle_case_visual_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((mode, anchor, cursor, text)) = self.visual_parts(cx) else {
            return;
        };
        let ranges = vim::visual_ranges(mode, &text, anchor, cursor);
        let cursor = ranges.first().map(|range| range.start).unwrap_or(anchor);
        let new_text = vim::toggle_case_ranges(&text, &ranges);
        self.replace_document(new_text, cursor, window, cx);
        self.set_vim_mode(VimMode::Normal, cx);
    }

    fn toggle_visual_end(&mut self, cx: &mut Context<Self>) {
        let mode = self.state.read(cx).vim_mode;
        if !mode.is_visual() {
            return;
        }
        let cursor = self.note_edit.read(cx).cursor();
        let Some(anchor) = self.visual_anchor(mode) else {
            return;
        };
        if mode == VimMode::VisualBlock {
            self.vim_block_anchor = Some(cursor);
            let text = self.vim_text(cx);
            self.vim_block_col = Some(vim::offset_to_line_display_col(&text, anchor).1);
        } else {
            self.vim_anchor = Some(cursor);
        }
        self.note_edit
            .update(cx, |s, cx| s.set_selected_range(anchor..anchor, cx));
        self.refresh_visual(cx);
    }

    fn toggle_block_corner(&mut self, cx: &mut Context<Self>) {
        let Some((mode, anchor, cursor, text)) = self.visual_parts(cx) else {
            return;
        };
        if mode != VimMode::VisualBlock {
            return;
        }
        let selection = vim::block_selection(&text, anchor, cursor);
        let (line, _) = vim::offset_to_line_display_col(&text, cursor);
        let (_, current_col) = vim::offset_to_line_display_col(&text, cursor);
        let target_col = if current_col <= selection.left_col {
            selection.right_col
        } else {
            selection.left_col
        };
        let to = vim::offset_for_display_col(&text, line, target_col);
        self.vim_block_col = Some(target_col);
        self.note_edit
            .update(cx, |s, cx| s.set_selected_range(to..to, cx));
        self.refresh_visual(cx);
    }

    fn select_text_object(
        &mut self,
        kind: vim::TextObjectKind,
        around: bool,
        cx: &mut Context<Self>,
    ) {
        let text = self.vim_text(cx);
        let cursor = self.note_edit.read(cx).cursor();
        let range = match kind {
            vim::TextObjectKind::Word => vim::word_object_range(&text, cursor, around),
            vim::TextObjectKind::Paragraph => vim::paragraph_object_range(&text, cursor, around),
        };
        if range.start >= range.end {
            return;
        }
        let last = text[..range.end]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(range.start)
            .max(range.start);
        self.vim_anchor = Some(range.start);
        self.note_edit
            .update(cx, |s, cx| s.set_selected_range(last..last, cx));
        self.set_vim_mode(VimMode::Visual, cx);
        self.refresh_visual(cx);
    }

    fn reselect_visual(&mut self, cx: &mut Context<Self>) {
        let Some(snapshot) = self.vim_last_visual else {
            return;
        };
        let text = self.vim_text(cx);
        match snapshot.mode {
            VimMode::VisualBlock => {
                self.vim_block_anchor = Some(snapshot.anchor);
                self.vim_block_col =
                    Some(vim::offset_to_line_display_col(&text, snapshot.cursor).1);
            }
            VimMode::Visual | VimMode::VisualLine => {
                self.vim_anchor = Some(snapshot.anchor);
            }
            _ => return,
        }
        self.note_edit.update(cx, |s, cx| {
            s.set_selected_range(snapshot.cursor..snapshot.cursor, cx)
        });
        self.set_vim_mode(snapshot.mode, cx);
        self.refresh_visual(cx);
    }

    /// Set one empty selection per offset (first active) and enter Insert.
    fn begin_block_multicursor(&mut self, offsets: &[usize], cx: &mut Context<Self>) {
        let Some(&first) = offsets.first() else {
            return;
        };
        self.note_edit
            .update(cx, |s, cx| s.set_selected_range(first..first, cx));
        for &offset in &offsets[1..] {
            self.note_edit
                .update(cx, |s, cx| s.add_selection(offset..offset, cx));
        }
        self.vim_block_insert_start = Some(first);
        self.enter_insert_mode(cx);
    }

    fn block_insert(&mut self, append: bool, cx: &mut Context<Self>) {
        let Some((mode, anchor, cursor, text)) = self.visual_parts(cx) else {
            return;
        };
        if mode != VimMode::VisualBlock {
            return;
        }
        let offsets = vim::block_insert_offsets(&text, anchor, cursor, append);
        self.begin_block_multicursor(&offsets, cx);
    }

    fn block_change(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((mode, anchor, cursor, text)) = self.visual_parts(cx) else {
            return;
        };
        if mode != VimMode::VisualBlock {
            return;
        }
        self.vim_register = VimRegister {
            text: vim::block_text(&text, anchor, cursor),
            linewise: false,
            blockwise: true,
            set: true,
        };
        let (new_text, new_cursor, offsets) = vim::block_change(&text, anchor, cursor);
        self.replace_document(new_text, new_cursor, window, cx);
        self.begin_block_multicursor(&offsets, cx);
    }

    fn sort_visual_selection(
        &mut self,
        descending: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((_, anchor, cursor, text)) = self.visual_parts(cx) else {
            return;
        };
        let (first, last) = self.visual_line_span(&text, anchor, cursor);
        let new_text = vim::sort_line_range(&text, first, last, descending);
        let cursor = vim::line_starts(&text).get(first).copied().unwrap_or(0);
        self.replace_document(new_text, cursor, window, cx);
        self.set_vim_mode(VimMode::Normal, cx);
    }

    /// Search for `query` from `from` and move the cursor to the match.
    fn vim_search(&mut self, query: &str, backward: bool, from: usize, cx: &mut Context<Self>) {
        let text = self.vim_text(cx);
        let Some(pos) = vim::find_match(&text, from, query, backward) else {
            self.state
                .update(cx, |s, _| s.set_status(format!("未找到：{query}")));
            return;
        };
        self.note_edit
            .update(cx, |s, cx| s.set_selected_range(pos..pos, cx));
        self.state.update(cx, |s, _| {
            s.set_status(format!("搜索：{query}"));
        });
    }

    /// Apply a substitute spec. When no range is given the current line is
    /// used. Matches are replaced back-to-front so earlier offsets stay valid;
    /// each `replace` goes through the textarea so Change/undo still fire.
    fn vim_substitute(
        &mut self,
        mut spec: vim::SubSpec,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !spec.whole && spec.start.is_none() {
            let line = self.note_edit.read(cx).cursor_position().line as usize + 1;
            spec.start = Some(line);
            spec.end = Some(line);
        }
        let text = self.vim_text(cx);
        let mut hits = vim::substitute_matches(&text, &spec);
        hits.reverse();
        let count = hits.len();
        for (range, replacement) in hits {
            self.note_edit.update(cx, |s, cx| {
                s.set_selected_range(range, cx);
                s.replace(replacement, window, cx);
            });
        }
        self.state.update(cx, |s, _| {
            s.set_status(format!("替换 {count} 处"));
        });
    }

    fn apply_vim_action(&mut self, action: VimAction, window: &mut Window, cx: &mut Context<Self>) {
        match action {
            VimAction::PassThrough | VimAction::Swallow | VimAction::Pending => {}
            VimAction::ToInsert => {
                self.vim_insert_snapshot = Some(self.vim_text(cx));
                self.vim_anchor = None;
                self.set_vim_mode(VimMode::Insert, cx);
            }
            VimAction::ToNormal => {
                let block_start = self.vim_block_insert_start.take();
                let snapshot = self.vim_insert_snapshot.take();
                if block_start.is_none() {
                    if let Some(old) = snapshot {
                        let new = self.vim_text(cx);
                        if new != old {
                            let prefix = common_prefix(&old, &new);
                            let suffix = common_suffix(&old[prefix..], &new[prefix..]);
                            let inserted = &new[prefix..new.len().saturating_sub(suffix)];
                            if !inserted.is_empty() {
                                self.record_change(RepeatableChange::Insert(inserted.to_string()));
                            }
                        }
                    }
                }
                self.vim_anchor = None;
                self.set_vim_mode(VimMode::Normal, cx);
                if let Some(start) = block_start {
                    let start = start.min(self.vim_text(cx).len());
                    self.note_edit
                        .update(cx, |s, cx| s.set_selected_range(start..start, cx));
                }
            }
            VimAction::ToVisual => {
                let previous = self.state.read(cx).vim_mode;
                let cursor = self.note_edit.read(cx).cursor();
                let anchor = if previous.is_visual() {
                    self.visual_anchor(previous).unwrap_or(cursor)
                } else {
                    cursor
                };
                self.vim_anchor = Some(anchor);
                self.set_vim_mode(VimMode::Visual, cx);
                self.refresh_visual(cx);
            }
            VimAction::ToVisualLine => {
                let previous = self.state.read(cx).vim_mode;
                let cursor = self.note_edit.read(cx).cursor();
                let anchor = if previous.is_visual() {
                    self.visual_anchor(previous).unwrap_or(cursor)
                } else {
                    cursor
                };
                self.vim_anchor = Some(anchor);
                self.set_vim_mode(VimMode::VisualLine, cx);
                self.refresh_visual(cx);
            }
            VimAction::ToVisualBlock => {
                let previous = self.state.read(cx).vim_mode;
                let cursor = self.note_edit.read(cx).cursor();
                let text = self.vim_text(cx);
                let anchor = if previous.is_visual() {
                    self.visual_anchor(previous).unwrap_or(cursor)
                } else {
                    cursor
                };
                self.vim_block_anchor = Some(anchor);
                self.vim_block_col = Some(vim::offset_to_line_display_col(&text, cursor).1);
                self.set_vim_mode(VimMode::VisualBlock, cx);
                self.refresh_visual(cx);
            }
            VimAction::VisualExit => {
                let cursor = self.note_edit.read(cx).cursor();
                self.note_edit
                    .update(cx, |s, cx| s.set_selected_range(cursor..cursor, cx));
                self.set_vim_mode(VimMode::Normal, cx);
            }
            VimAction::Move { kind, count } => {
                if self.state.read(cx).vim_mode == VimMode::VisualBlock {
                    let cursor = self.note_edit.read(cx).cursor();
                    let text = self.vim_text(cx);
                    if matches!(
                        kind,
                        MoveKind::Left | MoveKind::Right | MoveKind::Up | MoveKind::Down
                    ) {
                        let to = match kind {
                            MoveKind::Up | MoveKind::Down => {
                                let (line, col) = vim::offset_to_line_display_col(&text, cursor);
                                let preferred = self.vim_block_col.unwrap_or(col);
                                let lines = vim::line_starts(&text).len();
                                let target = if kind == MoveKind::Up {
                                    line.saturating_sub(count.max(1))
                                } else {
                                    (line + count.max(1)).min(lines.saturating_sub(1))
                                };
                                vim::offset_for_display_col(&text, target, preferred)
                            }
                            _ => vim::block_move(&text, cursor, kind, count),
                        };
                        if matches!(kind, MoveKind::Left | MoveKind::Right) {
                            self.vim_block_col = Some(vim::offset_to_line_display_col(&text, to).1);
                        }
                        self.note_edit
                            .update(cx, |s, cx| s.set_selected_range(to..to, cx));
                    } else {
                        self.vim_move(kind, count, window, cx);
                        let at = self.note_edit.read(cx).cursor();
                        self.vim_block_col = Some(vim::offset_to_line_display_col(&text, at).1);
                    }
                } else {
                    self.vim_move(kind, count, window, cx);
                    self.refresh_visual(cx);
                }
            }
            VimAction::GotoLine(line) => {
                let line = line.max(1) as u32 - 1;
                let pos = Position::new(line, 0);
                self.note_edit
                    .update(cx, |s, cx| s.set_cursor_position(pos, window, cx));
                self.refresh_visual(cx);
            }
            VimAction::DeleteChar(count) => {
                let cursor = self.note_edit.read(cx).cursor();
                let text = self.vim_text(cx);
                let end = nth_char_boundary(&text, cursor, count.max(1));
                self.vim_delete_range(
                    cursor..end,
                    false,
                    RepeatableChange::DeleteChar(count.max(1)),
                    window,
                    cx,
                );
            }
            VimAction::DeleteLines(count) => {
                let cursor = self.note_edit.read(cx).cursor();
                let text = self.vim_text(cx);
                let (start, end) = vim::delete_line_range(&text, cursor, count);
                self.vim_delete_range(
                    start..end,
                    true,
                    RepeatableChange::DeleteLines(count),
                    window,
                    cx,
                );
            }
            VimAction::YankLines(count) => {
                let cursor = self.note_edit.read(cx).cursor();
                let text = self.vim_text(cx);
                self.vim_register = VimRegister {
                    text: vim::yank_line_text(&text, cursor, count),
                    linewise: true,
                    blockwise: false,
                    set: true,
                };
            }
            VimAction::DeleteWord(count) => {
                let cursor = self.note_edit.read(cx).cursor();
                let text = self.vim_text(cx);
                let end = vim::word_forward_offset(&text, cursor, count);
                self.vim_delete_range(
                    cursor..end,
                    false,
                    RepeatableChange::DeleteWord(count),
                    window,
                    cx,
                );
            }
            VimAction::DeleteToLineEnd(_) => {
                let cursor = self.note_edit.read(cx).cursor();
                let text = self.vim_text(cx);
                let end = vim::line_end_offset(&text, cursor);
                self.vim_delete_range(
                    cursor..end,
                    false,
                    RepeatableChange::DeleteToLineEnd(1),
                    window,
                    cx,
                );
            }
            VimAction::DeleteSelection => {
                self.delete_visual_selection(window, cx);
            }
            VimAction::YankSelection => {
                self.yank_visual_selection(cx);
            }
            VimAction::ChangeSelection => {
                self.change_visual_selection(window, cx);
            }
            VimAction::IndentSelection { increase, count } => {
                self.indent_visual_selection(increase, count, window, cx);
            }
            VimAction::ToggleCaseSelection => {
                self.toggle_case_visual_selection(window, cx);
            }
            VimAction::ToggleVisualEnd => self.toggle_visual_end(cx),
            VimAction::ToggleBlockCorner => self.toggle_block_corner(cx),
            VimAction::SelectTextObject { kind, around } => {
                self.select_text_object(kind, around, cx);
            }
            VimAction::ReselectVisual => self.reselect_visual(cx),
            VimAction::BlockInsert { append } => self.block_insert(append, cx),
            VimAction::BlockChange => self.block_change(window, cx),
            VimAction::SortSelection { descending } => {
                self.sort_visual_selection(descending, window, cx);
            }
            VimAction::Paste { before, count } => self.vim_paste(before, count, window, cx),
            VimAction::Undo => window.dispatch_action(Box::new(Undo), cx),
            VimAction::Redo => window.dispatch_action(Box::new(Redo), cx),
            VimAction::InsertAfterCursor => {
                window.dispatch_action(Box::new(MoveRight), cx);
                self.vim_insert_snapshot = Some(self.vim_text(cx));
                self.set_vim_mode(VimMode::Insert, cx);
            }
            action @ (VimAction::InsertAtLineStart | VimAction::InsertAtLineEnd) => {
                let text = self.vim_text(cx);
                let cursor = self.note_edit.read(cx).cursor();
                if let Some(at) = vim_insert_entry_at(&text, cursor, &action) {
                    self.vim_insert_at(at, cx);
                }
            }
            VimAction::OpenLineBelow => self.vim_open_line(false, window, cx),
            VimAction::OpenLineAbove => self.vim_open_line(true, window, cx),
            VimAction::Save => {
                self.state.update(cx, |s, _| {
                    s.save_edit();
                });
            }
            VimAction::FinishEdit => self.finish_edit(window, cx),
            VimAction::Search { query, backward } => {
                let cursor = self.note_edit.read(cx).cursor();
                self.vim_search(&query, backward, cursor, cx);
                self.vim_last_search = Some((query, backward));
                self.refresh_visual(cx);
            }
            VimAction::SearchNext { backward } => {
                if let Some((query, previous_backward)) = self.vim_last_search.clone() {
                    let direction = if backward {
                        !previous_backward
                    } else {
                        previous_backward
                    };
                    let cursor = self.note_edit.read(cx).cursor();
                    let from = if direction {
                        cursor.saturating_sub(1)
                    } else {
                        cursor.saturating_add(1)
                    };
                    self.vim_search(&query, direction, from, cx);
                    self.refresh_visual(cx);
                }
            }
            VimAction::Substitute(mut spec) => {
                let selection_range = spec.selection_range;
                if selection_range {
                    if let Some((_, anchor, cursor, text)) = self.visual_parts(cx) {
                        let (first, last) = self.visual_line_span(&text, anchor, cursor);
                        spec.start = Some(first + 1);
                        spec.end = Some(last + 1);
                    }
                }
                self.vim_substitute(spec, window, cx);
                if selection_range {
                    self.set_vim_mode(VimMode::Normal, cx);
                }
            }
            VimAction::Repeat => {
                if let Some(change) = self.vim_last_change.clone() {
                    match change {
                        RepeatableChange::DeleteChar(count) => {
                            self.apply_vim_action(VimAction::DeleteChar(count), window, cx)
                        }
                        RepeatableChange::DeleteLines(count) => {
                            self.apply_vim_action(VimAction::DeleteLines(count), window, cx)
                        }
                        RepeatableChange::DeleteWord(count) => {
                            self.apply_vim_action(VimAction::DeleteWord(count), window, cx)
                        }
                        RepeatableChange::DeleteToLineEnd(count) => {
                            self.apply_vim_action(VimAction::DeleteToLineEnd(count), window, cx)
                        }
                        RepeatableChange::Paste { before, count } => {
                            self.apply_vim_action(VimAction::Paste { before, count }, window, cx)
                        }
                        RepeatableChange::Insert(text) => {
                            let insert_at = self.note_edit.read(cx).cursor();
                            self.note_edit.update(cx, |s, cx| {
                                s.set_selected_range(insert_at..insert_at, cx);
                                s.insert(text, window, cx);
                            });
                        }
                    }
                }
            }
            VimAction::Error(message) => {
                self.state
                    .update(cx, |s, _| s.set_status(format!("vim: {message}")));
            }
        }
    }

    /// Capture where the editor was scrolled, then leave edit mode. Called by
    /// the toolbar button, ⌘E, `:q`, `:wq` and `ZZ`.
    fn finish_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.state.read(cx).editing {
            return;
        }
        self.capture_editor_memory(cx);
        self.capture_preview_anchor(cx);
        self.state.update(cx, |s, _| s.end_edit());
        self.vim_anchor = None;
        self.vim_insert_snapshot = None;
        self.note_edit
            .update(cx, |state, cx| state.set_submit_on_enter(false, cx));
        self.refocus_after_editor_closed(window, cx);
    }

    /// Record the caret and editor scroll offset so the next edit session for
    /// this note can continue where this one stopped.
    fn capture_editor_memory(&mut self, cx: &mut Context<Self>) {
        let Some(note_id) = self.state.read(cx).selected_note.clone() else {
            return;
        };
        let st = self.note_edit.read(cx);
        let position = st.cursor_position();
        self.edit_memory = Some(EditorMemory {
            note_id,
            line: position.line,
            column: position.character,
            scroll_y: f32::from(st.scroll_offset().y),
        });
    }

    /// Put the caret (and the editor viewport) back to the captured position.
    /// Called after the editor fields are seeded from `edit_body`, because
    /// `set_value` resets the selection to `0..0` and scrolls to the start.
    fn restore_editor_position(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.state.read(cx).editing {
            return;
        }
        let Some(memory) = self.edit_memory.as_ref() else {
            return;
        };
        if !editor_memory_matches(memory, self.state.read(cx).selected_note.as_deref()) {
            return;
        }
        let memory = self.edit_memory.take().expect("checked above");
        self.note_edit.update(cx, |state, cx| {
            let position = Position::new(memory.line, memory.column);
            state.set_cursor_position(position, window, cx);
            state.set_scroll_offset(point(px(0.), px(memory.scroll_y)), cx);
        });
        self.focus_note_editor(window, cx);
    }

    /// Record the caret's source line and its relative screen position so the
    /// tick can scroll the preview near the same place once it is laid out.
    fn capture_preview_anchor(&mut self, cx: &mut Context<Self>) {
        let Some(note_id) = self.state.read(cx).selected_note.clone() else {
            return;
        };
        let st = self.note_edit.read(cx);
        let Some((caret, line_height)) = st.cursor_layout() else {
            return;
        };
        let input = st.input_bounds();
        let line_height = f32::from(line_height);
        let viewport = f32::from(input.size.height);
        if line_height <= 0.0 || viewport <= 0.0 {
            return;
        }
        let total_lines = st.text().to_string().lines().count().max(1);
        let cursor_line = st.cursor_position().line as f32;
        // `cursor_layout()` is in content space and excludes the vertical
        // scroll offset that paint applies to the native caret.
        let scroll_y = st.scroll_offset().y;
        let caret_top = f32::from(caret.top() + scroll_y - input.top());
        let screen_fraction = (caret_top / viewport).clamp(0.0, 1.0);
        self.pending_preview_anchor = Some(PreviewAnchor {
            cursor_line,
            total_lines: total_lines as f32,
            screen_fraction,
        });
        self.preview_note = Some(note_id);
        self.preview_retry = 0;
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
            AppDialog::Search => self.open_search_dialog(window, cx),
            AppDialog::NewNotebook | AppDialog::RenameNotebook => {
                self.open_notebook_dialog(window, cx, want)
            }
            AppDialog::ConfirmDeleteNote | AppDialog::ConfirmDeleteNotebook => {
                self.open_confirm_delete_dialog(window, cx, want)
            }
            AppDialog::Settings => self.open_settings_dialog(window, cx),
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

    /// Modal for a global search query (⌘⌥F). Enter applies the query and
    /// closes; Esc/Cancel leaves the current filter untouched.
    fn open_search_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let browser = cx.entity().downgrade();
        let input = self.search_query.clone();
        // Prefill with the active query so it can be edited or cleared.
        let initial = self.state.read(cx).search.clone();
        input.update(cx, |i, cx| i.set_value(initial, window, cx));
        window.open_dialog(cx, move |dialog, window, cx| {
            let handle = input.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
            let browser_ok = browser.clone();
            let browser_cancel = browser.clone();
            let browser_close = browser.clone();
            let input_field = input.clone();
            let input_ok = input.clone();
            dialog
                .title("搜索笔记")
                .w(px(420.))
                .content(move |content, _window, _cx| {
                    content
                        .child(form_row("查询", input_field.clone()))
                        .child(hint_line(
                            "输入关键词后回车搜索全部笔记；留空回车则清除筛选。",
                        ))
                })
                .footer(
                    DialogFooter::new()
                        .child(Button::new("search-cancel").label("取消").on_click(
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
                            DialogAction::new()
                                .child(Button::new("search-ok").primary().label("搜索")),
                        ),
                )
                .on_ok(move |_, _, cx| {
                    let query = input_ok.read(cx).value().to_string();
                    browser_ok
                        .update(cx, |view, cx| {
                            view.state.update(cx, |s, _| s.apply_search(&query));
                            view.pending_dialog = AppDialog::None;
                        })
                        .ok();
                    true
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
            let note_title = self.state.read(cx).confirm_note_title.clone();
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
                            view.state.update(cx, |s, _| s.confirm_delete());
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

    /// Settings dialog mirroring the CLI config surface, plus Vim/sync toggles.
    fn open_settings_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let browser = cx.entity().downgrade();
        let url = self.set_url.clone();
        let user = self.set_user.clone();
        let pw = self.set_pw.clone();
        let e2ee = self.set_e2ee.clone();
        let ca = self.set_ca.clone();
        window.open_dialog(cx, move |dialog, _window, cx| {
            let read = |cx: &mut App| {
                browser
                    .update(cx, |view, cx| {
                        let s = view.state.read(cx);
                        (
                            SettingsFlag::InsecureTls.get(s),
                            SettingsFlag::Vim.get(s),
                            SettingsFlag::SyncOnStartup.get(s),
                            SettingsFlag::SyncOnQuit.get(s),
                            s.set_error.clone(),
                            s.set_message.clone(),
                        )
                    })
                    .unwrap_or((false, true, true, true, None, None))
            };
            let (insecure, vim, sync_start, sync_quit, error, message) = read(cx);
            let b_save = browser.clone();
            let b_cancel = browser.clone();
            let b_close = browser.clone();
            dialog
                .title("设置")
                .w(px(480.))
                .content({
                    let url = url.clone();
                    let user = user.clone();
                    let pw = pw.clone();
                    let e2ee = e2ee.clone();
                    let ca = ca.clone();
                    let b_toggles = browser.clone();
                    let error = error.clone();
                    let message = message.clone();
                    move |content, _window, _cx| {
                        content
                            .child(form_row("WebDAV URL", url.clone()))
                            .child(form_row("用户名", user.clone()))
                            .child(form_row("密码（留空不改）", pw.clone()))
                            .child(form_row("E2EE 密码（留空不改）", e2ee.clone()))
                            .child(form_row("CA 证书路径", ca.clone()))
                            .child(settings_toggle_row(
                                "set-insecure",
                                "跳过 TLS 校验",
                                insecure,
                                SettingsFlag::InsecureTls,
                                b_toggles.clone(),
                            ))
                            .child(settings_toggle_row(
                                "set-vim",
                                "Vim 编辑模式",
                                vim,
                                SettingsFlag::Vim,
                                b_toggles.clone(),
                            ))
                            .child(settings_toggle_row(
                                "set-sync-start",
                                "启动时同步",
                                sync_start,
                                SettingsFlag::SyncOnStartup,
                                b_toggles.clone(),
                            ))
                            .child(settings_toggle_row(
                                "set-sync-quit",
                                "退出前同步",
                                sync_quit,
                                SettingsFlag::SyncOnQuit,
                                b_toggles.clone(),
                            ))
                            .when_some(error.clone(), |el, err| el.child(error_line(err)))
                            .when_some(message.clone(), |el, msg| el.child(ok_line(msg)))
                    }
                })
                .footer(
                    DialogFooter::new()
                        .child(Button::new("set-cancel").label("取消").on_click(
                            move |_, window, cx| {
                                b_cancel
                                    .update(cx, |view, cx| {
                                        view.state.update(cx, |s, _| s.dialog = AppDialog::None);
                                        view.pending_dialog = AppDialog::None;
                                    })
                                    .ok();
                                window.close_dialog(cx);
                            },
                        ))
                        .child(
                            DialogAction::new()
                                .child(Button::new("set-save").primary().label("保存")),
                        ),
                )
                .on_ok(move |_, _, cx| {
                    b_save
                        .update(cx, |view, cx| {
                            if view.submit_settings_dialog(cx) {
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
                    b_close
                        .update(cx, |view, cx| {
                            view.state.update(cx, |s, _| s.dialog = AppDialog::None);
                            view.pending_dialog = AppDialog::None;
                        })
                        .ok();
                    true
                })
        });
    }

    /// About entry point. macOS opens the native panel; elsewhere the
    /// in-window dialog, reached from the Help menu. The main-window toolbar
    /// has no About button.
    fn show_about(&mut self, _: &ShowAbout, window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(target_os = "macos")]
        {
            let _ = (window, cx);
            crate::macos_about::show_native_about();
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.state.update(cx, |s, _| s.dialog = AppDialog::About);
            self.sync_dialog_from_state(window, cx);
            cx.notify();
        }
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
        let pending = self.state.read(cx).local_pending;
        let active_search = self.state.read(cx).search.clone();
        let base = if !worker.is_empty() {
            worker
        } else if status.is_empty() {
            "ready".to_string()
        } else {
            status
        };
        let status_line = if pending > 0 {
            format!("{base} · 本地 {pending} 项待同步")
        } else {
            base
        };
        let nb_focused = self.notebooks_focus.is_focused(window);
        let notes_focused = self.notes_focus.is_focused(window);

        div()
            .id("root")
            .key_context("NoteBrowser")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::sync_clicked))
            .on_action(cx.listener(Self::reader_password_clicked))
            .on_action(cx.listener(Self::quit_action))
            .on_action(cx.listener(Self::show_about))
            .on_action(cx.listener(Self::toggle_notebooks))
            .on_action(cx.listener(Self::toggle_notes))
            .on_action(cx.listener(Self::focus_notebooks))
            .on_action(cx.listener(Self::focus_notes))
            .on_action(cx.listener(Self::focus_body))
            .on_action(cx.listener(Self::toggle_edit))
            .on_action(cx.listener(Self::global_search))
            .on_action(cx.listener(Self::find_in_note))
            .on_action(cx.listener(Self::settings_clicked))
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
                    .child(
                        Button::new("btn-settings")
                            .small()
                            .outline()
                            .label("设置")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.state.update(cx, |s, _| s.open_settings());
                                let (url, user, ca) = {
                                    let s = this.state.read(cx);
                                    (
                                        s.set_webdav_url.clone(),
                                        s.set_username.clone(),
                                        s.set_ca_cert.clone(),
                                    )
                                };
                                this.set_url
                                    .update(cx, |i, cx| i.set_value(url, window, cx));
                                this.set_user
                                    .update(cx, |i, cx| i.set_value(user, window, cx));
                                this.set_ca.update(cx, |i, cx| i.set_value(ca, window, cx));
                                this.set_pw.update(cx, |i, cx| i.set_value("", window, cx));
                                this.set_e2ee
                                    .update(cx, |i, cx| i.set_value("", window, cx));
                                this.sync_dialog_from_state(window, cx);
                                cx.notify();
                            })),
                    ),
            )
            // ---- active-search indicator (the query is entered via ⌘⌥F) ----
            .when(!active_search.is_empty(), |el| {
                el.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_1()
                        .border_b_1()
                        .border_color(theme.border)
                        .child(
                            div()
                                .flex_1()
                                .text_color(theme.muted_foreground)
                                .child(SharedString::from(format!("🔍 搜索：{active_search}"))),
                        )
                        .child(
                            Button::new("search-clear")
                                .ghost()
                                .small()
                                .label("清除")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.state.update(cx, |s, _| s.apply_search(""));
                                    cx.notify();
                                })),
                        ),
                )
            })
            // ---- body: notebooks | note preview | notes ----
            .child(
                div()
                    .flex_1()
                    .flex()
                    .min_h_0()
                    .when(show_notebooks, |el| {
                        el.child(self.render_notebooks_column(nb_focused, cx))
                    })
                    .child(self.render_note_panel(window, cx))
                    .when(show_notes, |el| {
                        el.child(self.render_notes_column(notes_focused, cx))
                    }),
            )
    }
}

impl NoteBrowser {
    fn render_notebooks_column(
        &self,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let theme = cx.theme();
        let state = self.state.read(cx);
        let folders = state.folders.clone();
        let selected_folder = state.selected_folder.clone();
        let folder_cursor = state.folder_cursor;
        let browser = cx.entity().downgrade();
        let all_notes_bg = list_row_opacity(focused, selected_folder.is_none(), folder_cursor == 0);

        div()
            .id("notebooks-col")
            .w(px(NOTEBOOKS_WIDTH))
            .flex_shrink_0()
            .border_r_1()
            .border_color(theme.border)
            .flex()
            .flex_col()
            .bg(theme.muted)
            .track_focus(&self.notebooks_focus)
            .on_key_down(cx.listener(Self::notebooks_key))
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
                        div().flex().items_center().gap_0p5().child(
                            Button::new("nb-new").ghost().small().label("＋").on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.state.update(cx, |s, _| s.open_new_notebook());
                                    this.sync_dialog_from_state(window, cx);
                                    cx.notify();
                                }),
                            ),
                        ),
                    ),
            )
            .child(
                div()
                    .id("folder-all")
                    .px_3()
                    .py_1()
                    .cursor_pointer()
                    .when_some(all_notes_bg, |el, opacity| {
                        el.bg(theme.selection.opacity(opacity))
                    })
                    .hover(|s| s.bg(theme.selection.opacity(0.15)))
                    .child("All notes")
                    .on_click(cx.listener(|this, _, window, cx| {
                        window.focus(&this.notebooks_focus, cx);
                        this.state.update(cx, |s, _| s.select_folder(None));
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
                            .children(folders.iter().enumerate().map(|(i, folder)| {
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
                                let row_bg =
                                    list_row_opacity(focused, selected, folder_cursor == i + 1);
                                let folder_id = id.clone();
                                div()
                                    .id(SharedString::from(format!("folder-{id}")))
                                    .px_3()
                                    .py_1()
                                    .cursor_pointer()
                                    .when_some(row_bg, |el, opacity| {
                                        el.bg(theme.selection.opacity(opacity))
                                    })
                                    .hover(|s| s.bg(theme.selection.opacity(0.15)))
                                    .child(SharedString::from(title))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        window.focus(&this.notebooks_focus, cx);
                                        this.state
                                            .update(cx, |s, _| s.select_folder(Some(id.clone())));
                                        cx.notify();
                                    }))
                                    .context_menu({
                                        let browser = browser.clone();
                                        let folder_id = folder_id.clone();
                                        move |menu, _window, _cx| {
                                            menu.item(PopupMenuItem::new("新增").on_click({
                                                let browser = browser.clone();
                                                move |_, window, cx| {
                                                    browser
                                                        .update(cx, |view, cx| {
                                                            view.state.update(cx, |s, _| {
                                                                s.open_new_notebook();
                                                            });
                                                            view.sync_dialog_from_state(window, cx);
                                                            cx.notify();
                                                        })
                                                        .ok();
                                                }
                                            }))
                                            .item(PopupMenuItem::new("重命名").on_click({
                                                let browser = browser.clone();
                                                let folder_id = folder_id.clone();
                                                move |_, window, cx| {
                                                    browser
                                                        .update(cx, |view, cx| {
                                                            view.state.update(cx, |s, _| {
                                                                s.open_rename_notebook_of(
                                                                    &folder_id,
                                                                );
                                                            });
                                                            view.sync_dialog_from_state(window, cx);
                                                            cx.notify();
                                                        })
                                                        .ok();
                                                }
                                            }))
                                            .item(
                                                PopupMenuItem::new("删除").on_click({
                                                    let browser = browser.clone();
                                                    let folder_id = folder_id.clone();
                                                    move |_, window, cx| {
                                                        browser
                                                            .update(cx, |view, cx| {
                                                                view.state.update(cx, |s, _| {
                                                                    s.request_delete_notebook_of(
                                                                        &folder_id,
                                                                    );
                                                                });
                                                                view.sync_dialog_from_state(
                                                                    window, cx,
                                                                );
                                                                cx.notify();
                                                            })
                                                            .ok();
                                                    }
                                                }),
                                            )
                                        }
                                    })
                            })),
                    )
                    .child(Scrollbar::vertical(&self.notebook_scroll)),
            )
            .into_any_element()
    }

    fn render_notes_column(&self, focused: bool, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let theme = cx.theme();
        let state = self.state.read(cx);
        let notes = state.notes.clone();
        let folders = state.folders.clone();
        let note_tags = state.note_tags.clone();
        let search_hits = state.search_hits.clone();
        let protected = state.protected.clone();
        let selected_note = state.selected_note.clone();
        let note_cursor = state.note_cursor;
        let has_folder = state.selected_folder.is_some();
        let browser = cx.entity().downgrade();

        div()
            .id("notes-col")
            .w(px(NOTES_WIDTH))
            .flex_shrink_0()
            .border_l_1()
            .border_color(theme.border)
            .flex()
            .flex_col()
            .bg(theme.muted)
            .track_focus(&self.notes_focus)
            .on_key_down(cx.listener(Self::notes_key))
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
                        div().flex().items_center().gap_0p5().child(
                            Button::new("note-new")
                                .ghost()
                                .small()
                                .label("＋")
                                .disabled(!has_folder)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.state.update(cx, |s, _| s.new_note());
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
                            .children(notes.iter().enumerate().map(|(i, note)| {
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
                                let row_bg = list_row_opacity(focused, selected, note_cursor == i);
                                let tags = note_tags.get(&id).cloned().unwrap_or_default();
                                let snippet = search_hits.get(&id).cloned();
                                let menu_note_id = id.clone();

                                div()
                                    .id(SharedString::from(format!("note-{id}")))
                                    .px_3()
                                    .py_1()
                                    .cursor_pointer()
                                    .when_some(row_bg, |el, opacity| {
                                        el.bg(theme.selection.opacity(opacity))
                                    })
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
                                        window.focus(&this.notes_focus, cx);
                                        let note_id = id.clone();
                                        this.state.update(cx, |s, _| {
                                            // Save any pending edit before switching.
                                            s.note_cursor = i;
                                            s.end_edit();
                                            s.request_open_note(&note_id);
                                            s.sync_cursors_to_selection();
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
                                    .context_menu({
                                        let browser = browser.clone();
                                        let note_id = menu_note_id.clone();
                                        let note_protected = protected.contains(&note_id);
                                        let parent_id = note
                                            .props
                                            .get("parent_id")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        let folder_list: Vec<(String, String)> = folders
                                            .iter()
                                            .filter_map(|f| {
                                                let fid = f
                                                    .props
                                                    .get("id")
                                                    .and_then(|v| v.as_str())?
                                                    .to_string();
                                                let ftitle = f
                                                    .props
                                                    .get("title")
                                                    .and_then(|v| v.as_str())
                                                    .unwrap_or("")
                                                    .to_string();
                                                Some((fid, ftitle))
                                            })
                                            .collect();
                                        move |menu, window, cx| {
                                            let mut menu =
                                                menu.item(PopupMenuItem::new("新增").on_click({
                                                    let browser = browser.clone();
                                                    let parent_id = parent_id.clone();
                                                    move |_, _window, cx| {
                                                        browser
                                                            .update(cx, |view, cx| {
                                                                view.state.update(cx, |s, _| {
                                                                    if parent_id.is_empty() {
                                                                        s.new_note();
                                                                    } else {
                                                                        s.new_note_in(&parent_id);
                                                                    }
                                                                });
                                                                cx.notify();
                                                            })
                                                            .ok();
                                                    }
                                                }));
                                            menu = menu.item(
                                                PopupMenuItem::new("删除笔记").on_click({
                                                    let browser = browser.clone();
                                                    let note_id = note_id.clone();
                                                    move |_, window, cx| {
                                                        browser
                                                            .update(cx, |view, cx| {
                                                                view.state.update(cx, |s, _| {
                                                                    s.request_delete_note_of(
                                                                        &note_id,
                                                                    );
                                                                });
                                                                view.sync_dialog_from_state(
                                                                    window, cx,
                                                                );
                                                                cx.notify();
                                                            })
                                                            .ok();
                                                    }
                                                }),
                                            );
                                            menu = menu.item(
                                                PopupMenuItem::new(if note_protected {
                                                    "取消保护"
                                                } else {
                                                    "🔒 保护此笔记"
                                                })
                                                .on_click({
                                                    let browser = browser.clone();
                                                    let note_id = note_id.clone();
                                                    move |_, window, cx| {
                                                        browser
                                                            .update(cx, |view, cx| {
                                                                view.state.update(cx, |s, _| {
                                                                    s.toggle_protect_note(&note_id);
                                                                });
                                                                view.sync_dialog_from_state(
                                                                    window, cx,
                                                                );
                                                                cx.notify();
                                                            })
                                                            .ok();
                                                    }
                                                }),
                                            );
                                            let sub_browser = browser.clone();
                                            let sub_note_id = note_id.clone();
                                            let sub_folders = folder_list.clone();
                                            menu.submenu(
                                                "移动",
                                                window,
                                                cx,
                                                move |submenu, _window, _cx| {
                                                    let mut sub = submenu;
                                                    for (fid, ftitle) in sub_folders.clone() {
                                                        let browser = sub_browser.clone();
                                                        let note_id = sub_note_id.clone();
                                                        sub = sub.item(
                                                            PopupMenuItem::new(ftitle).on_click(
                                                                move |_, _window, cx| {
                                                                    browser
                                                                        .update(cx, |view, cx| {
                                                                            view.state.update(
                                                                                cx,
                                                                                |s, _| {
                                                                                    s.move_note_to(
                                                                                        &note_id,
                                                                                        &fid,
                                                                                    );
                                                                                },
                                                                            );
                                                                            cx.notify();
                                                                        })
                                                                        .ok();
                                                                },
                                                            ),
                                                        );
                                                    }
                                                    sub
                                                },
                                            )
                                        }
                                    })
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
        let vim_mode = state.config.vim_mode;
        let vim_state = state.vim_mode;
        let vim_pending = state.vim_pending.clone();
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
                            .when(editing && vim_mode, |el| {
                                let badge = if vim_pending.is_empty() {
                                    vim_state.label().to_string()
                                } else {
                                    vim_pending.clone()
                                };
                                el.child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(theme.muted_foreground)
                                        .when(vim_state != VimMode::Insert, |el| {
                                            el.bg(theme.muted).px_1().rounded(px(3.))
                                        })
                                        .child(SharedString::from(badge)),
                                )
                            })
                            .child(
                                Button::new("btn-edit-toggle")
                                    .outline()
                                    .small()
                                    .label(if editing { "预览" } else { "编辑" })
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        let was_editing = this.state.read(cx).editing;
                                        if was_editing {
                                            this.finish_edit(window, cx);
                                        } else {
                                            this.state.update(cx, |s, _| s.begin_edit());
                                            this.focus_note_editor(window, cx);
                                        }
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .child(if editing {
                let selection_ranges = if vim_state.is_visual() {
                    let st = self.note_edit.read(cx);
                    let anchor = if vim_state == VimMode::VisualBlock {
                        self.vim_block_anchor
                    } else {
                        self.vim_anchor
                    };
                    anchor
                        .map(|anchor| {
                            vim::visual_ranges(
                                vim_state,
                                &st.text().to_string(),
                                anchor,
                                st.cursor(),
                            )
                        })
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };
                let editor = div()
                    .id("note-editor")
                    .flex_1()
                    .min_h_0()
                    .relative()
                    // Display-column block math matches the editor's pixel
                    // advances only in a monospace face.
                    .font_family(theme.mono_font_family.clone())
                    .px_4()
                    .py_3()
                    .child(Textarea::new(&self.note_edit).h(relative(1.)))
                    .child(vim_cursor_overlay(
                        self.note_edit.clone(),
                        vim_state,
                        theme.caret.opacity(0.4),
                        theme.selection.opacity(0.35),
                        selection_ranges,
                    ));
                if vim_mode {
                    editor
                        .capture_key_down(cx.listener(Self::editor_key))
                        .into_any_element()
                } else {
                    editor.into_any_element()
                }
            } else {
                div()
                    .id("note-body-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.preview_scroll)
                    .track_focus(&self.body_focus)
                    .on_key_down(cx.listener(Self::body_key))
                    .on_mouse_down(
                        gpui_kit::MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            window.focus(&this.body_focus, cx);
                            cx.notify();
                        }),
                    )
                    .px_4()
                    .py_3()
                    .child(render_markdown(&body, &resource_dir))
                    .into_any_element()
            })
            .into_any_element()
    }
}

fn preview_scroll_target(current: f32, max: f32, viewport: f32, down: bool) -> f32 {
    let step = if viewport > 0.0 {
        viewport * 0.5
    } else {
        200.0
    };
    if down {
        (current - step).max(-max.abs())
    } else {
        (current + step).min(0.0)
    }
}

/// Scroll offset that places the caret's source line at the same relative
/// vertical position it had in the editor.
///
/// The preview cannot report per-source-line heights, so the line index is
/// mapped onto the preview's scrollable content by ratio. `screen_fraction` is
/// where the caret sat in the editor viewport (0 = top, 1 = bottom).
fn preview_offset_for_cursor(
    cursor_line: f32,
    total_lines: f32,
    screen_fraction: f32,
    max_offset: f32,
    viewport: f32,
) -> f32 {
    if max_offset <= 0.0 {
        return 0.0;
    }
    let content = max_offset + viewport.max(0.0);
    let line_ratio = (cursor_line / (total_lines - 1.0).max(1.0)).clamp(0.0, 1.0);
    let screen_fraction = screen_fraction.clamp(0.0, 1.0);
    let offset = -(content * line_ratio - viewport * screen_fraction);
    offset.clamp(-max_offset, 0.0)
}

/// Whether a captured editor position belongs to the note that is open now.
/// Switching notes clears the memory, but matching by id also keeps a stale
/// capture from ever moving the caret in a different note.
fn editor_memory_matches(memory: &EditorMemory, selected_note: Option<&str>) -> bool {
    selected_note == Some(memory.note_id.as_str())
}

/// Background opacity for one list row.
///
/// A focused list shows only its keyboard cursor row; an unfocused list shows
/// only the selected row.
fn list_row_opacity(focused: bool, selected: bool, is_cursor: bool) -> Option<f32> {
    if focused && is_cursor {
        Some(0.6)
    } else if !focused && selected {
        Some(0.35)
    } else {
        None
    }
}

/// Length of the byte prefix shared by `a` and `b`, kept on a char boundary.
fn common_prefix(a: &str, b: &str) -> usize {
    let (ab, bb) = (a.as_bytes(), b.as_bytes());
    let mut i = 0;
    while i < ab.len() && i < bb.len() && ab[i] == bb[i] {
        i += 1;
    }
    while i > 0 && (!a.is_char_boundary(i) || !b.is_char_boundary(i)) {
        i -= 1;
    }
    i
}

/// Length of the byte suffix shared by `a` and `b`, kept on a char boundary.
fn common_suffix(a: &str, b: &str) -> usize {
    let (ab, bb) = (a.as_bytes(), b.as_bytes());
    let mut n = 0;
    while n < ab.len() && n < bb.len() && ab[ab.len() - 1 - n] == bb[bb.len() - 1 - n] {
        n += 1;
    }
    while n > 0 && (!a.is_char_boundary(a.len() - n) || !b.is_char_boundary(b.len() - n)) {
        n -= 1;
    }
    n
}

/// Byte offset `count` characters after `from` (clamped to the text length).
fn nth_char_boundary(text: &str, from: usize, count: usize) -> usize {
    let from = from.min(text.len());
    let mut end = from;
    for (i, c) in text[from..].char_indices().take(count) {
        end = from + i + c.len_utf8();
    }
    end.min(text.len())
}

/// Place a cursor/selection rectangle in window coordinates.
///
/// `caret` is in window coordinates and is returned unchanged except for the
/// optional vertical scroll offset. `paint_quad` expects window coordinates,
/// so callers must not subtract the canvas origin.
fn block_caret_bounds(
    caret: Bounds<gpui_kit::Pixels>,
    scroll_y: gpui_kit::Pixels,
    width: gpui_kit::Pixels,
) -> Bounds<gpui_kit::Pixels> {
    Bounds::new(
        point(caret.origin.x, caret.origin.y + scroll_y),
        Size::new(width, caret.size.height),
    )
}

/// One character-cell advance for the caret at `cursor`.
///
/// On an empty line the newline has no glyph, so measure the nearest printable
/// character (preferring a single-width one). This keeps the block cursor one
/// character wide instead of falling back to a fraction of the line height.
fn caret_cell_width(
    state: &TextareaState,
    cursor: usize,
    line_height: gpui_kit::Pixels,
) -> gpui_kit::Pixels {
    let text = state.text();
    let next = text
        .slice(cursor..text.len())
        .chars()
        .next()
        .map(|c| cursor + c.len_utf8())
        .unwrap_or(cursor);
    if next > cursor {
        if let Some(width) = state
            .range_to_bounds(&(cursor..next))
            .map(|bounds| bounds.size.width)
            .filter(|width| *width > px(0.))
        {
            return width;
        }
    }
    let text = text.to_string();
    let mut nearest_single = None;
    let mut nearest_any = None;
    for (index, c) in text.char_indices() {
        if c == '\n' || c == '\r' {
            continue;
        }
        let distance = index.abs_diff(cursor);
        let candidate = (index, index + c.len_utf8(), distance);
        if nearest_any
            .map(|(_, _, best): (usize, usize, usize)| distance < best)
            .unwrap_or(true)
        {
            nearest_any = Some(candidate);
        }
        if vim::char_display_width(c) == 1
            && nearest_single
                .map(|(_, _, best): (usize, usize, usize)| distance < best)
                .unwrap_or(true)
        {
            nearest_single = Some(candidate);
        }
    }
    nearest_single
        .or(nearest_any)
        .and_then(|(start, end, _)| {
            state
                .range_to_bounds(&(start..end))
                .map(|bounds| bounds.size.width)
                .filter(|width| *width > px(0.))
        })
        .unwrap_or(line_height * 0.42)
}

/// Mode cursor plus Visual Block selection rectangles.
///
/// gpui's native caret is always a thin bar, so the overlay paints a
/// translucent block over the laid-out character cell. The cell bounds are
/// taken from the text layout (not the caret geometry), so the block follows
/// the glyph grid and vertical scrolling exactly. Normal/Visual/V-BLOCK show
/// the block; Insert leaves the native caret alone.
fn vim_cursor_overlay(
    state: Entity<TextareaState>,
    mode: VimMode,
    caret_color: gpui_kit::Hsla,
    block_color: gpui_kit::Hsla,
    block_ranges: Vec<std::ops::Range<usize>>,
) -> impl IntoElement {
    let show = mode != VimMode::Insert;
    canvas(
        |_, _, _| (),
        move |_bounds, _, window, cx| {
            let state = state.read(cx);
            for range in &block_ranges {
                if let Some(block) = state.range_to_bounds(range) {
                    let width = if mode == VimMode::VisualLine {
                        // Vim's linewise highlight covers the whole row, not
                        // only the glyphs on it.
                        (state.input_bounds().right() - block.origin.x).max(block.size.width)
                    } else if block.size.width > px(0.) {
                        block.size.width
                    } else {
                        block.size.height * 0.55
                    };
                    let rect = block_caret_bounds(block, px(0.), width);
                    window.paint_quad(gpui_kit::fill(rect, block_color));
                }
            }
            if !show {
                return;
            }
            let cursor = state.cursor();
            let text = state.text();
            let next = text
                .slice(cursor..text.len())
                .chars()
                .next()
                .map(|c| cursor + c.len_utf8())
                .unwrap_or(cursor);
            // The laid-out cell is already in paint coordinates (including
            // vertical scroll) and matches the glyph grid, unlike the caret
            // geometry which excludes the vertical scroll offset.
            let (base, scroll_y) = match state.range_to_bounds(&(cursor..next)) {
                Some(cell) => (cell, px(0.)),
                None => {
                    let Some((caret, _line_height)) = state.cursor_layout() else {
                        return;
                    };
                    (caret, state.scroll_offset().y)
                }
            };
            let width = caret_cell_width(&state, cursor, base.size.height);
            // A full line-height block looks like it spans two rows on an
            // empty line. Use the native caret height, centered in the cell.
            let caret_height = state
                .cursor_layout()
                .map(|(caret, _)| caret.size.height)
                .unwrap_or(base.size.height);
            let top = base.origin.y + (base.size.height - caret_height) / 2.;
            let base = Bounds::new(
                point(base.origin.x, top),
                Size::new(base.size.width, caret_height),
            );
            let rect = block_caret_bounds(base, scroll_y, width);
            window.paint_quad(gpui_kit::fill(rect, caret_color));
        },
    )
    .absolute()
    .inset_0()
}

/// One boolean setting exposed in the settings dialog.
#[derive(Clone, Copy)]
enum SettingsFlag {
    InsecureTls,
    Vim,
    SyncOnStartup,
    SyncOnQuit,
}

impl SettingsFlag {
    fn get(self, s: &AppState) -> bool {
        match self {
            SettingsFlag::InsecureTls => s.set_insecure_tls,
            SettingsFlag::Vim => s.set_vim_mode,
            SettingsFlag::SyncOnStartup => s.set_sync_on_startup,
            SettingsFlag::SyncOnQuit => s.set_sync_on_quit,
        }
    }

    fn toggle(self, s: &mut AppState) {
        match self {
            SettingsFlag::InsecureTls => s.set_insecure_tls = !s.set_insecure_tls,
            SettingsFlag::Vim => s.set_vim_mode = !s.set_vim_mode,
            SettingsFlag::SyncOnStartup => s.set_sync_on_startup = !s.set_sync_on_startup,
            SettingsFlag::SyncOnQuit => s.set_sync_on_quit = !s.set_sync_on_quit,
        }
    }
}

/// One on/off row for the settings dialog (label left, toggle button right).
fn settings_toggle_row(
    id: &'static str,
    label: &str,
    value: bool,
    flag: SettingsFlag,
    browser: gpui_kit::WeakEntity<NoteBrowser>,
) -> gpui_kit::AnyElement {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_2()
        .child(SharedString::from(label.to_string()))
        .child(
            Button::new(id)
                .small()
                .label(if value { "开启" } else { "关闭" })
                .on_click(move |_, _, cx| {
                    browser
                        .update(cx, |view, cx| {
                            view.state.update(cx, |s, _| flag.toggle(s));
                            cx.notify();
                        })
                        .ok();
                }),
        )
        .into_any_element()
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

    #[test]
    fn insert_diff_helpers_recover_typed_text() {
        assert_eq!(common_prefix("abX", "abY"), 2);
        assert_eq!(common_suffix("Xcd", "Ycd"), 2);
        // Inserted text is what lies between the shared prefix and suffix.
        let old = "hello world";
        let new = "hello brave world";
        let prefix = common_prefix(old, new);
        let suffix = common_suffix(&old[prefix..], &new[prefix..]);
        assert_eq!(&new[prefix..new.len() - suffix], "brave ");

        // Multi-byte text never splits a character boundary.
        assert_eq!(common_prefix("你好", "你坏"), "你".len());
        assert_eq!(common_suffix("你好", "很好"), "好".len());
        assert_eq!(nth_char_boundary("你好世界", 0, 2), "你好".len());
        assert_eq!(nth_char_boundary("你好世界", "你".len(), 1), "你好".len());
        assert_eq!(nth_char_boundary("abc", 1, 99), 3);
    }

    #[test]
    fn block_caret_bounds_adds_vertical_scroll_and_keeps_height() {
        let caret = Bounds::new(point(px(10.), px(20.)), Size::new(px(2.), px(18.)));
        let rect = block_caret_bounds(caret, px(7.), px(11.));
        assert_eq!(rect.origin, point(px(10.), px(27.)));
        assert_eq!(rect.size, Size::new(px(11.), px(18.)));
    }

    #[test]
    fn preview_scroll_uses_half_viewport_and_clamps() {
        assert_eq!(preview_scroll_target(0.0, 500.0, 200.0, true), -100.0);
        assert_eq!(preview_scroll_target(-100.0, 500.0, 200.0, false), 0.0);
        assert_eq!(preview_scroll_target(-480.0, 500.0, 200.0, true), -500.0);
        assert_eq!(preview_scroll_target(0.0, 500.0, 0.0, true), -200.0);
    }

    #[test]
    fn preview_offset_keeps_the_cursor_line_at_its_screen_fraction() {
        // First line at the top: no scroll. Last line at the bottom: full scroll.
        assert_eq!(preview_offset_for_cursor(0.0, 10.0, 0.0, 500.0, 200.0), 0.0);
        assert_eq!(
            preview_offset_for_cursor(9.0, 10.0, 1.0, 500.0, 200.0),
            -500.0
        );
        // Middle line at the middle of the viewport: content 700 -> 350 - 100.
        assert_eq!(
            preview_offset_for_cursor(4.5, 10.0, 0.5, 500.0, 200.0),
            -250.0
        );
        // A document that fits does not scroll.
        assert_eq!(preview_offset_for_cursor(5.0, 10.0, 0.5, 0.0, 200.0), 0.0);
        // A single-line document never divides by zero or scrolls.
        assert_eq!(preview_offset_for_cursor(0.0, 1.0, 0.0, 500.0, 200.0), 0.0);
        // Target above the content start clamps to the top.
        assert_eq!(preview_offset_for_cursor(0.0, 10.0, 1.0, 500.0, 200.0), 0.0);
        // Viewport zero still maps the line ratio onto the content.
        assert_eq!(
            preview_offset_for_cursor(9.0, 10.0, 0.0, 500.0, 0.0),
            -500.0
        );
    }

    #[test]
    fn edit_memory_only_applies_to_its_own_note() {
        let memory = EditorMemory {
            note_id: "note-a".into(),
            line: 12,
            column: 4,
            scroll_y: -240.0,
        };
        assert!(editor_memory_matches(&memory, Some("note-a")));
        assert!(!editor_memory_matches(&memory, Some("note-b")));
        assert!(!editor_memory_matches(&memory, None));
    }

    #[test]
    fn focused_list_shows_only_the_cursor_row() {
        assert_eq!(list_row_opacity(true, false, true), Some(0.6));
        assert_eq!(list_row_opacity(true, true, false), None);
        assert_eq!(list_row_opacity(false, true, false), Some(0.35));
        assert_eq!(list_row_opacity(false, false, true), None);
    }

    #[test]
    fn vim_insert_and_line_keys_map_to_expected_offsets() {
        let text = "  one\ntwo\n   \n你好";
        let mut pending = String::new();
        let normal =
            |pending: &mut String, key: &str| vim::translate(VimMode::Normal, pending, key, false);

        // `A` targets the logical line end, `I` the first non-blank.
        let a = normal(&mut pending, "A");
        assert_eq!(vim_insert_entry_at(text, 7, &a), Some(9));
        let i = normal(&mut pending, "I");
        assert_eq!(vim_insert_entry_at(text, 3, &i), Some(2));
        assert_eq!(vim_insert_entry_at(text, 10, &i), Some(13));
        assert_eq!(vim_insert_entry_at(text, 3, &VimAction::Swallow), None);

        // `0` / `$` target the logical start and the last character.
        let VimAction::Move { kind, .. } = normal(&mut pending, "0") else {
            panic!("0 should be a line-start motion");
        };
        assert_eq!(vim_line_move_at(text, 7, kind), Some(6));
        let VimAction::Move { kind, .. } = normal(&mut pending, "$") else {
            panic!("$ should be a line-end motion");
        };
        assert_eq!(vim_line_move_at(text, 7, kind), Some(8));
        assert_eq!(vim_line_move_at(text, text.len(), kind), Some(17));
        assert_eq!(vim_line_move_at(text, 7, MoveKind::Down), None);
    }

    #[gpui::test]
    fn vim_line_edits_drive_the_real_textarea(cx: &mut gpui::TestAppContext) {
        struct Root(Entity<TextareaState>);
        impl Render for Root {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div().size_full().child(self.0.clone())
            }
        }

        let mut state = None;
        let window = cx.add_window(|window, cx| {
            gpui_kit::init(cx);
            let textarea = cx.new(|cx| TextareaState::new(window, cx));
            state = Some(textarea.clone());
            Root(textarea)
        });
        let textarea = state.expect("textarea state");

        struct OpenCase {
            text: &'static str,
            cursor: usize,
            above: bool,
            expected: &'static str,
            expected_cursor: usize,
        }
        let cases = [
            OpenCase {
                text: "hello world",
                cursor: 5,
                above: false,
                expected: "hello world\n",
                expected_cursor: 12,
            },
            OpenCase {
                text: "hello world",
                cursor: 5,
                above: true,
                expected: "\nhello world",
                expected_cursor: 0,
            },
            OpenCase {
                text: "one\ntwo\nthree",
                cursor: 5,
                above: false,
                expected: "one\ntwo\n\nthree",
                expected_cursor: 8,
            },
            OpenCase {
                text: "one\ntwo\nthree",
                cursor: 5,
                above: true,
                expected: "one\n\ntwo\nthree",
                expected_cursor: 4,
            },
            OpenCase {
                text: "one\ntwo",
                cursor: 5,
                above: false,
                expected: "one\ntwo\n",
                expected_cursor: 8,
            },
            OpenCase {
                text: "你好世界",
                cursor: 3,
                above: false,
                expected: "你好世界\n",
                expected_cursor: 13,
            },
            OpenCase {
                text: "你好世界",
                cursor: 3,
                above: true,
                expected: "\n你好世界",
                expected_cursor: 0,
            },
        ];
        for case in cases {
            window
                .update(cx, |_root, window, cx| {
                    textarea.update(cx, |s, cx| {
                        s.set_value(case.text, window, cx);
                        s.set_selected_range(case.cursor..case.cursor, cx);
                        insert_vim_line(s, case.text, case.cursor, case.above, window, cx);
                        assert_eq!(s.value().to_string(), case.expected);
                        assert_eq!(s.cursor(), case.expected_cursor);
                    });
                })
                .unwrap();
        }

        // `I`, `A`, `0`, `$` only move the caret; verify the offsets the
        // action layer uses land where expected in a real textarea.
        let text = "  hello\nworld";
        window
            .update(cx, |_root, window, cx| {
                textarea.update(cx, |s, cx| {
                    s.set_value(text, window, cx);
                    let checks = [
                        (vim::first_non_blank_offset(text, 4), 2),
                        (vim::line_end_offset(text, 4), 7),
                        (vim::line_start_offset(text, 4), 0),
                        (vim::line_last_char_offset(text, 4), 6),
                    ];
                    for (offset, expected) in checks {
                        s.set_selected_range(offset..offset, cx);
                        assert_eq!(s.cursor(), expected);
                    }
                });
            })
            .unwrap();

        // `A` appends after the last character; `I` inserts at the first
        // non-blank. Typing goes through the textarea, not just the caret.
        window
            .update(cx, |_root, window, cx| {
                textarea.update(cx, |s, cx| {
                    s.set_value(text, window, cx);
                    let at = vim::line_end_offset(text, 4);
                    s.set_selected_range(at..at, cx);
                    s.insert("!", window, cx);
                    assert_eq!(s.value().to_string(), "  hello!\nworld");

                    s.set_value(text, window, cx);
                    let at = vim::first_non_blank_offset(text, 4);
                    s.set_selected_range(at..at, cx);
                    s.insert(">", window, cx);
                    assert_eq!(s.value().to_string(), "  >hello\nworld");
                });
            })
            .unwrap();

        // The newline is inserted as one atomic edit, so a single Undo
        // removes the whole opened line.
        window
            .update(cx, |_root, window, cx| {
                textarea.update(cx, |s, cx| {
                    s.set_value("hello", window, cx);
                    s.set_selected_range(0..0, cx);
                    insert_vim_line(s, "hello", 0, false, window, cx);
                    assert_eq!(s.value().to_string(), "hello\n");
                    s.focus_handle(cx).focus(window, cx);
                });
            })
            .unwrap();
        cx.run_until_parked();
        cx.dispatch_action(window.into(), Undo);
        cx.run_until_parked();
        window
            .update(cx, |_root, _window, cx| {
                textarea.update(cx, |s, _cx| {
                    assert_eq!(s.value().to_string(), "hello");
                });
            })
            .unwrap();
    }
}
