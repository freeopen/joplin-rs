//! Markdown rendering for GPUI: comrak AST -> GPUI element tree.
//!
//! Component design (parse-to-AST once, render blocks as stacked divs,
//! inline runs as styled elements) references the gpui-kit TextView approach
//! (longbridge/gpui-kit, Apache-2.0); the implementation here is written
//! against gpui 0.2.2 + comrak 0.42 directly with no gpui-kit dependency.
//!
//! Supported: headings h1-h3, paragraphs, emphasis/strong/code inline,
//! fenced code blocks, block quotes, bullet/ordered/task lists, links
//! (opened via cx.open_url), images (via `img` with Resource::Path),
//! GFM tables, thematic breaks, strikethrough. Math and raw HTML blocks
//! are not rendered.

use std::path::PathBuf;
use std::sync::Arc;

use comrak::nodes::{AstNode, ListType, NodeValue};
use comrak::{parse_document, Arena, ExtensionOptions, Options};
use gpui::{
    div, img, prelude::*, px, AnyElement, FontWeight, SharedString, black, blue, rgb,
};

/// Muted grays for chrome (gpui 0.2.2 has color functions, not a gray(n) helper).
const GRAY_BORDER: u32 = 0xd4d4d8;
const GRAY_FILL: u32 = 0xf4f4f5;
const GRAY_TEXT: u32 = 0x52525b;
const CODE_TEXT: u32 = 0xb91c1c;

/// Parse Markdown with GFM extensions (tables, task lists, strikethrough,
/// autolinks) - the same feature set egui_commonmark provided in the egui UI.
fn build_options() -> Options<'static> {
    let mut options = Options::default();
    options.extension = ExtensionOptions {
        table: true,
        strikethrough: true,
        tasklist: true,
        autolink: true,
        ..Default::default()
    };
    options
}

/// One-shot parse + render: parse into a fresh arena and build the element
/// tree in the same pass so the arena's lifetime covers element construction.
pub fn render_markdown(source: &str, resource_dir: &PathBuf) -> AnyElement {
    let arena = Arena::new();
    let options = build_options();
    let root = parse_document(&arena, source, &options);
    let view = MarkdownView {
        resource_dir: resource_dir.clone(),
    };
    view.render(root)
}

/// Renders comrak block/inline nodes as GPUI elements.
struct MarkdownView {
    /// Directory containing decrypted resource files (written by app.rs
    /// open_note). Markdown image URLs that name a file here render as
    /// `img` elements; remote URLs fall back to a plain link.
    resource_dir: PathBuf,
}

