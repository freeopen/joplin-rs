//! Minimal single-line text input for GPUI.
//!
//! Element structure and IME/selection handling follow Zed's official
//! `gpui` `input.rs` example (Apache-2.0), trimmed to what this app needs:
//! single-line editing, Enter-to-submit, change callback, placeholder.
//! Trait signatures verified against gpui 0.2.2 `EntityInputHandler`.

use std::ops::Range;

use gpui::{
    actions, div, prelude::*, px, App, Context, EntityInputHandler, FocusHandle,
    Focusable, KeyBinding, Pixels, SharedString, UTF16Selection, Window, black, rgb,
};

actions!(input, [Submit, Backspace, Delete, Left, Right, Home, End, SelectAll, Paste, Copy, ShowCharacterPalette]);

const INPUT_BG: u32 = 0xffffff;
const INPUT_BORDER: u32 = 0xd4d4d8;
const INPUT_FOCUS_BORDER: u32 = 0x2563eb;
const PLACEHOLDER_TEXT: u32 = 0xa1a1aa;

/// Retained state of one single-line input; owned by the parent view as an
/// `Entity<InputState>` so focus/selection/IME survive re-renders.
pub struct InputState {
    focus_handle: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    selected_range: Range<usize>,
    marked_range: Option<Range<usize>>,
    /// Character offset shown by the blinking caret. GPUI has no built-in
    /// caret; we draw a 1px div at this offset each paint.
    blink_paused: bool,
    /// Called when Enter is pressed; the parent wires business logic.
    on_submit: Option<Box<dyn Fn(&str, &mut App) + 'static>>,
    /// Called on every text change (search-as-you-type).
    on_change: Option<Box<dyn Fn(&str, &mut App) + 'static>>,
}

impl InputState {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: SharedString::from(""),
            placeholder: SharedString::from(""),
            selected_range: 0..0,
            marked_range: None,
            blink_paused: false,
            on_submit: None,
            on_change: None,
        }
    }

    pub fn placeholder(mut self, placeholder: impl Into<SharedString>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    pub fn on_submit(
        mut self,
        f: impl Fn(&str, &mut App) + 'static,
    ) -> Self {
        self.on_submit = Some(Box::new(f));
        self
    }

    pub fn on_change(mut self, f: impl Fn(&str, &mut App) + 'static) -> Self {
        self.on_change = Some(Box::new(f));
        self
    }

    pub fn value(&self) -> &str {
        &self.content
    }

    pub fn set_value(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.content = text.into();
        self.selected_range = self.content.len()..self.content.len();
        cx.notify();
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus_handle
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        self.blink_paused = false;
        cx.notify();
    }

    fn cursor_offset(&self) -> usize {
        self.selected_range.end
    }

    fn submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(f) = self.on_submit.take() {
            let text = self.content.to_string();
            f(&text, cx);
            self.on_submit = Some(f);
        }
    }

    fn backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.start == self.selected_range.end {
            if self.selected_range.start > 0 {
                let start = self.content.floor_char_boundary(self.selected_range.start - 1);
                self.replace_range(start..self.selected_range.end, "", cx);
            }
        } else {
            self.replace_range(self.selected_range.clone(), "", cx);
        }
    }

    fn delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.start == self.selected_range.end {
            if self.selected_range.end < self.content.len() {
                let end = self.content.ceil_char_boundary(self.selected_range.end + 1);
                self.replace_range(self.selected_range.start..end, "", cx);
            }
        } else {
            self.replace_range(self.selected_range.clone(), "", cx);
        }
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        let start = self.content.floor_char_boundary(self.selected_range.start.saturating_sub(1));
        self.move_to(start, cx);
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        let end = self.content.ceil_char_boundary((self.selected_range.end + 1).min(self.content.len()));
        self.move_to(end, cx);
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.selected_range = 0..self.content.len();
        cx.notify();
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) {
            self.replace_range(self.selected_range.clone(), &text, cx);
        }
        let _ = window;
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        let text = self.content[self.selected_range.clone()].to_string();
        if !text.is_empty() {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        }
    }

    /// Insert `text` at the selection, replacing it, then notify change.
    fn replace_range(&mut self, range: Range<usize>, text: &str, cx: &mut Context<Self>) {
        let start = self.content.floor_char_boundary(range.start);
        let end = self.content.ceil_char_boundary(range.end);
        let mut new_content = String::with_capacity(self.content.len() + text.len());
        new_content.push_str(&self.content[..start]);
        new_content.push_str(text);
        new_content.push_str(&self.content[end..]);
        self.content = SharedString::from(new_content);
        let cursor = start + text.len();
        self.selected_range = cursor..cursor;
        if let Some(f) = self.on_change.take() {
            let value = self.content.to_string();
            f(&value, cx);
            self.on_change = Some(f);
        }
        cx.notify();
    }
}

