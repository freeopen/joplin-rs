# gpui-base local patch

This directory vendors `gpui-base 0.7.3` from the `gpui-kit` project
(https://github.com/longbridge/gpui-kit, Apache-2.0) with two local changes:

## Native block cursor

- `src/input/base/state.rs`
  - adds `CursorShape { Bar, Block }`
  - adds `InputBaseState::cursor_shape()` / `set_cursor_shape()`
- `src/input/base/element.rs`
  - computes a one-character block rect in the same caret layout used by the
    native thin caret
  - paints the block instead of the thin bar when `CursorShape::Block`
  - repaints the character under the block in `editor_style.background`,
    producing Zed-style reverse video
  - keeps the existing native `BlinkCursor` / focus gating

## Byte-range fix for the block cursor glyph

The block cursor repaints the character under the caret, and the caret offset
is a **byte** offset. The first version of the patch converted it to a *char*
index and then used that index to slice the rope:

```rust
let start = text.byte_to_char_idx(cursor).min(text.len_chars());
let grapheme = text.slice(start..start + 1).to_string();
```

`Rope::slice` and `Rope::chars_at` both take **byte** offsets, so on any
non-ASCII text the char index pointed at a different byte and the slice
panicked with "byte index is not on a char boundary". Entering edit mode
(⌘E) switches the caret to `CursorShape::Block`, so every note with CJK text
crashed the app as soon as the editor was mounted.

- `src/input/base/rope_ext.rs`
  - adds `char_byte_range(text, byte_offset)`, which resolves a byte offset to
    the byte range of the character covering it
- `src/input/base/element.rs`
  - the block cursor painter uses that byte range instead of a char index
- `src/input/mod.rs`
  - re-exports `char_byte_range`

Everything else is upstream 0.7.3. The workspace applies this copy through
`[patch.crates-io]` in the root `Cargo.toml`.

When upgrading gpui-base, reapply the two changes above. Do not replace this
directory wholesale without checking that the block cursor still renders,
blinks, and inverts the character. The `char_byte_range` helper must keep
working in byte space: a char index there silently breaks every non-ASCII
note.
