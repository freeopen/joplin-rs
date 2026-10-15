//! Minimal Vim-style modal editing for the note body editor.
//!
//! Only the *pure* key translation lives here: [`translate`] maps one key (plus
//! a multi-key `pending` prefix such as `g`, `d`, `y`, `:`) to a [`VimAction`].
//! The view layer owns the textarea and performs the action (dispatching gpui
//! movement actions, inserting text, saving, ...).
//!
//! Scope is deliberately small: Normal/Insert modes, basic motions
//! (`h j k l w b 0 $ gg G`), `x`, `dd`, `yy`, `p`, `u`/`Ctrl-r`, `i a I A o O`,
//! and `:w`. Visual mode, registers beyond a single line yank, macros and text
//! objects are out of scope.

/// Editor mode. Insert passes keys straight through; Normal consumes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VimMode {
    Normal,
    Insert,
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
    MoveLeft,
    MoveRight,
    MoveUp,
    MoveDown,
    WordForward,
    WordBackward,
    LineStart,
    LineEnd,
    DocStart,
    DocEnd,
    DeleteChar,
    DeleteLine,
    YankLine,
    PasteAfter,
    Undo,
    Redo,
    InsertAfterCursor,
    InsertAtLineStart,
    InsertAtLineEnd,
    OpenLineBelow,
    OpenLineAbove,
    /// `:w` - persist the working copy.
    Save,
}

/// Translate one key press.
///
/// `pending` carries the partially-typed multi-key prefix across calls and is
/// mutated in place (e.g. `g` then `g`, or a `:w` command line). `ctrl` is true
/// when Control was held (used for `Ctrl-r` redo).
pub fn translate(mode: VimMode, pending: &mut String, key: &str, ctrl: bool) -> VimAction {
    use VimAction::*;

    if mode == VimMode::Insert {
        return if key == "escape" { ToNormal } else { PassThrough };
    }

    // Command line (`:` prefix): collect until Enter; only `:w` is recognised.
    if pending.starts_with(':') {
        return match key {
            "enter" => {
                let cmd = pending.clone();
                pending.clear();
                if cmd == ":w" {
                    Save
                } else {
                    Swallow
                }
            }
            "escape" => {
                pending.clear();
                Swallow
            }
            "backspace" => {
                pending.pop();
                Swallow
            }
            _ => {
                if key.chars().count() == 1 {
                    pending.push_str(key);
                }
                Swallow
            }
        };
    }

    // Multi-key prefixes.
    if pending == "g" {
        pending.clear();
        return if key == "g" { DocStart } else { Swallow };
    }
    if pending == "d" {
        pending.clear();
        return if key == "d" { DeleteLine } else { Swallow };
    }
    if pending == "y" {
        pending.clear();
        return if key == "y" { YankLine } else { Swallow };
    }

    if ctrl {
        return if key == "r" { Redo } else { Swallow };
    }

    match key {
        "escape" => Swallow,
        "h" => MoveLeft,
        "l" => MoveRight,
        "k" => MoveUp,
        "j" => MoveDown,
        "w" => WordForward,
        "b" => WordBackward,
        "0" => LineStart,
        "$" => LineEnd,
        "g" => {
            pending.push('g');
            Swallow
        }
        "G" => DocEnd,
        "x" => DeleteChar,
        "d" => {
            pending.push('d');
            Swallow
        }
        "y" => {
            pending.push('y');
            Swallow
        }
        "p" => PasteAfter,
        "u" => Undo,
        "i" => ToInsert,
        "a" => InsertAfterCursor,
        "I" => InsertAtLineStart,
        "A" => InsertAtLineEnd,
        "o" => OpenLineBelow,
        "O" => OpenLineAbove,
        ":" => {
            pending.push(':');
            Swallow
        }
        _ => Swallow,
    }
}

#[cfg(test)]
mod tests {
    use super::VimAction::*;
    use super::*;

    fn norm(pending: &mut String, key: &str) -> VimAction {
        translate(VimMode::Normal, pending, key, false)
    }

    #[test]
    fn insert_mode_passes_through_except_escape() {
        let mut p = String::new();
        assert_eq!(translate(VimMode::Insert, &mut p, "j", false), PassThrough);
        assert_eq!(translate(VimMode::Insert, &mut p, "x", false), PassThrough);
        assert_eq!(translate(VimMode::Insert, &mut p, "escape", false), ToNormal);
    }

    #[test]
    fn basic_motions() {
        let mut p = String::new();
        assert_eq!(norm(&mut p, "h"), MoveLeft);
        assert_eq!(norm(&mut p, "l"), MoveRight);
        assert_eq!(norm(&mut p, "k"), MoveUp);
        assert_eq!(norm(&mut p, "j"), MoveDown);
        assert_eq!(norm(&mut p, "w"), WordForward);
        assert_eq!(norm(&mut p, "0"), LineStart);
        assert_eq!(norm(&mut p, "$"), LineEnd);
        assert_eq!(norm(&mut p, "G"), DocEnd);
    }

    #[test]
    fn g_prefix_and_dd_yy() {
        let mut p = String::new();
        assert_eq!(norm(&mut p, "g"), Swallow);
        assert_eq!(p, "g");
        assert_eq!(norm(&mut p, "g"), DocStart);
        assert!(p.is_empty());

        assert_eq!(norm(&mut p, "d"), Swallow);
        assert_eq!(norm(&mut p, "d"), DeleteLine);
        assert!(p.is_empty());

        assert_eq!(norm(&mut p, "y"), Swallow);
        assert_eq!(norm(&mut p, "y"), YankLine);
        assert!(p.is_empty());

        // A lone `d` followed by a non-`d` clears the prefix and swallows.
        assert_eq!(norm(&mut p, "d"), Swallow);
        assert_eq!(norm(&mut p, "x"), Swallow);
        assert!(p.is_empty());
    }

    #[test]
    fn insert_entries_and_edit_ops() {
        let mut p = String::new();
        assert_eq!(norm(&mut p, "i"), ToInsert);
        assert_eq!(norm(&mut p, "a"), InsertAfterCursor);
        assert_eq!(norm(&mut p, "I"), InsertAtLineStart);
        assert_eq!(norm(&mut p, "A"), InsertAtLineEnd);
        assert_eq!(norm(&mut p, "o"), OpenLineBelow);
        assert_eq!(norm(&mut p, "O"), OpenLineAbove);
        assert_eq!(norm(&mut p, "x"), DeleteChar);
        assert_eq!(norm(&mut p, "p"), PasteAfter);
        assert_eq!(norm(&mut p, "u"), Undo);
    }

    #[test]
    fn ctrl_r_is_redo_and_unmapped_swallowed() {
        let mut p = String::new();
        assert_eq!(translate(VimMode::Normal, &mut p, "r", true), Redo);
        assert_eq!(norm(&mut p, "q"), Swallow);
        assert_eq!(norm(&mut p, "5"), Swallow);
    }

    #[test]
    fn command_line_write() {
        let mut p = String::new();
        assert_eq!(norm(&mut p, ":"), Swallow);
        assert_eq!(p, ":");
        assert_eq!(norm(&mut p, "w"), Swallow);
        assert_eq!(p, ":w");
        assert_eq!(norm(&mut p, "enter"), Save);
        assert!(p.is_empty());

        // Esc abandons the command.
        assert_eq!(norm(&mut p, ":"), Swallow);
        assert_eq!(norm(&mut p, "escape"), Swallow);
        assert!(p.is_empty());
    }
}
