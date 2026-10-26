//! Minimal Vim-style modal editing for the note body editor.
//!
//! Only the *pure* key translation and text-offset arithmetic live here:
//! [`translate`] maps one key (plus a multi-key `pending` buffer such as `g`,
//! `d`, a count, `:` or `/`) to a [`VimAction`]. The view layer owns the
//! textarea and performs the action (dispatching gpui movement actions,
//! editing the rope, saving, ...).
//!
//! Supported: Normal/Insert/Visual modes, motions `h j k l w b 0 $ gg G`,
//! `Ctrl-f/b/d/u`, counts (`3j`, `12G`, `2dd`, `3x`, `d2w`), operators `x dd
//! yy dw d$`, `p P`, `u`/`Ctrl-r`, `i a I A o O`, `.`, `/word ?word n N`,
//! `:w :q :wq :x`, `ZZ` and `:%s/old/new/g` / `:n1,n2s/old/new/g` /
//! `:s/old/new[/g]`. Text objects, macros, named registers, marks, `f/t`, `J`,
//! `>>/<<`, and visual line/block modes are out of scope.

/// Editor mode. Insert passes keys straight through; Normal and Visual
/// consume them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VimMode {
    Normal,
    Insert,
    Visual,
}

impl VimMode {
    /// Short badge label for the title bar.
    pub fn label(self) -> &'static str {
        match self {
            VimMode::Normal => "NORMAL",
            VimMode::Insert => "INSERT",
            VimMode::Visual => "VISUAL",
        }
    }
}

/// A movement produced by a normal-mode motion key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveKind {
    Left,
    Right,
    Up,
    Down,
    WordForward,
    WordBackward,
    LineStart,
    LineEnd,
    DocStart,
    DocEnd,
    PageDown,
    PageUp,
    HalfPageDown,
    HalfPageUp,
}

/// Parsed `:s` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubSpec {
    /// First line of the range (1-based, inclusive). `None` means the whole
    /// document when [`Self::whole`] is set, otherwise the current line.
    pub start: Option<usize>,
    /// Last line of the range (1-based, inclusive).
    pub end: Option<usize>,
    /// `%` prefix: every line.
    pub whole: bool,
    pub pattern: String,
    pub replacement: String,
    /// `g` flag: replace every occurrence per line, not just the first.
    pub global: bool,
}

/// What the view should do with one translated keystroke.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VimAction {
    /// Let the key reach the textarea (Insert-mode typing).
    PassThrough,
    /// Consumed; nothing to do (also swallows unmapped Normal-mode keys).
    Swallow,
    /// Enter Insert mode.
    ToInsert,
    /// Return to Normal mode (Esc).
    ToNormal,
    /// Start Visual mode (`v`).
    ToVisual,
    /// Leave Visual mode and collapse the selection (`Esc` / `v`).
    VisualExit,
    /// A motion repeated `count` times.
    Move {
        kind: MoveKind,
        count: usize,
    },
    /// `nG` / `ngg` (1-based line).
    GotoLine(usize),
    /// `x` (or `3x`) - delete characters at the cursor.
    DeleteChar(usize),
    /// `dd`, `2dd` - delete whole lines.
    DeleteLines(usize),
    /// `yy`, `2yy` - yank whole lines.
    YankLines(usize),
    /// `dw`, `d2w` - delete to the start of the next word.
    DeleteWord(usize),
    /// `d$` - delete to the end of the line.
    DeleteToLineEnd(usize),
    /// Visual-mode `d`.
    DeleteSelection,
    /// Visual-mode `y`.
    YankSelection,
    /// `p` / `P`, repeated `count` times.
    Paste {
        before: bool,
        count: usize,
    },
    Undo,
    Redo,
    InsertAfterCursor,
    InsertAtLineStart,
    InsertAtLineEnd,
    OpenLineBelow,
    OpenLineAbove,
    /// `:w` - persist the working copy.
    Save,
    /// `:q`, `:wq`, `:x`, `ZZ` - leave edit mode (saving first).
    FinishEdit,
    /// Search from the cursor in one direction.
    Search {
        query: String,
        backward: bool,
    },
    /// `n` / `N`.
    SearchNext {
        backward: bool,
    },
    /// `:s` / `:%s` / `:n1,n2s`.
    Substitute(SubSpec),
    /// `.` - replay the last change.
    Repeat,
    /// A command line the layer does not understand; the message is shown
    /// in the status line instead of being silently dropped.
    Error(String),
    /// `:` / `/` / `?` opened (or extended) a command buffer; the badge
    /// shows it.
    Pending,
}

