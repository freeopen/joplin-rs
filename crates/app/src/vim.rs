//! Minimal Vim-style modal editing for the note body editor.
//!
//! Only the *pure* key translation and text-offset arithmetic live here:
//! [`translate`] maps one key (plus a multi-key `pending` buffer such as `g`,
//! `d`, a count, `:` or `/`) to a [`VimAction`]. The view layer owns the
//! textarea and performs the action (dispatching gpui movement actions,
//! editing the rope, saving, ...).
//!
//! Supported: Normal/Insert/Visual/Visual Line/Visual Block modes, motions
//! `h j k l w b 0 $ gg G`,
//! `Ctrl-f/b/d/u`, counts (`3j`, `12G`, `2dd`, `3x`, `d2w`), operators `x dd
//! yy dw d$`, `p P`, `u`/`Ctrl-r`, `i a I A o O`, `.`, `/word ?word n N`,
//! `:w :q :wq :x`, `ZZ` and `:%s/old/new/g` / `:n1,n2s/old/new/g` /
//! `:s/old/new[/g]`. Visual modes support `d y c > < ~ o`, `gv`, text objects
//! `iw aw ip ap`, `:'<,'>s`, `:'<,'>sort[!]`, and block `I A c O` (display-width
//! columns, CJK = 2). Macros, named registers, marks, `f/t`, `J`, and `>>/<<`
//! are out of scope.

use unicode_width::UnicodeWidthChar;

/// Editor mode. Insert passes keys straight through; Normal and Visual
/// consume them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VimMode {
    Normal,
    Insert,
    Visual,
    VisualLine,
    VisualBlock,
}

impl VimMode {
    /// Short badge label for the title bar.
    pub fn label(self) -> &'static str {
        match self {
            VimMode::Normal => "NORMAL",
            VimMode::Insert => "INSERT",
            VimMode::Visual => "VISUAL",
            VimMode::VisualLine => "V-LINE",
            VimMode::VisualBlock => "V-BLOCK",
        }
    }
}

impl VimMode {
    /// True for `v`, `V` and `Ctrl-v`.
    pub fn is_visual(self) -> bool {
        matches!(
            self,
            VimMode::Visual | VimMode::VisualLine | VimMode::VisualBlock
        )
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

/// Visual-mode text objects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextObjectKind {
    Word,
    Paragraph,
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
    /// `:'<,'>s` prefix: the range comes from the active visual selection.
    pub selection_range: bool,
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
    /// Start linewise Visual mode (`V`).
    ToVisualLine,
    /// Start blockwise Visual mode (`Ctrl-v`).
    ToVisualBlock,
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
    /// Visual-mode `d` (char, line or block).
    DeleteSelection,
    /// Visual-mode `y` (char, line or block).
    YankSelection,
    /// Visual-mode `c`: delete the selection and enter Insert.
    ChangeSelection,
    /// Visual-mode `>` / `<`.
    IndentSelection {
        increase: bool,
        count: usize,
    },
    /// Visual-mode `~`.
    ToggleCaseSelection,
    /// Visual-mode `o` (char/line) or block corner swap.
    ToggleVisualEnd,
    /// Blockwise `O`: other corner on the same line.
    ToggleBlockCorner,
    /// Blockwise `I` / `A`: multi-cursor insert at each selected line.
    BlockInsert {
        append: bool,
    },
    /// Blockwise `c`: delete the block, then multi-cursor insert.
    BlockChange,
    /// Visual text object (`iw`, `ip`, `aw`, `ap`).
    SelectTextObject {
        kind: TextObjectKind,
        around: bool,
    },
    /// `gv`: restore the last visual selection.
    ReselectVisual,
    /// `:'<,'>sort` / `:'<,'>sort!`.
    SortSelection {
        descending: bool,
    },
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
            "v" | "q" => match mode {
                VimMode::VisualBlock => VisualExit,
                _ => ToVisualBlock,
            },
            _ => Swallow,
        };
    }

    if key == "escape" {
        pending.clear();
        return if mode.is_visual() {
            VisualExit
        } else {
            Swallow
        };
    }

    let Some(ch) = single_char(key) else {
        pending.clear();
        return Swallow;
    };

    // Visual text objects: `i`/`a` followed by `w` or `p`.
    if mode.is_visual() {
        if pending == "i" || pending == "a" {
            let around = pending == "a";
            pending.clear();
            return match ch {
                'w' => SelectTextObject {
                    kind: TextObjectKind::Word,
                    around,
                },
                'p' => SelectTextObject {
                    kind: TextObjectKind::Paragraph,
                    around,
                },
                _ => Swallow,
            };
        }
        if ch == 'i' || ch == 'a' {
            pending.clear();
            pending.push(ch);
            return Pending;
        }
    }

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
        } else if ch == 'v' {
            ReselectVisual
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
        'V' => {
            pending.clear();
            if mode == VimMode::VisualLine {
                VisualExit
            } else {
                ToVisualLine
            }
        }
        '>' | '<' | '~' => {
            let count = pending_count(pending);
            pending.clear();
            if ch == '~' {
                ToggleCaseSelection
            } else {
                IndentSelection {
                    increase: ch == '>',
                    count,
                }
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
            if mode.is_visual() {
                pending.push_str(":'<,'>");
            } else {
                pending.push(':');
            }
            Pending
        }
        _ => {
            pending.clear();
            Swallow
        }
    };
    // Visual modes edit the selection instead of a motion target.
    if !mode.is_visual() {
        return action;
    }
    let block = mode == VimMode::VisualBlock;
    match ch {
        'd' => {
            pending.clear();
            VimAction::DeleteSelection
        }
        'y' => {
            pending.clear();
            VimAction::YankSelection
        }
        'c' => {
            pending.clear();
            if block {
                VimAction::BlockChange
            } else {
                VimAction::ChangeSelection
            }
        }
        'o' => {
            pending.clear();
            VimAction::ToggleVisualEnd
        }
        'O' => {
            pending.clear();
            if block {
                VimAction::ToggleBlockCorner
            } else {
                VimAction::Swallow
            }
        }
        'I' => {
            pending.clear();
            if block {
                VimAction::BlockInsert { append: false }
            } else {
                VimAction::Swallow
            }
        }
        'A' => {
            pending.clear();
            if block {
                VimAction::BlockInsert { append: true }
            } else {
                VimAction::Swallow
            }
        }
        'p' | 'P' | 'x' => VimAction::Swallow,
        _ => action,
    }
}