impl MarkdownView {
    fn render<'a>(&self, root: &'a AstNode<'a>) -> AnyElement {
        let mut blocks: Vec<AnyElement> = Vec::new();
        for child in root.children() {
            if let Some(el) = self.render_block(child) {
                blocks.push(el);
            }
        }
        div()
            .flex()
            .flex_col()
            .gap_3()
            .text_size(px(14.))
            .text_color(black())
            .children(blocks)
            .into_any_element()
    }

    fn render_block<'a>(&self, node: &'a AstNode<'a>) -> Option<AnyElement> {
        match &node.data.borrow().value {
            NodeValue::Document => None,
            NodeValue::Heading(h) => {
                let size = match h.level {
                    1 => px(24.),
                    2 => px(19.),
                    _ => px(16.),
                };
                let runs = self.render_inlines(node);
                Some(
                    div()
                        .flex()
                        .flex_col()
                        .text_size(size)
                        .font_weight(FontWeight::BOLD)
                        .children(runs)
                        .into_any_element(),
                )
            }
            NodeValue::Paragraph => {
                let runs = self.render_inlines(node);
                Some(div().children(runs).into_any_element())
            }
            NodeValue::CodeBlock(cb) => {
                // comrak 0.42: NodeCodeBlock.literal is a String.
                Some(
                    div()
                        .rounded_sm()
                        .px_3()
                        .py_2()
                        .bg(rgb(GRAY_FILL))
                        .text_color(rgb(0x1c1917))
                        .child(SharedString::from(cb.literal.clone()))
                        .into_any_element(),
                )
            }
            NodeValue::BlockQuote => {
                let mut blocks: Vec<AnyElement> = Vec::new();
                for child in node.children() {
                    if let Some(el) = self.render_block(child) {
                        blocks.push(el);
                    }
                }
                Some(
                    div()
                        .pl_3()
                        .border_l_2()
                        .border_color(rgb(GRAY_BORDER))
                        .text_color(rgb(GRAY_TEXT))
                        .flex()
                        .flex_col()
                        .gap_2()
                        .children(blocks)
                        .into_any_element(),
                )
            }
            NodeValue::List(list) => {
                let mut items: Vec<AnyElement> = Vec::new();
                for (ix, item) in node.children().enumerate() {
                    let marker = match list.list_type {
                        ListType::Bullet => "\u{2022}".to_string(),
                        ListType::Ordered => format!("{}.", ix + 1),
                    };
                    // Task list item: the item itself carries TaskItem(char?).
                    let marker_el = if let NodeValue::TaskItem(checked) =
                        &item.data.borrow().value
                    {
                        div().w(px(16.)).child(if checked.is_some() {
                            SharedString::from("[x]")
                        } else {
                            SharedString::from("[ ]")
                        })
                    } else {
                        div().w(px(16.)).child(marker)
                    };
                    items.push(
                        div()
                            .flex()
                            .gap_2()
                            .child(marker_el)
                            .children(self.render_inlines(item))
                            .into_any_element(),
                    );
                }
                Some(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .pl_2()
                        .children(items)
                        .into_any_element(),
                )
            }
            NodeValue::Table(_) => {
                let mut rows: Vec<AnyElement> = Vec::new();
                for row in node.children() {
                    let header = matches!(&row.data.borrow().value, NodeValue::TableRow(true));
                    let mut cells: Vec<AnyElement> = Vec::new();
                    for cell in row.children() {
                        let mut cell_el = div().flex_1().px_2().py_1();
                        if header {
                            cell_el = cell_el
                                .font_weight(FontWeight::BOLD)
                                .border_b_1()
                                .border_color(rgb(GRAY_BORDER));
                        }
                        cells.push(
                            cell_el
                                .children(self.render_inlines(cell))
                                .into_any_element(),
                        );
                    }
                    rows.push(
                        div()
                            .flex()
                            .border_b_1()
                            .border_color(rgb(GRAY_FILL))
                            .children(cells)
                            .into_any_element(),
                    );
                }
                Some(
                    div()
                        .flex()
                        .flex_col()
                        .border_1()
                        .rounded_sm()
                        .border_color(rgb(GRAY_BORDER))
                        .children(rows)
                        .into_any_element(),
                )
            }
            NodeValue::ThematicBreak => Some(
                div()
                    .h(px(1.))
                    .w_full()
                    .bg(rgb(GRAY_BORDER))
                    .my_2()
                    .into_any_element(),
            ),
            NodeValue::HtmlBlock(_) => None, // not rendered
            _ => {
                let runs = self.render_inlines(node);
                if runs.is_empty() {
                    None
                } else {
                    Some(div().children(runs).into_any_element())
                }
            }
        }
    }

    fn render_inlines<'a>(&self, node: &'a AstNode<'a>) -> Vec<AnyElement> {
        let mut out: Vec<AnyElement> = Vec::new();
        for child in node.children() {
            match &child.data.borrow().value {
                NodeValue::Text(t) => {
                    out.push(div().child(SharedString::from(t.clone())).into_any_element());
                }
                NodeValue::Code(code) => {
                    out.push(
                        div()
                            .px_1()
                            .rounded_xs()
                            .bg(rgb(GRAY_FILL))
                            .text_color(rgb(CODE_TEXT))
                            .child(SharedString::from(code.literal.clone()))
                            .into_any_element(),
                    );
                }
                NodeValue::Emph => {
                    out.push(
                        div()
                            .flex()
                            .italic()
                            .children(self.render_inlines(child))
                            .into_any_element(),
                    );
                }
                NodeValue::Strong => {
                    out.push(
                        div()
                            .flex()
                            .font_weight(FontWeight::BOLD)
                            .children(self.render_inlines(child))
                            .into_any_element(),
                    );
                }
                NodeValue::Strikethrough => {
                    out.push(
                        div()
                            .flex()
                            .line_through()
                            .children(self.render_inlines(child))
                            .into_any_element(),
                    );
                }
                NodeValue::Link(link) => {
                    let url = link.url.clone();
                    let mut inner = self.render_inlines(child);
                    if inner.is_empty() {
                        inner.push(
                            div()
                                .child(SharedString::from(url.clone()))
                                .into_any_element(),
                        );
                    }
                    out.push(
                        div()
                            .id(SharedString::from(format!("md-link-{}", out.len())))
                            .flex()
                            .text_color(blue())
                            .cursor_pointer()
                            .hover(|s| s.underline())
                            .children(inner)
                            .on_click(move |_, _, cx| {
                                cx.open_url(&url);
                            })
                            .into_any_element(),
                    );
                }
                NodeValue::Image(image) => {
                    let url = image.url.clone();
                    // Joplin resource links were rewritten by app.rs to temp
                    // file paths; those render via Resource::Path.
                    let is_remote = url.starts_with("http://") || url.starts_with("https://");
                    let path = if is_remote {
                        None
                    } else {
                        Some(self.resource_dir.join(&url))
                    };
                    let element = match path {
                        Some(p) if p.exists() => img(gpui::ImageSource::Resource(
                            gpui::Resource::Path(Arc::from(p.as_path())),
                        ))
                        .max_w_full()
                        .into_any_element(),
                        _ => div()
                            .text_color(blue())
                            .child(SharedString::from(url.clone()))
                            .into_any_element(),
                    };
                    out.push(element);
                }
                NodeValue::SoftBreak | NodeValue::LineBreak => {
                    out.push(div().h(px(6.)).into_any_element());
                }
                NodeValue::HtmlInline(html) => {
                    out.push(
                        div().child(SharedString::from(html.clone())).into_any_element(),
                    );
                }
                _ => {
                    out.extend(self.render_inlines(child));
                }
            }
        }
        out
    }
}
