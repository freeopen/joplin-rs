# gpui-pre-macos local patch

This directory vendors `gpui-pre-macos 0.3.9` (Zed's `gpui_macos`, Apache-2.0)
from the registry so the macOS app-delegate reopen behavior can be fixed
without waiting for an upstream release. It is wired in through
`[patch.crates-io]` in the workspace `Cargo.toml`, the same pattern as
`vendor/gpui-base`.

## Reopen delegate must return `BOOL`

- `src/platform.rs`
  - `build_classes()`: the method cast is now
    `extern "C" fn(&mut Object, Sel, id, bool) -> BOOL`.
  - `should_handle_reopen()` now returns `BOOL` and always returns `YES`
    after the optional `App::on_reopen` callback.

AppKit's `applicationShouldHandleReopen:hasVisibleWindows:` is declared as
returning `BOOL`. Upstream declared the Rust implementation as returning
`()`, so AppKit read an undefined value from the return register. That breaks
the macOS default reopen behavior: when a minimized app is selected in the
app switcher and revealed with `⌘⇥` then `⌘⌥` (or when its Dock icon is
clicked), AppKit only deminiaturizes and orders the windows front if the
delegate returns `YES`. With the old signature the window stayed in the Dock.

Returning `YES` also keeps AppKit's default handling for the no-window case,
while the preserved `reopen` callback path lets the application create a new
main window after the user closes the last one (macOS apps stay running when
their last window closes).

## Re-applying on upgrade

1. Copy the new `gpui-pre-macos-<version>` registry crate here (keep
   `Cargo.toml`, `Cargo.toml.orig`, `LICENSE-APACHE`, `src/`).
2. Re-apply the two changes above in `src/platform.rs`.
3. Update the version in the workspace `Cargo.toml` patch entry if the crate
   version changes.
4. Run `cargo update -p gpui-pre-macos` and the macOS manual checks in
   `README.md` (minimize, `⌘⇥` + `⌘⌥`, close window then Dock click).