/// Parse a complete `:...` command line.
pub fn parse_command(cmd: &str) -> VimAction {
    let body = cmd.trim_start_matches(':').trim();
    let (selection_range, body) = match body.strip_prefix("'<,'>") {
        Some(rest) => (true, rest.trim()),
        None => (false, body),
    };
    match body {
        "" => return VimAction::Swallow,
        "w" | "w!" => return VimAction::Save,
        "q" | "wq" | "wq!" | "x" => return VimAction::FinishEdit,
        "sort" => return VimAction::SortSelection { descending: false },
        "sort!" => return VimAction::SortSelection { descending: true },
        _ => {}
    }
    if body.starts_with('s') || body.contains('s') {
        match parse_substitute(body) {
            Ok(mut spec) => {
                spec.selection_range = selection_range;
                VimAction::Substitute(spec)
            }
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
        selection_range: false,
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

// ---- Visual Block (display columns) -------------------------------------

/// Tabs occupy four display cells, matching the configured Vim tabstop.
pub const TAB_WIDTH: usize = 4;

/// One character's width in display cells. CJK characters count as 2.
pub fn char_display_width(c: char) -> usize {
    if c == '\t' {
        TAB_WIDTH
    } else {
        UnicodeWidthChar::width(c).unwrap_or(0)
    }
}

fn str_display_width(text: &str) -> usize {
    text.chars().map(char_display_width).sum()
}

/// `(line index, display column)` of a byte offset.
pub fn offset_to_line_display_col(text: &str, offset: usize) -> (usize, usize) {
    let offset = text
        .get(..offset.min(text.len()))
        .map(|s| s.len())
        .unwrap_or(0);
    let line = text[..offset].bytes().filter(|&b| b == b'\n').count();
    let start = line_start_offset(text, offset);
    (line, str_display_width(&text[start..offset]))
}

/// First byte offset on `line` whose display column is at least `target`.
///
/// A target that lands inside a wide character snaps to that character's
/// start, so selection and deletion never split a CJK glyph.
pub fn offset_for_display_col(text: &str, line: usize, target: usize) -> usize {
    let starts = line_starts(text);
    let Some(&start) = starts.get(line) else {
        return text.len();
    };
    let end = line_end_offset(text, start);
    start + str_offset_for_display_col(&text[start..end], target)
}

/// First byte offset in `text` whose display column is at least `target`.
fn str_offset_for_display_col(text: &str, target: usize) -> usize {
    let mut col = 0;
    for (i, c) in text.char_indices() {
        if col >= target {
            return i;
        }
        let width = char_display_width(c);
        if width > 0 && col + width > target {
            return i;
        }
        col += width;
    }
    text.len()
}

/// Inclusive display-column rectangle between two cursor offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockSelection {
    pub top_line: usize,
    pub left_col: usize,
    pub bottom_line: usize,
    /// Display column where the last selected cell starts.
    pub right_col: usize,
}

/// Rectangle covered by a blockwise selection from `anchor` to `cursor`.
pub fn block_selection(text: &str, anchor: usize, cursor: usize) -> BlockSelection {
    let (a_line, a_col) = offset_to_line_display_col(text, anchor);
    let (c_line, c_col) = offset_to_line_display_col(text, cursor);
    BlockSelection {
        top_line: a_line.min(c_line),
        left_col: a_col.min(c_col),
        bottom_line: a_line.max(c_line),
        right_col: a_col.max(c_col),
    }
}

/// Per-line byte ranges covered by the block.
///
/// Characters are included when their display cells overlap
/// `[left_col, right_col]`; a wide character that starts before `left_col`
/// but extends into the block is selected whole.
pub fn block_ranges(text: &str, anchor: usize, cursor: usize) -> Vec<std::ops::Range<usize>> {
    let selection = block_selection(text, anchor, cursor);
    let starts = line_starts(text);
    let mut ranges = Vec::new();
    for line in selection.top_line..=selection.bottom_line {
        let Some(&start) = starts.get(line) else {
            break;
        };
        let end = line_end_offset(text, start);
        let mut col = 0;
        let mut run: Option<(usize, usize)> = None;
        for (i, c) in text[start..end].char_indices() {
            let width = char_display_width(c);
            let overlaps = col + width > selection.left_col && col <= selection.right_col;
            let at = start + i;
            if overlaps {
                match &mut run {
                    Some((_, run_end)) if *run_end == at => *run_end = at + c.len_utf8(),
                    Some((run_start, run_end)) => {
                        ranges.push(*run_start..*run_end);
                        run = Some((at, at + c.len_utf8()));
                    }
                    None => run = Some((at, at + c.len_utf8())),
                }
            } else if let Some((run_start, run_end)) = run.take() {
                ranges.push(run_start..run_end);
            }
            col += width;
        }
        if let Some((run_start, run_end)) = run {
            ranges.push(run_start..run_end);
        }
    }
    ranges
}

/// Text of a blockwise selection, one line per selected source line.
pub fn block_text(text: &str, anchor: usize, cursor: usize) -> String {
    let selection = block_selection(text, anchor, cursor);
    let starts = line_starts(text);
    let ranges = block_ranges(text, anchor, cursor);
    let mut lines = Vec::new();
    for line in selection.top_line..=selection.bottom_line {
        let Some(&start) = starts.get(line) else {
            break;
        };
        let end = line_end_offset(text, start);
        let line_text = ranges
            .iter()
            .filter(|range| range.start >= start && range.end <= end)
            .filter_map(|range| text.get(range.clone()))
            .collect::<String>();
        lines.push(line_text);
    }
    lines.join("\n")
}

/// Remove the block and return `(new text, cursor at the block's top-left)`.
pub fn block_delete(text: &str, anchor: usize, cursor: usize) -> (String, usize) {
    let selection = block_selection(text, anchor, cursor);
    let top_left = offset_for_display_col(text, selection.top_line, selection.left_col);
    let mut ranges = block_ranges(text, anchor, cursor);
    ranges.sort_by_key(|range| range.start);
    let mut out = text.to_string();
    for range in ranges.into_iter().rev() {
        if range.end <= out.len() {
            out.replace_range(range, "");
        }
    }
    let cursor = top_left.min(out.len());
    (out, cursor)
}

/// Insert a rectangular block at `cursor`, repeating each line `count` times.
///
/// Returns `(new text, cursor at the start of the inserted block)`.
pub fn block_insert(text: &str, cursor: usize, lines: &[String], count: usize) -> (String, usize) {
    if lines.is_empty() {
        return (text.to_string(), cursor);
    }
    let count = count.max(1);
    let (line, col) = offset_to_line_display_col(text, cursor);
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut parts: Vec<String> = text
        .split('\n')
        .map(|part| part.strip_suffix('\r').unwrap_or(part).to_string())
        .collect();
    while parts.len() < line + lines.len() {
        parts.push(String::new());
    }
    for (i, block_line) in lines.iter().enumerate() {
        let part = &mut parts[line + i];
        let width = str_display_width(part);
        let at = if col > width {
            part.push_str(&" ".repeat(col - width));
            part.len()
        } else {
            str_offset_for_display_col(part, col)
        };
        part.insert_str(at, &block_line.repeat(count));
    }
    let out = parts.join(newline);
    // `split('\n')` keeps the final empty part for a trailing newline; the
    // join already reproduces it.
    let mut cursor_at = 0;
    for part in parts.iter().take(line) {
        cursor_at += part.len() + newline.len();
    }
    cursor_at += str_offset_for_display_col(&parts[line], col);
    cursor_at = cursor_at.min(out.len());
    (out, cursor_at)
}

/// Move the cursor inside a blockwise selection. `h/l` stay on the current
/// line; `j/k` move by logical line while keeping the display column.
pub fn block_move(text: &str, cursor: usize, kind: MoveKind, count: usize) -> usize {
    let count = count.max(1);
    let (line, col) = offset_to_line_display_col(text, cursor);
    let starts = line_starts(text);
    match kind {
        MoveKind::Left => {
            let start = line_start_offset(text, cursor);
            let mut at = cursor;
            for _ in 0..count {
                if at <= start {
                    break;
                }
                at = prev_char(text, at).0;
            }
            at
        }
        MoveKind::Right => {
            let end = line_end_offset(text, cursor);
            let mut at = cursor;
            for _ in 0..count {
                let Some(c) = text[at..end].chars().next() else {
                    break;
                };
                at += c.len_utf8();
            }
            at
        }
        MoveKind::Up => {
            let target = line.saturating_sub(count);
            offset_for_display_col(text, target, col)
        }
        MoveKind::Down => {
            let target = (line + count).min(starts.len().saturating_sub(1));
            offset_for_display_col(text, target, col)
        }
        _ => cursor,
    }
}

// ---- Visual selection ranges --------------------------------------------

/// Indentation unit used by visual `>` / `<`.
pub const INDENT: &str = "    ";

fn next_char_boundary(text: &str, offset: usize) -> usize {
    if offset >= text.len() {
        return text.len();
    }
    let Some(c) = text[offset..].chars().next() else {
        return text.len();
    };
    offset + c.len_utf8()
}

/// Inclusive charwise operation range (Vim includes the cursor character).
pub fn charwise_range(text: &str, anchor: usize, cursor: usize) -> std::ops::Range<usize> {
    let start = anchor.min(cursor).min(text.len());
    let end = next_char_boundary(text, anchor.max(cursor).min(text.len()));
    start..end
}

/// Per-line highlight ranges for a charwise selection.
pub fn charwise_ranges(text: &str, anchor: usize, cursor: usize) -> Vec<std::ops::Range<usize>> {
    let range = charwise_range(text, anchor, cursor);
    let (first, _) = offset_to_line_display_col(text, range.start);
    let (last, _) = offset_to_line_display_col(text, range.end.saturating_sub(1));
    let starts = line_starts(text);
    let mut out = Vec::new();
    for line in first..=last {
        let Some(&line_start) = starts.get(line) else {
            break;
        };
        let line_end = line_end_offset(text, line_start);
        let start = range.start.max(line_start);
        let end = range.end.min(line_end);
        if start < end {
            out.push(start..end);
        }
    }
    out
}

/// Whole-line operation range, including the final newline when present.
pub fn linewise_range(text: &str, anchor: usize, cursor: usize) -> std::ops::Range<usize> {
    let (first, _) = offset_to_line_display_col(text, anchor);
    let (last, _) = offset_to_line_display_col(text, cursor);
    let starts = line_starts(text);
    let start = starts.get(first.min(last)).copied().unwrap_or(0);
    let last_line_start = starts.get(first.max(last)).copied().unwrap_or(text.len());
    let last_end = line_end_offset(text, last_line_start);
    let end = if last_end < text.len() {
        last_end + 1
    } else {
        text.len()
    };
    start..end
}

/// Per-line highlight ranges for a linewise selection.
pub fn linewise_ranges(text: &str, anchor: usize, cursor: usize) -> Vec<std::ops::Range<usize>> {
    let (first, _) = offset_to_line_display_col(text, anchor);
    let (last, _) = offset_to_line_display_col(text, cursor);
    let starts = line_starts(text);
    let mut out = Vec::new();
    for line in first.min(last)..=first.max(last) {
        let Some(&start) = starts.get(line) else {
            break;
        };
        let end = line_end_offset(text, start);
        if start < end {
            out.push(start..end);
        } else {
            // Keep empty lines in the selection; the overlay paints a small
            // marker for a zero-width range.
            out.push(start..start);
        }
    }
    out
}

/// Highlight ranges for the active visual mode.
pub fn visual_ranges(
    mode: VimMode,
    text: &str,
    anchor: usize,
    cursor: usize,
) -> Vec<std::ops::Range<usize>> {
    match mode {
        VimMode::Visual => charwise_ranges(text, anchor, cursor),
        VimMode::VisualLine => linewise_ranges(text, anchor, cursor),
        VimMode::VisualBlock => block_ranges(text, anchor, cursor),
        _ => Vec::new(),
    }
}

// ---- Text objects --------------------------------------------------------

fn char_class(c: char) -> u8 {
    if c.is_alphanumeric() || c == '_' {
        0
    } else if c.is_whitespace() {
        1
    } else {
        2
    }
}

/// `iw`: word/punctuation/whitespace run under the cursor, within one line.
pub fn inner_word_range(text: &str, cursor: usize) -> std::ops::Range<usize> {
    let line_start = line_start_offset(text, cursor);
    let line_end = line_end_offset(text, cursor);
    let mut at = cursor.min(line_end);
    if at >= line_end && at > line_start {
        at = prev_char(text, at).0;
    }
    let Some(ch) = text[at..line_end].chars().next() else {
        return at..at;
    };
    let class = char_class(ch);
    let mut start = at;
    while start > line_start {
        let (previous, c) = prev_char(text, start);
        if char_class(c) == class {
            start = previous;
        } else {
            break;
        }
    }
    let mut end = at + ch.len_utf8();
    while end < line_end {
        let Some(c) = text[end..line_end].chars().next() else {
            break;
        };
        if char_class(c) == class {
            end += c.len_utf8();
        } else {
            break;
        }
    }
    start..end
}

/// `aw`: inner word plus trailing whitespace, falling back to leading.
pub fn word_object_range(text: &str, cursor: usize, around: bool) -> std::ops::Range<usize> {
    let inner = inner_word_range(text, cursor);
    if !around || inner.is_empty() {
        return inner;
    }
    let line_start = line_start_offset(text, inner.start);
    let line_end = line_end_offset(text, inner.start);
    let mut end = inner.end;
    while end < line_end {
        let Some(c) = text[end..line_end].chars().next() else {
            break;
        };
        if c.is_whitespace() {
            end += c.len_utf8();
        } else {
            break;
        }
    }
    if end > inner.end {
        return inner.start..end;
    }
    let mut start = inner.start;
    while start > line_start {
        let (previous, c) = prev_char(text, start);
        if c.is_whitespace() {
            start = previous;
        } else {
            break;
        }
    }
    start..inner.end
}

fn line_is_blank(text: &str, line: usize) -> bool {
    let starts = line_starts(text);
    let Some(&start) = starts.get(line) else {
        return true;
    };
    text[start..line_end_offset(text, start)].trim().is_empty()
}

/// `ip`: contiguous non-blank paragraph around the cursor, or the contiguous
/// blank-line block when the cursor sits on a blank line.
pub fn paragraph_range(text: &str, cursor: usize) -> std::ops::Range<usize> {
    let (line, _) = offset_to_line_display_col(text, cursor);
    let starts = line_starts(text);
    let total = starts.len();
    if total == 0 {
        return 0..0;
    }
    let blank = line_is_blank(text, line);
    let mut first = line;
    while first > 0 && line_is_blank(text, first - 1) == blank {
        first -= 1;
    }
    let mut last = line;
    while last + 1 < total && line_is_blank(text, last + 1) == blank {
        last += 1;
    }
    let start = starts[first];
    let end = line_end_offset(text, starts[last]);
    start..end
}

/// `ap`: paragraph plus one following blank line (or one preceding when no
/// following blank line exists).
pub fn paragraph_object_range(text: &str, cursor: usize, around: bool) -> std::ops::Range<usize> {
    let inner = paragraph_range(text, cursor);
    if !around || inner.is_empty() {
        return inner;
    }
    let starts = line_starts(text);
    let (last, _) = offset_to_line_display_col(text, inner.end.saturating_sub(1));
    if last + 1 < starts.len() && line_is_blank(text, last + 1) {
        let end = line_end_offset(text, starts[last + 1]);
        return inner.start..end;
    }
    let (first, _) = offset_to_line_display_col(text, inner.start);
    if first > 0 && line_is_blank(text, first - 1) {
        return starts[first - 1]..inner.end;
    }
    inner
}

// ---- Visual operators ----------------------------------------------------

fn split_ending(line: &str) -> (&str, &str) {
    if let Some(content) = line.strip_suffix("\r\n") {
        (content, "\r\n")
    } else if let Some(content) = line.strip_suffix('\n') {
        (content, "\n")
    } else {
        (line, "")
    }
}

/// Add or remove `levels` indentation units on lines `first..=last`.
pub fn indent_lines(
    text: &str,
    first: usize,
    last: usize,
    increase: bool,
    levels: usize,
) -> String {
    let levels = levels.max(1);
    let mut out = String::with_capacity(text.len() + levels * first.max(last) * 4);
    for (line, part) in text.split_inclusive('\n').enumerate() {
        if line < first || line > last {
            out.push_str(part);
            continue;
        }
        let (content, ending) = split_ending(part);
        if increase {
            for _ in 0..levels {
                out.push_str(INDENT);
            }
            out.push_str(content);
        } else {
            let mut rest = content;
            for _ in 0..levels {
                if let Some(stripped) = rest.strip_prefix(INDENT) {
                    rest = stripped;
                } else if let Some(stripped) = rest.strip_prefix('\t') {
                    rest = stripped;
                } else {
                    let spaces = rest.chars().take_while(|c| *c == ' ').count().min(4);
                    rest = &rest[spaces..];
                }
            }
            out.push_str(rest);
        }
        out.push_str(ending);
    }
    out
}

fn toggle_case_str(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_lowercase() {
            out.extend(c.to_uppercase());
        } else if c.is_uppercase() {
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// Toggle case in each range, back-to-front so offsets stay valid.
pub fn toggle_case_ranges(text: &str, ranges: &[std::ops::Range<usize>]) -> String {
    let mut out = text.to_string();
    for range in ranges.iter().rev() {
        if range.start >= range.end || range.end > out.len() {
            continue;
        }
        let replacement = toggle_case_str(&out[range.clone()]);
        out.replace_range(range.clone(), &replacement);
    }
    out
}

/// Stable sort of lines `first..=last`, preserving the document's newlines.
pub fn sort_line_range(text: &str, first: usize, last: usize, descending: bool) -> String {
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut parts: Vec<String> = text
        .split('\n')
        .map(|part| part.strip_suffix('\r').unwrap_or(part).to_string())
        .collect();
    let last = last.min(parts.len().saturating_sub(1));
    if first > last {
        return text.to_string();
    }
    let mut slice: Vec<String> = parts[first..=last].to_vec();
    slice.sort();
    if descending {
        slice.reverse();
    }
    parts.splice(first..=last, slice);
    parts.join(newline)
}

/// Per-line insertion offsets for blockwise `I` (append=false) / `A`.
pub fn block_insert_offsets(text: &str, anchor: usize, cursor: usize, append: bool) -> Vec<usize> {
    let selection = block_selection(text, anchor, cursor);
    let starts = line_starts(text);
    let ranges = block_ranges(text, anchor, cursor);
    let mut out = Vec::new();
    for line in selection.top_line..=selection.bottom_line {
        let Some(&start) = starts.get(line) else {
            break;
        };
        let end = line_end_offset(text, start);
        let offset = if append {
            ranges
                .iter()
                .filter(|range| range.start >= start && range.end <= end)
                .map(|range| range.end)
                .max()
                .unwrap_or(end)
        } else {
            offset_for_display_col(text, line, selection.left_col)
        };
        out.push(offset);
    }
    out
}

/// Apply blockwise `c`: remove the block and return the new text, the cursor
/// at the block's top-left, and one insertion offset per affected line.
pub fn block_change(text: &str, anchor: usize, cursor: usize) -> (String, usize, Vec<usize>) {
    let selection = block_selection(text, anchor, cursor);
    let (new_text, new_cursor) = block_delete(text, anchor, cursor);
    let offsets = (selection.top_line..=selection.bottom_line)
        .map(|line| offset_for_display_col(&new_text, line, selection.left_col))
        .collect();
    (new_text, new_cursor, offsets)
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
                selection_range: false,
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
            selection_range: false,
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

    #[test]
    fn ctrl_v_enters_and_leaves_block_mode() {
        let mut p = String::new();
        assert_eq!(translate(VimMode::Normal, &mut p, "v", true), ToVisualBlock);
        assert_eq!(translate(VimMode::Visual, &mut p, "v", true), ToVisualBlock);
        assert_eq!(
            translate(VimMode::VisualBlock, &mut p, "v", true),
            VisualExit
        );
        assert_eq!(
            translate(VimMode::VisualBlock, &mut p, "v", false),
            ToVisual
        );
        assert_eq!(
            translate(VimMode::VisualBlock, &mut p, "escape", false),
            VisualExit
        );
        assert_eq!(
            translate(VimMode::VisualBlock, &mut p, "h", false),
            move_(MoveKind::Left, 1)
        );
        assert_eq!(
            translate(VimMode::VisualBlock, &mut p, "d", false),
            DeleteSelection
        );
        assert_eq!(
            translate(VimMode::VisualBlock, &mut p, "y", false),
            YankSelection
        );
        assert_eq!(translate(VimMode::VisualBlock, &mut p, "i", false), Pending);
    }

    #[test]
    fn display_width_counts_cjk_as_two_cells() {
        assert_eq!(char_display_width('a'), 1);
        assert_eq!(char_display_width('中'), 2);
        assert_eq!(char_display_width('\t'), TAB_WIDTH);
        let text = "a中b";
        assert_eq!(offset_to_line_display_col(text, 0), (0, 0));
        assert_eq!(offset_to_line_display_col(text, 1), (0, 1));
        assert_eq!(offset_to_line_display_col(text, 4), (0, 3));
        // Inside a wide character snaps to its start.
        assert_eq!(offset_for_display_col(text, 0, 2), 1);
        assert_eq!(offset_for_display_col(text, 0, 3), 4);
    }

    #[test]
    fn block_ranges_respect_display_columns() {
        let text = "abcdef\n中文xy\nab\n";
        assert_eq!(
            block_selection(text, 0, 10),
            BlockSelection {
                top_line: 0,
                left_col: 0,
                bottom_line: 1,
                right_col: 2,
            }
        );
        assert_eq!(block_ranges(text, 0, 10), vec![0..3, 7..13]);
        assert_eq!(block_text(text, 0, 10), "abc\n中文");

        // Extending to the third line keeps the two-line block's columns.
        let cursor = offset_for_display_col(text, 2, 2);
        assert_eq!(block_ranges(text, 0, cursor), vec![0..3, 7..13, 16..18]);
        assert_eq!(block_text(text, 0, cursor), "abc\n中文\nab");
    }

    #[test]
    fn block_delete_removes_rectangle() {
        let text = "abcdef\n中文xy\nab\n";
        let (out, cursor) = block_delete(text, 0, 10);
        assert_eq!(out, "def\nxy\nab\n");
        assert_eq!(cursor, 0);
    }

    #[test]
    fn block_insert_pastes_by_display_column() {
        let lines = vec!["X".to_string(), "Y".to_string()];
        let (out, cursor) = block_insert("abcdef\n中文xy\nab\n", 0, &lines, 1);
        assert_eq!(out, "Xabcdef\nY中文xy\nab\n");
        assert_eq!(cursor, 0);

        let lines = vec!["中".to_string(), "文".to_string()];
        let (out, cursor) = block_insert("a\nb", 2, &lines, 1);
        assert_eq!(out, "a\n中b\n文");
        assert_eq!(cursor, 2);
    }

    #[test]
    fn block_move_stays_in_line_and_keeps_column() {
        let text = "abc\n中文xy\nz";
        assert_eq!(block_move(text, 0, MoveKind::Right, 2), 2);
        assert_eq!(block_move(text, 0, MoveKind::Left, 1), 0);
        assert_eq!(block_move(text, 0, MoveKind::Down, 1), 4);
        assert_eq!(block_move(text, 4, MoveKind::Right, 1), 7);
        assert_eq!(block_move(text, 4, MoveKind::Left, 1), 4);
        assert_eq!(block_move(text, 4, MoveKind::Down, 1), 13);
        assert_eq!(block_move(text, 14, MoveKind::Down, 1), 14);
    }

    #[test]
    fn block_selection_is_symmetric_and_keeps_empty_lines() {
        let text = "abcdef\nx\n中文\n";
        let cursor = offset_for_display_col(text, 2, 2);
        let anchor = 0;
        assert_eq!(block_text(text, cursor, anchor), "abc\nx\n中文");
        assert_eq!(block_delete(text, cursor, anchor).0, "def\n\n\n");

        // A line shorter than the rectangle contributes an empty row.
        let short = "abc\n\nxyz\n";
        let cursor = offset_for_display_col(short, 2, 1);
        assert_eq!(block_text(short, 0, cursor), "ab\n\nxy");
    }

    #[test]
    fn charwise_range_includes_cursor_and_supports_reverse() {
        let text = "one two\nthree\n";
        assert_eq!(charwise_range(text, 0, 0), 0..1);
        assert_eq!(charwise_range(text, 0, 2), 0..3);
        assert_eq!(charwise_range(text, 6, 2), 2..7);
        assert_eq!(charwise_ranges(text, 0, 11), vec![0..7, 8..12]);
        assert_eq!(charwise_ranges(text, 11, 0), vec![0..7, 8..12]);
    }

    #[test]
    fn linewise_range_covers_whole_lines() {
        let text = "one two\nthree\nfour";
        assert_eq!(linewise_range(text, 2, 9), 0..14);
        assert_eq!(linewise_ranges(text, 2, 9), vec![0..7, 8..13]);
        assert_eq!(linewise_range(text, 9, 2), 0..14);
    }

    #[test]
    fn indent_and_case_helpers() {
        let text = "a\nb\nc\n";
        assert_eq!(indent_lines(text, 0, 1, true, 1), "    a\n    b\nc\n");
        assert_eq!(indent_lines("    a\n\tb\nc", 0, 1, false, 1), "a\nb\nc");
        assert_eq!(indent_lines(text, 0, 0, true, 2), "        a\nb\nc\n");
        assert_eq!(
            toggle_case_ranges("one TWO\nThree", &[0..3, 4..7]),
            "ONE two\nThree"
        );
        assert_eq!(toggle_case_ranges("中文Ab", &[0.."中文Ab".len()]), "中文aB");
    }

    #[test]
    fn text_object_ranges() {
        let text = "one two\n\npara line\nnext";
        assert_eq!(inner_word_range(text, 1), 0..3);
        assert_eq!(inner_word_range(text, 4), 4..7);
        assert_eq!(word_object_range(text, 0, true), 0..4);
        assert_eq!(inner_word_range(text, 0), 0..3);
        // Paragraph around "para line": lines 2..=3.
        let start = text.find("para").unwrap();
        assert_eq!(paragraph_range(text, start), start..text.len());
        let first = paragraph_range("a\nb\n\nc", 0);
        assert_eq!(first, 0..3);
        assert_eq!(paragraph_object_range("a\nb\n\nc", 0, true), 0..4);
    }

    #[test]
    fn sort_selected_lines() {
        let text = "b\na\nc\n";
        assert_eq!(sort_line_range(text, 0, 1, false), "a\nb\nc\n");
        assert_eq!(sort_line_range(text, 0, 1, true), "b\na\nc\n");
        assert_eq!(sort_line_range(text, 0, 2, true), "c\nb\na\n");
    }

    #[test]
    fn visual_mode_keys_and_range_commands() {
        let mut p = String::new();
        assert_eq!(norm(&mut p, "V"), ToVisualLine);
        assert_eq!(
            translate(VimMode::VisualLine, &mut p, "V", false),
            VisualExit
        );
        assert_eq!(translate(VimMode::VisualLine, &mut p, "v", false), ToVisual);
        assert_eq!(translate(VimMode::Visual, &mut p, "V", false), ToVisualLine);

        let mut p = String::new();
        assert_eq!(visual(&mut p, "c"), ChangeSelection);
        assert_eq!(
            visual(&mut p, ">"),
            IndentSelection {
                increase: true,
                count: 1
            }
        );
        assert_eq!(
            visual(&mut p, "<"),
            IndentSelection {
                increase: false,
                count: 1
            }
        );
        assert_eq!(visual(&mut p, "~"), ToggleCaseSelection);
        assert_eq!(visual(&mut p, "o"), ToggleVisualEnd);
        assert_eq!(visual(&mut p, "i"), Pending);
        assert_eq!(
            visual(&mut p, "w"),
            SelectTextObject {
                kind: TextObjectKind::Word,
                around: false
            }
        );
        assert_eq!(visual(&mut p, "a"), Pending);
        assert_eq!(
            visual(&mut p, "p"),
            SelectTextObject {
                kind: TextObjectKind::Paragraph,
                around: true
            }
        );

        let mut p = String::new();
        assert_eq!(norm(&mut p, "g"), Pending);
        assert_eq!(norm(&mut p, "v"), ReselectVisual);

        let mut p = String::new();
        assert_eq!(norm(&mut p, "3"), Swallow);
        assert_eq!(
            visual(&mut p, ">"),
            IndentSelection {
                increase: true,
                count: 3
            }
        );

        let mut p = String::new();
        assert_eq!(visual(&mut p, ":"), Pending);
        assert_eq!(p, ":'<,'>");
        for k in ["s", "/", "a", "/", "b", "/"] {
            assert_eq!(visual(&mut p, k), Pending);
        }
        assert_eq!(
            visual(&mut p, "enter"),
            Substitute(SubSpec {
                start: None,
                end: None,
                whole: false,
                pattern: "a".into(),
                replacement: "b".into(),
                global: false,
                selection_range: true,
            })
        );
        assert_eq!(p, "");
    }

    #[test]
    fn block_specific_keys() {
        let mut p = String::new();
        assert_eq!(
            translate(VimMode::VisualBlock, &mut p, "I", false),
            BlockInsert { append: false }
        );
        assert_eq!(
            translate(VimMode::VisualBlock, &mut p, "A", false),
            BlockInsert { append: true }
        );
        assert_eq!(
            translate(VimMode::VisualBlock, &mut p, "O", false),
            ToggleBlockCorner
        );
        assert_eq!(
            translate(VimMode::VisualBlock, &mut p, "c", false),
            BlockChange
        );
    }

    #[test]
    fn block_insert_offsets_cover_each_line() {
        let text = "abc\ndef\n";
        let anchor = 0;
        let cursor = offset_for_display_col(text, 1, 1);
        assert_eq!(
            block_insert_offsets(text, anchor, cursor, false),
            vec![0, 4]
        );
        assert_eq!(block_insert_offsets(text, anchor, cursor, true), vec![2, 6]);
    }

    #[test]
    fn block_change_returns_replacement_offsets() {
        let text = "abcdef\nabcdef\n";
        let cursor = offset_for_display_col(text, 1, 1);
        let (new_text, new_cursor, offsets) = block_change(text, 0, cursor);
        assert_eq!(new_text, "cdef\ncdef\n");
        assert_eq!(new_cursor, 0);
        assert_eq!(offsets, vec![0, 5]);
    }

    #[test]
    fn selection_range_commands_parse() {
        let action = parse_command(":'<,'>s/a/b/g");
        match action {
            Substitute(spec) => {
                assert!(spec.selection_range);
                assert_eq!(spec.pattern, "a");
                assert!(spec.global);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(
            parse_command(":'<,'>sort"),
            SortSelection { descending: false }
        );
        assert_eq!(
            parse_command(":'<,'>sort!"),
            SortSelection { descending: true }
        );
    }
}
