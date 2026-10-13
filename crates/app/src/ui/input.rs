//! Minimal single-line text input for GPUI.
//!
//! Element structure and IME/selection handling follow Zed's official
//! `gpui` `input.rs` example (Apache-2.0), trimmed to what this app needs:
//! single-line editing, Enter-to-submit, change callback, placeholder.
//! Design conventions (state owned by the parent view, `EntityInputHandler`
//! for platform integration) reference the gpui-kit project (Apache-2.0).

use std::ops::Range;

use gpui::{
    actions, div, prelude::*, px, App, Bounds, Context, Entity, EntityInputHandler,
    FocusHandle, Focusable, KeyBinding, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    Pixels, ShapedLine, SharedString, StyledText, TextRun, UTF16Selection, UnderlineStyle,
    Window, WindowBounds, WindowOptions, black, white,
};

actions!(input, [Submit, Backspace, Delete, Left, Right, Home, End, SelectAll, Paste, Cut, Copy, ShowCharacterPalette]);

pub struct InputState {
    focus_handle: FocusHandle,
    pub content: SharedString,
    pub placeholder: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    pub on_submit: bool,
}

impl InputState {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: "".into(),
            placeholder: "".into(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            on_submit: false,
        }
    }

    pub fn placeholder(mut self, placeholder: &str) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        self.pause_cursor_blink();
        cx.notify();
    }

    fn pause_cursor_blink(&mut self) {}

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    pub fn set_text(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.content = text.into();
        self.selected_range = self.content.len()..self.content.len();
        cx.notify();
    }

    pub fn value(&self) -> &str {
        &self.content
    }

    fn replace_text_in_range(&mut self, range: Range<usize>, text: &str, cx: &mut Context<Self>) {
        let start = self.content.ceil_char_boundary(range.start);
        let end = self.content.ceil_char_boundary(range.end);
        self.content = SharedString::from(
            (self.content[..start].to_owned() + text + &self.content[end..]).into_bytes(),
        );
        let new_cursor = start + text.len();
        self.selected_range = new_cursor..new_cursor;
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
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let text = &self.content;
        let start = text.floor_char_boundary(range_utf16.start);
        let end = text.ceil_char_boundary(range_utf16.end);
        actual_range.replace(start..end);
        Some(text[start..end].to_string())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let to_utf16 = |b: usize| self.content[..b].chars().map(char::len_utf16).sum();
        Some(UTF16Selection {
            range: to_utf16(self.selected_range.start)..to_utf16(self.selected_range.end),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(
        &mut self,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range.clone()
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(&mut self, range: Range<usize>, text: &str, w: &mut Window, cx: &mut Context<Self>) {
        InputState::replace_text_in_range(self, range, text, cx);
        let _ = w;
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
        self.replace_text_in_range(range, new_text, cx);
        self.marked_range = Some(self.cursor_offset()..self.cursor_offset() + new_text.len());
        if let Some(sel) = new_selected_range {
            self.selected_range = sel;
        }
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let start = self.content.floor_char_boundary(range_utf16.start.min(self.content.len()));
        Some(Bounds::new(
            gpui::point(px(start as f32 * 8.0), px(0.)),
            gpui::size(px((range_utf16.len()) as f32 * 8.0), px(24.)),
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
