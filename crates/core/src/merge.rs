//! Git-style three-way merge for note conflicts.
//!
//! The sync engine uses this module only when it has a common ancestor
//! (`sync_items.base_title` / `base_body`) for the note. Without a base there
//! is no safe way to decide which side changed a line, so the caller falls
//! back to a Joplin-style conflict copy.
//!
//! Conflict markers are written into the local note only. The sync engine
//! blocks uploads while a note still carries them, so other clients never see
//! a half-resolved merge.

/// Upper bounds for attempting a line merge. Notes above these limits fall
/// back to a conflict copy instead of risking a slow/ambiguous merge.
pub const MAX_MERGE_BYTES: usize = 1024 * 1024;
pub const MAX_MERGE_LINES: usize = 20_000;

pub const MARKER_START: &str = "<<<<<<< LOCAL";
pub const MARKER_MIDDLE: &str = "=======";
pub const MARKER_END: &str = ">>>>>>> REMOTE";
pub const TITLE_MARKER_START: &str = "<<<<<<< LOCAL TITLE";
pub const TITLE_MARKER_END: &str = ">>>>>>> REMOTE TITLE";

/// Result of a successful (possibly conflicted) three-way merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedNote {
    pub title: String,
    pub body: String,
    /// True when at least one conflict block remains in the body.
    pub conflicted: bool,
}

/// Merge a note's title and body against its common ancestor.
///
/// Returns `None` when the note is too large to merge safely; the caller then
/// falls back to a conflict copy.
pub fn merge_note(
    base_title: &str,
    base_body: &str,
    local_title: &str,
    local_body: &str,
    remote_title: &str,
    remote_body: &str,
) -> Option<MergedNote> {
    if exceeds_merge_limits(base_body, local_body, remote_body) {
        return None;
    }

    // Normalise line endings before diffing so a Windows-written note does not
    // end up with mixed markers, then restore the original convention.
    let crlf =
        base_body.contains("\r\n") || local_body.contains("\r\n") || remote_body.contains("\r\n");
    let base_n = normalize_newlines(base_body);
    let local_n = normalize_newlines(local_body);
    let remote_n = normalize_newlines(remote_body);

    let (title, title_conflict) = merge_title(base_title, local_title, remote_title);
    let (mut body, body_conflict) = merge_body(&base_n, &local_n, &remote_n);

    if title_conflict {
        let block = format!(
            "{TITLE_MARKER_START}\n{local_title}\n{MARKER_MIDDLE}\n{remote_title}\n{TITLE_MARKER_END}\n\n"
        );
        body = format!("{block}{body}");
    }

    if crlf {
        body = body.replace('\n', "\r\n");
    }

    Some(MergedNote {
        title,
        body,
        conflicted: title_conflict || body_conflict,
    })
}

/// True when the note still contains one of our conflict marker blocks.
///
/// A note is considered resolved once neither an opening nor a closing marker
/// remains. `=======` alone is not enough: it is also a valid Markdown
/// underline, and a half-deleted block should stay blocked rather than upload
/// a broken merge.
pub fn has_unresolved_markers(body: &str) -> bool {
    body.lines()
        .any(|line| line.starts_with(MARKER_START) || line.starts_with(MARKER_END))
}

fn merge_title(base: &str, local: &str, remote: &str) -> (String, bool) {
    if local == remote {
        return (local.to_string(), false);
    }
    if local == base {
        return (remote.to_string(), false);
    }
    if remote == base {
        return (local.to_string(), false);
    }
    // Two different titles. Keep the local title in the field (titles cannot
    // carry markers) and put a resolvable block at the top of the body.
    (local.to_string(), true)
}

fn merge_body(base: &str, local: &str, remote: &str) -> (String, bool) {
    if local == remote {
        return (local.to_string(), false);
    }
    if local == base {
        return (remote.to_string(), false);
    }
    if remote == base {
        return (local.to_string(), false);
    }

    use diffy::{ConflictStyle, IncompleteHunkStyle, MergeOptions};
    let mut options = MergeOptions::new();
    options
        .set_conflict_style(ConflictStyle::Merge)
        .set_incomplete_hunk_style(IncompleteHunkStyle::Git);

    match options.merge(base, local, remote) {
        Ok(merged) => (merged, false),
        Err(marked) => (relabel_conflicts(&marked), true),
    }
}