impl Focusable for InputState {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EntityInputHandler for InputState {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let text = &self.content;
        // UTF-16 offsets -> UTF-8 offsets (IME works in UTF-16 code units).
        let mut utf16 = 0usize;
        let mut start8 = None;
        let mut end8 = None;
        for (i, ch) in text.char_indices() {
            if start8.is_none() && utf16 >= range_utf16.start {
                start8 = Some(i);
            }
            if end8.is_none() && utf16 >= range_utf16.end {
                end8 = Some(i);
            }
            utf16 += ch.len_utf16();
            if start8.is_some() && end8.is_some() {
                break;
            }
        }
        let start = start8.unwrap_or(text.len());
        let end = end8.unwrap_or(text.len());
        *adjusted_range = Some(start..end);
        Some(text[start..end].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let to_utf16 = |b: usize| self.content[..b].chars().map(char::len_utf16).sum();
        Some(UTF16Selection {
            range: to_utf16(self.selected_range.start)..to_utf16(self.selected_range.end),
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range.clone()
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range.unwrap_or(self.selected_range.clone());
        self.replace_range(range, text, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range.unwrap_or(self.selected_range.clone());
        let start = self.content.floor_char_boundary(range.start);
        self.replace_range(range, new_text, cx);
        let marked = start..start + new_text.len();
        self.marked_range = Some(marked.clone());
        self.selected_range = new_selected_range.unwrap_or(marked);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: gpui::Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<gpui::Bounds<Pixels>> {
        // Approximate caret position: 8px per UTF-16 unit is good enough for
        // IME popup placement; the exact metric needs a text layout pass.
        let chars: usize = self
            .content
            .char_indices()
            .take_while(|(i, _)| {
                let mut utf16 = 0usize;
                for c in self.content[..*i].chars() {
                    utf16 += c.len_utf16();
                }
                utf16 < range_utf16.start
            })
            .count();
        let x = element_bounds.left() + px(chars as f32 * 8.0);
        Some(gpui::Bounds::new(
            gpui::point(x, element_bounds.top()),
            gpui::size(px(1.), element_bounds.bottom() - element_bounds.top()),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _: gpui::Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

impl gpui::Render for InputState {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        let focused = self.focus_handle.is_focused(window);
        let border = if focused {
            INPUT_FOCUS_BORDER
        } else {
            INPUT_BORDER
        };
        let content = self.content.clone();
        let placeholder = self.placeholder.clone();
        let cursor = self.cursor_offset();
        let _ = cursor;

        div()
            .id("input-box")
            .track_focus(&self.focus_handle)
            .key_context("Input")
            .on_action(cx.listener(Self::submit))
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::copy))
            .on_mouse_down(gpui::MouseButton::Left, cx.listener(|this, _, window, _cx| {
                window.focus(&this.focus_handle);
            }))
            .on_click(|_, _, _| {})
            .flex()
            .items_center()
            .px_2()
            .py_1()
            .h(px(28.))
            .w_full()
            .rounded_sm()
            .border_1()
            .border_color(rgb(border))
            .bg(rgb(INPUT_BG))
            .text_size(px(13.))
            .text_color(black())
            .when(content.is_empty() && !placeholder.is_empty(), |el| {
                el.child(
                    div().text_color(rgb(PLACEHOLDER_TEXT)).child(placeholder),
                )
            })
            .when(!content.is_empty(), |el| el.child(content))
            .when(focused && self.selected_range.is_empty(), |el| {
                el.child(div().w(px(1.)).h(px(16.)).bg(rgb(0x18181b)))
            })
    }
}

impl InputState {
    /// Register the key bindings this input handles. Call once at startup.
    pub fn register(cx: &mut App) {
        cx.bind_keys([
            KeyBinding::new("enter", Submit, Some("Input")),
            KeyBinding::new("backspace", Backspace, Some("Input")),
            KeyBinding::new("delete", Delete, Some("Input")),
            KeyBinding::new("left", Left, Some("Input")),
            KeyBinding::new("right", Right, Some("Input")),
            KeyBinding::new("home", Home, Some("Input")),
            KeyBinding::new("end", End, Some("Input")),
            KeyBinding::new("cmd-a", SelectAll, Some("Input")),
            KeyBinding::new("cmd-v", Paste, Some("Input")),
            KeyBinding::new("cmd-c", Copy, Some("Input")),
        ]);
    }
}