/// The one-character form of a keystroke, if it is a plain character key.
fn single_char(key: &str) -> Option<char> {
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Some(c),
        _ => None,
    }
}

/// Parse a pending buffer as `[count] op [count]`.
fn parse_pending(p: &str) -> Option<(Option<usize>, Option<char>, Option<usize>)> {
    let mut chars = p.chars().peekable();
    let mut before = String::new();
    while let Some(c) = chars.peek().copied() {
        if c.is_ascii_digit() {
            before.push(c);
            chars.next();
        } else {
            break;
        }
    }
    let before: Option<usize> = if before.is_empty() {
        None
    } else {
        Some(before.parse().ok()?)
    };
    let op = match chars.next() {
        Some(c) => Some(c),
        None => return Some((before, None, None)),
    };
    let mut after = String::new();
    for c in chars {
        if c.is_ascii_digit() {
            after.push(c);
        } else {
            return None;
        }
    }
    let after: Option<usize> = if after.is_empty() {
        None
    } else {
        Some(after.parse().ok()?)
    };
    Some((before, op, after))
}

/// Count encoded in a count-only prefix (`"12"` -> `12`), defaulting to 1.
fn pending_count(p: &str) -> usize {
    match parse_pending(p) {
        Some((Some(n), None, None)) => n.max(1),
        _ => 1,
    }
}

/// Combined count of `[count] op [count]`.
fn op_count(before: Option<usize>, after: Option<usize>) -> usize {
    before.unwrap_or(1).max(1) * after.unwrap_or(1).max(1)
}