/// diffy labels its markers `ours` / `theirs`; use names that make sense in a
/// note body. Marker lines are exact matches, so ordinary note text is left
/// alone.
fn relabel_conflicts(marked: &str) -> String {
    marked
        .split('\n')
        .map(|line| match line {
            "<<<<<<< ours" => MARKER_START,
            ">>>>>>> theirs" => MARKER_END,
            _ => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn exceeds_merge_limits(base: &str, local: &str, remote: &str) -> bool {
    let bytes = base.len().max(local.len()).max(remote.len());
    if bytes > MAX_MERGE_BYTES {
        return true;
    }
    let lines = base
        .lines()
        .count()
        .max(local.lines().count())
        .max(remote.lines().count());
    lines > MAX_MERGE_LINES
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_sided_changes_need_no_conflict() {
        let merged = merge_note("T", "a\nb\nc\n", "T", "a\nb\nc\n", "T", "a\nB\nc\n").unwrap();
        assert_eq!(merged.body, "a\nB\nc\n");
        assert!(!merged.conflicted);

        let merged = merge_note("T", "a\nb\nc\n", "T", "a\nX\nc\n", "T", "a\nb\nc\n").unwrap();
        assert_eq!(merged.body, "a\nX\nc\n");
        assert!(!merged.conflicted);
    }

    #[test]
    fn disjoint_changes_auto_merge() {
        let merged = merge_note("T", "a\nb\nc\n", "T", "A\nb\nc\n", "T", "a\nb\nC\n").unwrap();
        assert_eq!(merged.body, "A\nb\nC\n");
        assert!(!merged.conflicted);
    }

    #[test]
    fn overlapping_changes_keep_markers() {
        let merged = merge_note(
            "T",
            "a\nb\nc\n",
            "T",
            "a\nLOCAL\nc\n",
            "T",
            "a\nREMOTE\nc\n",
        )
        .unwrap();
        assert!(merged.conflicted);
        assert!(merged.body.contains(MARKER_START));
        assert!(merged.body.contains(MARKER_MIDDLE));
        assert!(merged.body.contains(MARKER_END));
        assert!(merged.body.contains("LOCAL"));
        assert!(merged.body.contains("REMOTE"));
        assert!(has_unresolved_markers(&merged.body));
    }

    #[test]
    fn title_conflict_block_is_prepended() {
        let merged = merge_note("Base", "body\n", "Local", "body\n", "Remote", "body\n").unwrap();
        assert!(merged.conflicted);
        assert_eq!(merged.title, "Local");
        assert!(merged.body.starts_with(TITLE_MARKER_START));
        assert!(merged.body.contains("Local\n=======\nRemote"));
        assert!(has_unresolved_markers(&merged.body));
    }

    #[test]
    fn markers_disappear_after_manual_resolution() {
        assert!(!has_unresolved_markers("a\nresolved\nc\n"));
        assert!(has_unresolved_markers(
            "a\n<<<<<<< LOCAL\nb\n=======\nc\n>>>>>>> REMOTE\nd\n"
        ));
    }

    #[test]
    fn crlf_is_preserved() {
        let merged = merge_note(
            "T",
            "a\r\nb\r\nc\r\n",
            "T",
            "A\r\nb\r\nc\r\n",
            "T",
            "a\r\nb\r\nC\r\n",
        )
        .unwrap();
        assert_eq!(merged.body, "A\r\nb\r\nC\r\n");
    }

    #[test]
    fn oversized_notes_are_not_merged() {
        let big = "x".repeat(MAX_MERGE_BYTES + 1);
        assert!(merge_note("T", "", "T", &big, "T", "other").is_none());
    }
}
