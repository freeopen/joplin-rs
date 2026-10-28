//! Preview-only Markdown preprocessing.
//!
//! `gpui-base` deliberately collapses CommonMark *soft* line breaks (a single
//! `\n` inside a paragraph) to a space so hard-wrapped prose reflows. Joplin
//! shows a line break for every source newline, so the preview would differ
//! from the note the user wrote. The text view exposes no option for this, and
//! its plugin hooks never see a `Node::Text`, so the only place to change the
//! behaviour is the Markdown source handed to `TextView::markdown`.
//!
//! [`with_hard_breaks`] appends the CommonMark hard-break marker (two spaces)
//! to ordinary source lines. Structural Markdown (fenced code, indented code,
//! tables, frontmatter, link reference definitions) is left untouched.

/// Convert soft line breaks to CommonMark hard breaks, leaving block structure
/// that would change meaning alone.
///
/// The transformation is line-local and idempotent for lines that already end
/// in a hard break (`  ` or `\`).
pub fn with_hard_breaks(body: &str) -> String {
    let lines: Vec<&str> = body.split_inclusive('\n').collect();
    let mut out = String::with_capacity(body.len() + body.len() / 8);
    let mut in_fence: Option<(char, usize)> = None;
    // A leading `---` is frontmatter only when a closing marker follows;
    // otherwise it is an ordinary thematic break and prose below it should
    // still get breaks.
    let mut in_frontmatter = lines
        .first()
        .is_some_and(|line| line_content(line).trim_end() == "---")
        && lines
            .iter()
            .skip(1)
            .any(|line| matches!(line_content(line).trim_end(), "---" | "..."));
    let mut in_table = false;

    for (i, raw) in lines.iter().enumerate() {
        let (content, ending) = split_line_ending(raw);

        // Fenced code: copy every line verbatim until the closing fence.
        if let Some((fence, len)) = in_fence {
            out.push_str(content);
            out.push_str(ending);
            if is_closing_fence(content, fence, len) {
                in_fence = None;
            }
            continue;
        }

        // Frontmatter: copy verbatim between the opening and closing marker.
        if in_frontmatter {
            out.push_str(content);
            out.push_str(ending);
            if i > 0 && matches!(content.trim_end(), "---" | "...") {
                in_frontmatter = false;
            }
            continue;
        }

        if let Some((fence, len)) = opening_fence(content) {
            in_fence = Some((fence, len));
            out.push_str(content);
            out.push_str(ending);
            continue;
        }

        // GFM table: rows and their delimiter line are block structure, not
        // soft-wrapped prose.
        if in_table {
            if content.trim().is_empty() || !content.contains('|') {
                in_table = false;
            } else {
                out.push_str(content);
                out.push_str(ending);
                continue;
            }
        }
        if is_table_delimiter(content) {
            in_table = true;
            out.push_str(content);
            out.push_str(ending);
            continue;
        }
        if content.contains('|') {
            let next_is_delimiter = lines
                .get(i + 1)
                .is_some_and(|line| is_table_delimiter(line_content(line)));
            if next_is_delimiter {
                in_table = true;
                out.push_str(content);
                out.push_str(ending);
                continue;
            }
        }

        let trimmed = content.trim_start();
        let indented_code = content.starts_with('\t') || content.starts_with("    ");
        let link_definition = trimmed.starts_with('[') && trimmed.contains("]:");
        let html_block = trimmed.starts_with('<');
        let separator_only = !trimmed.is_empty()
            && trimmed
                .chars()
                .all(|character| character == '-' || character == '=');
        let next_line = lines.get(i + 1).map(|line| line_content(line));
        let next_is_setext = next_line.is_some_and(|next| {
            let next = next.trim();
            !next.is_empty() && (next.chars().all(|c| c == '=') || next.chars().all(|c| c == '-'))
        });
        let next_is_blank = next_line.is_none_or(|next| next.trim().is_empty());
        let already_hard_break = content.ends_with("  ") || content.ends_with('\\');

        let add_break = !ending.is_empty()
            && !content.trim().is_empty()
            && !indented_code
            && !link_definition
            && !html_block
            && !separator_only
            && !next_is_setext
            && !next_is_blank
            && !already_hard_break;

        out.push_str(content);
        if add_break {
            out.push_str("  ");
        }
        out.push_str(ending);
    }

    out
}