/// Translate one key press.
///
/// `pending` carries the partially-typed multi-key prefix across calls and is
/// mutated in place (e.g. `g` then `g`, a count, or a `:w` command line).
/// `ctrl` is true when Control was held (`Ctrl-r`, `Ctrl-f`, ...).
pub fn translate(mode: VimMode, pending: &mut String, key: &str, ctrl: bool) -> VimAction {
    use VimAction::*;

    if mode == VimMode::Insert {
        return if key == "escape" {
            ToNormal
        } else {
            PassThrough
        };
    }

    // Command line (`:` prefix): collect until Enter.
    if pending.starts_with(':') {
        return match key {
            "enter" => {
                let cmd = pending.clone();
                pending.clear();
                parse_command(&cmd)
            }
            "escape" => {
                pending.clear();
                Swallow
            }
            "backspace" => {
                if pending.len() > 1 {
                    pending.pop();
                }
                Pending
            }
            _ => {
                if single_char(key).is_some() {
                    pending.push_str(key);
                }
                Pending
            }
        };
    }

    // Search line (`/` or `?` prefix).
    if pending.starts_with('/') || pending.starts_with('?') {
        return match key {
            "enter" => {
                let backward = pending.starts_with('?');
                let query = pending[1..].to_string();
                pending.clear();
                if query.is_empty() {
                    Swallow
                } else {
                    Search { query, backward }
                }
            }
            "escape" => {
                pending.clear();
                Swallow
            }
            "backspace" => {
                if pending.len() > 1 {
                    pending.pop();
                }
                Pending
            }
            _ => {
                if single_char(key).is_some() {
                    pending.push_str(key);
                }
                Pending
            }
        };
    }

    if ctrl {
        pending.clear();
        return match key {
            "r" => Redo,
            "f" => Move {
                kind: MoveKind::PageDown,
                count: 1,
            },
            "b" => Move {
                kind: MoveKind::PageUp,
                count: 1,
            },
            "d" => Move {
                kind: MoveKind::HalfPageDown,
                count: 1,
            },
            "u" => Move {
                kind: MoveKind::HalfPageUp,
                count: 1,
            },
            _ => Swallow,
        };
    }

    if key == "escape" {
        pending.clear();
        return if mode == VimMode::Visual {
            VisualExit
        } else {
            Swallow
        };
    }

    let Some(ch) = single_char(key) else {
        pending.clear();
        return Swallow;
    };

    // `g` prefix: `gg` + optional count; any other follower cancels.
    if pending.ends_with('g') {
        let prefix = &pending[..pending.len() - 1];
        let count = pending_count(prefix);
        let explicit = !prefix.is_empty();
        pending.clear();
        return if ch == 'g' {
            if explicit {
                GotoLine(count)
            } else {
                Move {
                    kind: MoveKind::DocStart,
                    count: 1,
                }
            }
        } else {
            Swallow
        };
    }

    // `Z` prefix: only `ZZ` is bound.
    if pending == "Z" {
        pending.clear();
        return if ch == 'Z' { FinishEdit } else { Swallow };
    }

    // Operator prefixes (`d`, `y`) followed by a motion.
    if let Some((before, Some(op), after)) = parse_pending(pending) {
        // A bare operator ("d") followed by a digit is still collecting the
        // post-operator count, so let the digit branch below handle it.
        if after.is_some() || !ch.is_ascii_digit() {
            let count = op_count(before, after);
            match op {
                'd' => {
                    pending.clear();
                    return match ch {
                        'd' => DeleteLines(count),
                        'w' => DeleteWord(count),
                        '$' => DeleteToLineEnd(count),
                        _ => Swallow,
                    };
                }
                'y' => {
                    pending.clear();
                    return match ch {
                        'y' => YankLines(count),
                        _ => Swallow,
                    };
                }
                _ => {}
            }
        }
    }

    if ch.is_ascii_digit() {
        if pending.is_empty() && ch == '0' {
            return Move {
                kind: MoveKind::LineStart,
                count: 1,
            };
        }
        let count_context = match parse_pending(pending) {
            Some((_, None, None)) => true,
            Some((_, Some('d' | 'y'), _)) => true,
            _ => false,
        };
        if count_context {
            pending.push(ch);
        } else {
            pending.clear();
        }
        return Swallow;
    }

    let action = match ch {
        'h' | 'j' | 'k' | 'l' | 'w' | 'b' | '$' => {
            let count = pending_count(pending);
            pending.clear();
            let kind = match ch {
                'h' => MoveKind::Left,
                'j' => MoveKind::Down,
                'k' => MoveKind::Up,
                'w' => MoveKind::WordForward,
                'b' => MoveKind::WordBackward,
                '$' => MoveKind::LineEnd,
                _ => MoveKind::Right,
            };
            Move { kind, count }
        }
        'G' => {
            let explicit = !pending.is_empty() && pending.chars().all(|c| c.is_ascii_digit());
            let n = pending_count(pending);
            pending.clear();
            if explicit {
                GotoLine(n)
            } else {
                Move {
                    kind: MoveKind::DocEnd,
                    count: 1,
                }
            }
        }
        'g' => {
            pending.push('g');
            Pending
        }
        'Z' => {
            pending.clear();
            pending.push('Z');
            Pending
        }
        'x' => {
            let count = pending_count(pending);
            pending.clear();
            DeleteChar(count)
        }
        'd' | 'y' => {
            pending.push(ch);
            Pending
        }
        'p' | 'P' => {
            let count = pending_count(pending);
            pending.clear();
            Paste {
                before: ch == 'P',
                count,
            }
        }
        'u' => {
            pending.clear();
            Undo
        }
        'i' => {
            pending.clear();
            ToInsert
        }
        'a' => {
            pending.clear();
            InsertAfterCursor
        }
        'I' => {
            pending.clear();
            InsertAtLineStart
        }
        'A' => {
            pending.clear();
            InsertAtLineEnd
        }
        'o' => {
            pending.clear();
            OpenLineBelow
        }
        'O' => {
            pending.clear();
            OpenLineAbove
        }
        'v' => {
            pending.clear();
            if mode == VimMode::Visual {
                VisualExit
            } else {
                ToVisual
            }
        }
        'n' => {
            pending.clear();
            SearchNext { backward: false }
        }
        'N' => {
            pending.clear();
            SearchNext { backward: true }
        }
        '.' => {
            pending.clear();
            Repeat
        }
        '/' | '?' => {
            pending.clear();
            pending.push(ch);
            Pending
        }
        ':' => {
            pending.clear();
            pending.push(':');
            Pending
        }
        _ => {
            pending.clear();
            Swallow
        }
    };
    // Visual mode edits the selection instead of a motion target.
    if mode != VimMode::Visual {
        return action;
    }
    match ch {
        'd' => {
            pending.clear();
            VimAction::DeleteSelection
        }
        'y' => {
            pending.clear();
            VimAction::YankSelection
        }
        'i' | 'a' | 'I' | 'A' | 'o' | 'O' | 'p' | 'P' | 'x' => VimAction::Swallow,
        _ => action,
    }
}

