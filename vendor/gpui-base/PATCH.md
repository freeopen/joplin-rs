# gpui-base local patch

This directory vendors `gpui-base 0.7.3` from the `gpui-kit` project
(https://github.com/longbridge/gpui-kit, Apache-2.0) with one local change:

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

Everything else is upstream 0.7.3. The workspace applies this copy through
`[patch.crates-io]` in the root `Cargo.toml`.

When upgrading gpui-base, reapply the two changes above. Do not replace this
directory wholesale without checking that the block cursor still renders,
blinks, and inverts the character.