fn line_content(line: &str) -> &str {
    split_line_ending(line).0
}

fn split_line_ending(line: &str) -> (&str, &str) {
    if let Some(stripped) = line.strip_suffix("\r\n") {
        (stripped, "\r\n")
    } else if let Some(stripped) = line.strip_suffix('\n') {
        (stripped, "\n")
    } else {
        (line, "")
    }
}

/// `(fence_char, fence_length)` when `line` opens a fenced code block.
fn opening_fence(line: &str) -> Option<(char, usize)> {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    let fence = trimmed.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let len = trimmed.chars().take_while(|&c| c == fence).count();
    (len >= 3).then_some((fence, len))
}

fn is_closing_fence(line: &str, fence: char, len: usize) -> bool {
    let trimmed = line.trim();
    let count = trimmed.chars().take_while(|&c| c == fence).count();
    count >= len
}

/// A GFM table delimiter row such as `|---|:--:|` (outer pipes optional).
fn is_table_delimiter(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() || !trimmed.contains('|') || !trimmed.contains('-') {
        return false;
    }
    let cells = trimmed.trim_matches('|').split('|');
    let mut any = false;
    for cell in cells {
        let cell = cell.trim();
        if cell.is_empty() {
            return false;
        }
        if !cell.chars().all(|c| matches!(c, '-' | ':')) || !cell.contains('-') {
            return false;
        }
        any = true;
    }
    any
}

#[cfg(test)]
mod tests {
    use super::with_hard_breaks;

    #[test]
    fn soft_break_becomes_hard_break() {
        assert_eq!(with_hard_breaks("a\nb"), "a  \nb");
        assert_eq!(with_hard_breaks("a\nb\nc"), "a  \nb  \nc");
    }

    #[test]
    fn blank_lines_are_untouched() {
        assert_eq!(with_hard_breaks("a\n\nb"), "a\n\nb");
        assert_eq!(with_hard_breaks("a\n\n\nb"), "a\n\n\nb");
    }

    #[test]
    fn existing_hard_breaks_are_idempotent() {
        assert_eq!(with_hard_breaks("a  \nb"), "a  \nb");
        assert_eq!(with_hard_breaks("a\\\nb"), "a\\\nb");
    }

    #[test]
    fn fenced_code_is_verbatim() {
        let src = "```rust\nlet a = 1;\nlet b = 2;\n```\nafter";
        assert_eq!(with_hard_breaks(src), src);
        let src = "~~~\na\n~~~\nb";
        assert_eq!(with_hard_breaks(src), src);
    }

    #[test]
    fn indented_code_is_verbatim() {
        assert_eq!(with_hard_breaks("    a\n    b"), "    a\n    b");
    }

    #[test]
    fn table_rows_are_verbatim() {
        let src = "| a | b |\n|---|:--:|\n| 1 | 2 |\nafter";
        assert_eq!(with_hard_breaks(src), src);
    }

    #[test]
    fn frontmatter_is_verbatim() {
        let src = "---\ntitle: x\n---\nbody\nnext";
        assert_eq!(with_hard_breaks(src), "---\ntitle: x\n---\nbody  \nnext");
    }

    #[test]
    fn crlf_is_preserved() {
        assert_eq!(with_hard_breaks("a\r\nb"), "a  \r\nb");
        assert_eq!(with_hard_breaks("a\r\n\r\nb"), "a\r\n\r\nb");
    }

    #[test]
    fn setext_heading_and_reference_definition_are_untouched() {
        assert_eq!(with_hard_breaks("Title\n---"), "Title\n---");
        assert_eq!(
            with_hard_breaks("[label]: https://x\nnext"),
            "[label]: https://x\nnext"
        );
    }

    #[test]
    fn chinese_prose_gets_breaks() {
        assert_eq!(with_hard_breaks("第一行\n第二行"), "第一行  \n第二行");
    }
}