/// Parse a complete `:...` command line.
pub fn parse_command(cmd: &str) -> VimAction {
    let body = cmd.trim_start_matches(':').trim();
    match body {
        "" => return VimAction::Swallow,
        "w" | "w!" => return VimAction::Save,
        "q" | "wq" | "wq!" | "x" => return VimAction::FinishEdit,
        _ => {}
    }
    if body.starts_with('s') || body.contains('s') {
        match parse_substitute(body) {
            Ok(spec) => VimAction::Substitute(spec),
            Err(msg) => VimAction::Error(msg),
        }
    } else {
        VimAction::Error(format!("unsupported command :{body}"))
    }
}

/// Parse `s/old/new[/g]`, `%s/...`, or `n1,n2s/...` (the leading `:` is
/// already stripped).
pub fn parse_substitute(body: &str) -> Result<SubSpec, String> {
    // Find the `s` whose prefix is a valid range (empty prefix included).
    let mut split = None;
    for (i, c) in body.char_indices() {
        if c == 's' && parse_range(&body[..i]).is_ok() {
            split = Some(i);
            break;
        }
    }
    let Some(i) = split else {
        return Err(format!("not a substitute command :{body}"));
    };
    let (range_part, rest) = body.split_at(i);
    let (start, end, whole) = parse_range(range_part)?;
    let rest = &rest[1..];
    let mut chars = rest.chars();
    let Some(delim) = chars.next() else {
        return Err("missing substitute delimiter".into());
    };
    let tail = chars.as_str();
    let mut parts: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut escaped = false;
    for c in tail.chars() {
        if escaped {
            if c != delim {
                cur.push('\\');
            }
            cur.push(c);
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == delim {
            parts.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    parts.push(cur);
    if parts.len() < 2 || parts.len() > 3 {
        return Err("substitute needs /pattern/replacement/".into());
    }
    let pattern = parts[0].clone();
    let replacement = parts[1].clone();
    let flags = parts.get(2).cloned().unwrap_or_default();
    if !flags.is_empty() && flags != "g" {
        return Err(format!("unsupported substitute flags `{flags}`"));
    }
    if pattern.is_empty() {
        return Err("empty substitute pattern".into());
    }
    Ok(SubSpec {
        start,
        end,
        whole,
        pattern,
        replacement,
        global: flags == "g",
    })
}

fn parse_range(p: &str) -> Result<(Option<usize>, Option<usize>, bool), String> {
    if p.is_empty() {
        return Ok((None, None, false));
    }
    if p == "%" {
        return Ok((None, None, true));
    }
    let mut parts = p.split(',');
    let first = parts.next().unwrap_or("");
    let second = parts.next();
    if parts.next().is_some() {
        return Err(format!("bad range `{p}`"));
    }
    let parse_line = |s: &str| -> Result<usize, String> {
        let n: usize = s.parse().map_err(|_| format!("bad line number `{s}`"))?;
        if n == 0 {
            return Err("line numbers start at 1".into());
        }
        Ok(n)
    };
    let a = parse_line(first)?;
    match second {
        Some(s) => Ok((Some(a), Some(parse_line(s)?), false)),
        None => Ok((Some(a), Some(a), false)),
    }
}

// ---- pure text helpers ---------------------------------------------------

/// Byte offset of the start of the line containing `cursor`.
pub fn line_start_offset(text: &str, cursor: usize) -> usize {
    let cursor = cursor.min(text.len());
    text[..cursor].rfind('\n').map(|i| i + 1).unwrap_or(0)
}

/// Byte offset of the end of the line containing `cursor` (the `\n`, or the
/// end of the text).
pub fn line_end_offset(text: &str, cursor: usize) -> usize {
    let cursor = cursor.min(text.len());
    text[cursor..]
        .find('\n')
        .map(|i| cursor + i)
        .unwrap_or(text.len())
}

/// Byte range deleted by `dd` / `2dd`: whole lines, including their trailing
/// newlines. For the final line without a trailing newline, the preceding
/// newline is included so no blank line is left behind, but nothing more.
pub fn delete_line_range(text: &str, cursor: usize, count: usize) -> (usize, usize) {
    let start = line_start_offset(text, cursor);
    let mut end = start;
    for _ in 0..count.max(1) {
        let e = line_end_offset(text, end);
        if e >= text.len() {
            end = text.len();
            break;
        }
        end = e + 1;
    }
    if end == text.len() && !text.ends_with('\n') && start > 0 {
        return (start - 1, end);
    }
    (start, end)
}

/// Yanked text for `yy` / `2yy` (no trailing newline).
pub fn yank_line_text(text: &str, cursor: usize, count: usize) -> String {
    let (start, end) = delete_line_range(text, cursor, count);
    text[start..end]
        .trim_end_matches('\n')
        .trim_start_matches('\n')
        .to_string()
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn is_punct(c: char) -> bool {
    !c.is_whitespace() && !is_word_char(c)
}

/// Byte offset after `count` `w` motions from `cursor`.
pub fn word_forward_offset(text: &str, cursor: usize, count: usize) -> usize {
    let mut offset = cursor.min(text.len());
    for _ in 0..count.max(1) {
        let Some(first) = text[offset..].chars().next() else {
            break;
        };
        if is_word_char(first) {
            offset = consume_run(text, offset, is_word_char);
        } else if is_punct(first) {
            offset = consume_run(text, offset, is_punct);
        } else {
            offset = consume_run(text, offset, |c| c.is_whitespace());
        }
        offset = consume_run(text, offset, |c| c.is_whitespace());
        if offset >= text.len() {
            return text.len();
        }
    }
    offset
}

fn consume_run(text: &str, from: usize, pred: impl Fn(char) -> bool) -> usize {
    let mut end = from;
    for (i, c) in text[from..].char_indices() {
        if pred(c) {
            end = from + i + c.len_utf8();
        } else {
            break;
        }
    }
    end
}

/// Byte offset of the start of the previous word (`b`), `count` times.
pub fn word_backward_offset(text: &str, cursor: usize, count: usize) -> usize {
    let mut offset = cursor.min(text.len());
    for _ in 0..count.max(1) {
        if offset == 0 {
            break;
        }
        // Step back over whitespace first.
        while offset > 0 {
            let (i, c) = prev_char(text, offset);
            if c.is_whitespace() {
                offset = i;
            } else {
                break;
            }
        }
        if offset == 0 {
            break;
        }
        let (_, c) = prev_char(text, offset);
        let pred: fn(char) -> bool = if is_word_char(c) {
            is_word_char
        } else if is_punct(c) {
            is_punct
        } else {
            |c: char| c.is_whitespace()
        };
        while offset > 0 {
            let (i, c) = prev_char(text, offset);
            if pred(c) {
                offset = i;
            } else {
                break;
            }
        }
    }
    offset
}

fn prev_char(text: &str, offset: usize) -> (usize, char) {
    let slice = &text[..offset];
    let mut chars = slice.char_indices();
    match chars.next_back() {
        Some((i, c)) => (i, c),
        None => (0, '\0'),
    }
}

/// Find `query` at or after/before `cursor`, wrapping around the document.
pub fn find_match(text: &str, cursor: usize, query: &str, backward: bool) -> Option<usize> {
    if query.is_empty() || text.is_empty() {
        return None;
    }
    let cursor = cursor.min(text.len());
    if backward {
        if let Some(i) = text[..cursor].rfind(query) {
            return Some(i);
        }
        text.rfind(query)
    } else {
        if let Some(i) = text[cursor..].find(query) {
            return Some(cursor + i);
        }
        text.find(query)
    }
}

/// Byte offsets where each line starts.
pub fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (i, c) in text.char_indices() {
        if c == '\n' && i + 1 <= text.len() {
            starts.push(i + 1);
        }
    }
    starts
}

/// All matches a substitute command should replace, as absolute byte ranges
/// with their replacement, in document order.
pub fn substitute_matches(text: &str, spec: &SubSpec) -> Vec<(std::ops::Range<usize>, String)> {
    let starts = line_starts(text);
    let total = starts.len();
    let (first, last) = if spec.whole {
        (1, total)
    } else if let Some(start) = spec.start {
        (start, spec.end.unwrap_or(start).min(total))
    } else {
        // No range: the view passes a cursor-only spec as a single line range.
        (1, total)
    };
    let first = first.max(1).min(total.max(1));
    let last = last.max(first).min(total);
    let mut out = Vec::new();
    if spec.pattern.is_empty() {
        return out;
    }
    for line in first..=last {
        let start = starts[line - 1];
        let end = if line < total {
            starts[line] - 1 // exclude '\n'
        } else {
            text.len()
        };
        let content = &text[start..end];
        let mut from = 0;
        while from <= content.len() {
            let Some(rel) = content[from..].find(&spec.pattern) else {
                break;
            };
            let abs = start + from + rel;
            out.push((abs..abs + spec.pattern.len(), spec.replacement.clone()));
            if !spec.global {
                break;
            }
            from += rel + spec.pattern.len().max(1);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::VimAction::*;
    use super::*;

    fn norm(pending: &mut String, key: &str) -> VimAction {
        translate(VimMode::Normal, pending, key, false)
    }

    fn visual(pending: &mut String, key: &str) -> VimAction {
        translate(VimMode::Visual, pending, key, false)
    }

    fn move_(kind: MoveKind, count: usize) -> VimAction {
        Move { kind, count }
    }

    #[test]
    fn insert_mode_passes_through_except_escape() {
        let mut p = String::new();
        assert_eq!(translate(VimMode::Insert, &mut p, "j", false), PassThrough);
        assert_eq!(translate(VimMode::Insert, &mut p, "x", false), PassThrough);
        assert_eq!(
            translate(VimMode::Insert, &mut p, "escape", false),
            ToNormal
        );
    }

    #[test]
    fn mode_switches() {
        let mut p = String::new();
        assert_eq!(norm(&mut p, "v"), ToVisual);
        assert_eq!(visual(&mut p, "escape"), VisualExit);
        assert_eq!(visual(&mut p, "v"), VisualExit);
        assert_eq!(visual(&mut p, "d"), DeleteSelection);
        assert_eq!(visual(&mut p, "y"), YankSelection);
        assert_eq!(visual(&mut p, "h"), move_(MoveKind::Left, 1));
    }

    #[test]
    fn basic_motions_and_counts() {
        let mut p = String::new();
        assert_eq!(norm(&mut p, "h"), move_(MoveKind::Left, 1));
        assert_eq!(norm(&mut p, "l"), move_(MoveKind::Right, 1));
        assert_eq!(norm(&mut p, "k"), move_(MoveKind::Up, 1));
        assert_eq!(norm(&mut p, "j"), move_(MoveKind::Down, 1));
        assert_eq!(norm(&mut p, "w"), move_(MoveKind::WordForward, 1));
        assert_eq!(norm(&mut p, "b"), move_(MoveKind::WordBackward, 1));
        assert_eq!(norm(&mut p, "0"), move_(MoveKind::LineStart, 1));
        assert_eq!(norm(&mut p, "$"), move_(MoveKind::LineEnd, 1));
        assert_eq!(norm(&mut p, "G"), move_(MoveKind::DocEnd, 1));
        assert!(p.is_empty());

        // Counts.
        assert_eq!(norm(&mut p, "3"), Swallow);
        assert_eq!(norm(&mut p, "j"), move_(MoveKind::Down, 3));
        assert_eq!(norm(&mut p, "1"), Swallow);
        assert_eq!(norm(&mut p, "2"), Swallow);
        assert_eq!(norm(&mut p, "G"), GotoLine(12));
        assert_eq!(norm(&mut p, "2"), Swallow);
        assert_eq!(norm(&mut p, "g"), Pending);
        assert_eq!(norm(&mut p, "g"), GotoLine(2));
    }

    #[test]
    fn g_prefix_dd_yy() {
        let mut p = String::new();
        assert_eq!(norm(&mut p, "g"), Pending);
        assert_eq!(p, "g");
        assert_eq!(norm(&mut p, "g"), move_(MoveKind::DocStart, 1));
        assert!(p.is_empty());

        assert_eq!(norm(&mut p, "d"), Pending);
        assert_eq!(norm(&mut p, "d"), DeleteLines(1));
        assert!(p.is_empty());

        assert_eq!(norm(&mut p, "2"), Swallow);
        assert_eq!(norm(&mut p, "d"), Pending);
        assert_eq!(norm(&mut p, "d"), DeleteLines(2));

        assert_eq!(norm(&mut p, "y"), Pending);
        assert_eq!(norm(&mut p, "y"), YankLines(1));
        assert!(p.is_empty());

        // `d2w` and `2dw` both delete two words.
        assert_eq!(norm(&mut p, "d"), Pending);
        assert_eq!(norm(&mut p, "2"), Swallow);
        assert_eq!(norm(&mut p, "w"), DeleteWord(2));
        assert_eq!(norm(&mut p, "2"), Swallow);
        assert_eq!(norm(&mut p, "d"), Pending);
        assert_eq!(norm(&mut p, "w"), DeleteWord(2));

        assert_eq!(norm(&mut p, "d"), Pending);
        assert_eq!(norm(&mut p, "$"), DeleteToLineEnd(1));
        assert_eq!(norm(&mut p, "3"), Swallow);
        assert_eq!(norm(&mut p, "x"), DeleteChar(3));
    }

    #[test]
    fn insert_entries_and_paste() {
        let mut p = String::new();
        assert_eq!(norm(&mut p, "i"), ToInsert);
        assert_eq!(norm(&mut p, "a"), InsertAfterCursor);
        assert_eq!(norm(&mut p, "I"), InsertAtLineStart);
        assert_eq!(norm(&mut p, "A"), InsertAtLineEnd);
        assert_eq!(norm(&mut p, "o"), OpenLineBelow);
        assert_eq!(norm(&mut p, "O"), OpenLineAbove);
        assert_eq!(
            norm(&mut p, "p"),
            Paste {
                before: false,
                count: 1
            }
        );
        assert_eq!(
            norm(&mut p, "P"),
            Paste {
                before: true,
                count: 1
            }
        );
        assert_eq!(norm(&mut p, "3"), Swallow);
        assert_eq!(
            norm(&mut p, "p"),
            Paste {
                before: false,
                count: 3
            }
        );
        assert_eq!(norm(&mut p, "u"), Undo);
        assert_eq!(translate(VimMode::Normal, &mut p, "r", true), Redo);
    }

    #[test]
    fn page_motions() {
        let mut p = String::new();
        assert_eq!(
            translate(VimMode::Normal, &mut p, "f", true),
            move_(MoveKind::PageDown, 1)
        );
        assert_eq!(
            translate(VimMode::Normal, &mut p, "b", true),
            move_(MoveKind::PageUp, 1)
        );
        assert_eq!(
            translate(VimMode::Normal, &mut p, "d", true),
            move_(MoveKind::HalfPageDown, 1)
        );
        assert_eq!(
            translate(VimMode::Normal, &mut p, "u", true),
            move_(MoveKind::HalfPageUp, 1)
        );
    }

    #[test]
    fn command_line_write_and_quit() {
        let mut p = String::new();
        assert_eq!(norm(&mut p, ":"), Pending);
        assert_eq!(p, ":");
        assert_eq!(norm(&mut p, "w"), Pending);
        assert_eq!(p, ":w");
        assert_eq!(norm(&mut p, "enter"), Save);
        assert!(p.is_empty());

        assert_eq!(norm(&mut p, ":"), Pending);
        assert_eq!(norm(&mut p, "q"), Pending);
        assert_eq!(norm(&mut p, "enter"), FinishEdit);

        assert_eq!(norm(&mut p, ":"), Pending);
        for k in ["w", "q"] {
            assert_eq!(norm(&mut p, k), Pending);
        }
        assert_eq!(norm(&mut p, "enter"), FinishEdit);
        assert_eq!(norm(&mut p, "Z"), Pending);
        assert_eq!(norm(&mut p, "Z"), FinishEdit);

        // Esc abandons the command.
        assert_eq!(norm(&mut p, ":"), Pending);
        assert_eq!(norm(&mut p, "escape"), Swallow);
        assert!(p.is_empty());
    }

    #[test]
    fn search_and_repeat() {
        let mut p = String::new();
        assert_eq!(norm(&mut p, "/"), Pending);
        for k in ["f", "o", "o"] {
            assert_eq!(norm(&mut p, k), Pending);
        }
        assert_eq!(p, "/foo");
        assert_eq!(
            norm(&mut p, "enter"),
            Search {
                query: "foo".into(),
                backward: false
            }
        );
        assert_eq!(norm(&mut p, "?"), Pending);
        assert_eq!(norm(&mut p, "b"), Pending);
        assert_eq!(
            norm(&mut p, "enter"),
            Search {
                query: "b".into(),
                backward: true
            }
        );
        assert_eq!(norm(&mut p, "n"), SearchNext { backward: false });
        assert_eq!(norm(&mut p, "N"), SearchNext { backward: true });
        assert_eq!(norm(&mut p, "."), Repeat);
    }

    #[test]
    fn substitute_parsing() {
        let spec = parse_substitute("s/old/new/g").unwrap();
        assert_eq!(spec.start, None);
        assert_eq!(spec.end, None);
        assert!(!spec.whole);
        assert_eq!(spec.pattern, "old");
        assert_eq!(spec.replacement, "new");
        assert!(spec.global);

        let spec = parse_substitute("%s/a/b/").unwrap();
        assert!(spec.whole);
        assert!(!spec.global);

        let spec = parse_substitute("2,5s/a/b/g").unwrap();
        assert_eq!((spec.start, spec.end), (Some(2), Some(5)));
        assert!(spec.global);

        let spec = parse_substitute("7s/x/y/").unwrap();
        assert_eq!((spec.start, spec.end), (Some(7), Some(7)));

        let spec = parse_substitute("s|a/b|c|g").unwrap();
        assert_eq!(spec.pattern, "a/b");
        assert_eq!(spec.replacement, "c");

        assert!(parse_substitute("s/a").is_err());
        assert!(parse_substitute("s/a/b/z").is_err());
        assert!(parse_substitute("0,2s/a/b/").is_err());
        assert!(parse_substitute("s//b/").is_err());

        let mut p = String::new();
        for k in [":", "%", "s", "/", "a", "/", "b", "/", "g"] {
            assert_eq!(norm(&mut p, k), Pending);
        }
        assert_eq!(
            norm(&mut p, "enter"),
            Substitute(SubSpec {
                start: None,
                end: None,
                whole: true,
                pattern: "a".into(),
                replacement: "b".into(),
                global: true,
            })
        );
        // Unknown command reports an error instead of vanishing.
        let mut p = String::new();
        for k in [":", "z", "z"] {
            let _ = norm(&mut p, k);
        }
        assert!(matches!(norm(&mut p, "enter"), Error(_)));
    }

    #[test]
    fn line_helpers() {
        let text = "one\ntwo\nthree";
        assert_eq!(line_start_offset(text, 0), 0);
        assert_eq!(line_start_offset(text, 5), 4);
        assert_eq!(line_end_offset(text, 0), 3);
        assert_eq!(line_end_offset(text, 5), 7);
        assert_eq!(line_end_offset(text, text.len()), text.len());

        // dd on the first line removes "one\n".
        assert_eq!(delete_line_range(text, 0, 1), (0, 4));
        // dd on the last line (no trailing newline) also removes the newline.
        assert_eq!(delete_line_range(text, 9, 1), (7, 13));
        assert_eq!(delete_line_range(text, 9, 5), (7, 13));
        assert_eq!(yank_line_text(text, 4, 1), "two");
        assert_eq!(yank_line_text(text, 0, 2), "one\ntwo");
        assert_eq!(line_starts(text), vec![0, 4, 8]);
    }

    #[test]
    fn word_offsets() {
        let text = "foo bar  baz";
        assert_eq!(word_forward_offset(text, 0, 1), 4);
        assert_eq!(word_forward_offset(text, 0, 2), 9);
        assert_eq!(word_forward_offset(text, 0, 9), text.len());
        assert_eq!(word_backward_offset(text, text.len(), 1), 9);
        assert_eq!(word_backward_offset(text, text.len(), 2), 4);
        assert_eq!(word_backward_offset(text, 4, 1), 0);
        assert_eq!(word_forward_offset("a.b c", 0, 1), 1);
        assert_eq!(word_forward_offset("a.b c", 0, 2), 2);
        assert_eq!(word_forward_offset("a.b c", 0, 3), 4);
        assert_eq!(word_backward_offset("a.b c", 4, 1), 2);
        // Newlines count as whitespace for word motion.
        assert_eq!(word_forward_offset("foo\nbar", 0, 1), 4);
    }

    #[test]
    fn find_match_wraps() {
        let text = "foo bar foo";
        assert_eq!(find_match(text, 0, "foo", false), Some(0));
        assert_eq!(find_match(text, 1, "foo", false), Some(8));
        assert_eq!(find_match(text, 8, "foo", false), Some(8));
        assert_eq!(find_match(text, 9, "foo", false), Some(0));
        assert_eq!(find_match(text, 8, "foo", true), Some(0));
        assert_eq!(find_match(text, 0, "foo", true), Some(8));
        assert_eq!(find_match(text, 0, "zzz", false), None);
    }

    #[test]
    fn substitute_matches_ranges() {
        let text = "a a\nba\nccc";
        let spec = SubSpec {
            start: None,
            end: None,
            whole: true,
            pattern: "a".into(),
            replacement: "x".into(),
            global: false,
        };
        let hits = substitute_matches(text, &spec);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].0, 0..1);
        assert_eq!(hits[1].0, 5..6);

        let global = SubSpec {
            global: true,
            ..spec.clone()
        };
        assert_eq!(substitute_matches(text, &global).len(), 3);

        let ranged = SubSpec {
            start: Some(2),
            end: Some(2),
            whole: false,
            global: true,
            ..spec
        };
        let hits = substitute_matches(text, &ranged);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, 5..6);
    }
}
