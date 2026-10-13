//! Modal dialog layer for GPUI.
//!
//! Structure (full-window scrim + centered card + focus trap + Esc/Enter
//! semantics) references longbridge/gpui-kit's Dialog (Apache-2.0); written
//! against gpui 0.2.2 primitives (`deferred` for paint order, `anchored`
//! for centering) with no gpui-kit dependency.

use gpui::{
    actions, div, prelude::*, px, App, Context, KeyBinding, Render, SharedString, Styled, Window,
    black, rgb, white,
};

actions!(dialog, [Escape, Confirm]);

/// Register dialog key bindings. Call once at startup.
pub fn register(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("escape", Escape, Some("Dialog")),
        KeyBinding::new("enter", Confirm, Some("Dialog")),
    ]);
}

/// What the user did with the dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogAction {
    /// Primary button (or Enter).
    Confirm,
    /// Secondary button / scrim click / Esc.
    Dismiss,
}

/// Retained dialog state owned by the parent view. `on_action` receives the
/// outcome; the parent closes by clearing its `Option<Entity<DialogState>>`.
pub struct DialogState {
    title: SharedString,
    on_action: Box<dyn Fn(DialogAction, &mut App) + 'static>,
    /// Extra body elements embedded by the parent view (form fields,
    /// messages), handed over once via [`DialogState::with_body`]. AnyElement
    /// is arena-allocated (not Clone), so render consumes the vec.
    body: Option<Vec<gpui::AnyElement>>,
}

impl DialogState {
    pub fn new(
        title: impl Into<SharedString>,
        on_action: impl Fn(DialogAction, &mut App) + 'static,
    ) -> Self {
        Self {
            title: title.into(),
            on_action: Box::new(on_action),
            body: None,
        }
    }

    /// Embed form fields / messages into the card body. Called by the
    /// parent view right after `cx.new` builds the dialog entity; consumed
    /// by the first render.
    pub fn with_body(mut self, body: Vec<gpui::AnyElement>) -> Self {
        self.body = Some(body);
        self
    }

    fn confirm(&mut self, _: &Confirm, _: &mut Window, cx: &mut Context<Self>) {
        (self.on_action)(DialogAction::Confirm, cx);
    }

    fn escape(&mut self, _: &Escape, _: &mut Window, cx: &mut Context<Self>) {
        (self.on_action)(DialogAction::Dismiss, cx);
    }

    /// Click on the scrim outside the card dismisses.
    fn scrim_click(&mut self, _: &gpui::MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        (self.on_action)(DialogAction::Dismiss, cx);
    }
}

impl Render for DialogState {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        let title = self.title.clone();
        // deferred paints above everything already laid out in the window.
        gpui::deferred(
            div()
                .id("dialog-scrim")
                .absolute()
                .size_full()
                .top_0()
                .left_0()
                .bg(gpui::black().opacity(0.4))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(Self::scrim_click),
                )
                .key_context("Dialog")
                .on_action(cx.listener(Self::confirm))
                .on_action(cx.listener(Self::escape))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    // Stop propagation so clicks inside the card don't hit
                    // the scrim's dismiss handler.
                    div()
                        .id("dialog-card")
                        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
                            cx.stop_propagation();
                        })
                        .w(px(420.))
                        .rounded_md()
                        .border_1()
                        .border_color(rgb(0xd4d4d8))
                        .bg(white())
                        .shadow_lg()
                        .p_4()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .text_size(px(13.))
                        .text_color(black())
                        .child(
                            div()
                                .text_size(px(15.))
                                .font_weight(gpui::FontWeight::BOLD)
                                .child(title),
                        )
                        .children(self.body.take().unwrap_or_default())
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap_2()
                                .child(
                                    div()
                                        .id("dialog-cancel")
                                        .px_3()
                                        .py_1()
                                        .rounded_sm()
                                        .border_1()
                                        .border_color(rgb(0xd4d4d8))
                                        .cursor_pointer()
                                        .hover(|s| s.bg(rgb(0xf4f4f5)))
                                        .child("取消")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            (this.on_action)(
                                                DialogAction::Dismiss,
                                                cx,
                                            );
                                        })),
                                )
                                .child(
                                    div()
                                        .id("dialog-ok")
                                        .px_3()
                                        .py_1()
                                        .rounded_sm()
                                        .bg(rgb(0x2563eb))
                                        .text_color(white())
                                        .cursor_pointer()
                                        .hover(|s| s.bg(rgb(0x1d4ed8)))
                                        .child("确定")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            (this.on_action)(
                                                DialogAction::Confirm,
                                                cx,
                                            );
                                        })),
                                ),
                        ),
                ),
        )
        .with_priority(1)
        .into_any_element()
    }
}
