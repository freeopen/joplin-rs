# joplin-rs

A Joplin client written in Rust:

1. **E2EE sync, WebDAV only** - syncs a Joplin vault to/from a WebDAV sync
   target, encrypting and decrypting end-to-end-encrypted content (password
   master key).
2. **Notes you can edit** - browse, edit and organize notes in a GUI with
   Markdown rendering and editing, or use the CLI.

## Why joplin-rs

Joplin is my favorite daily note-taking tool, but the official client kept
raising its macOS requirements. My old MacBook cannot run anything newer than
macOS Big Sur, so recent official builds will not install on it; and after
downgrading to the last compatible Joplin release, it could no longer sync
properly with newer Joplin versions.

joplin-rs is a from-scratch Rust client built for exactly this situation:

1. **Compatibility first** - the primary goal is to run well on macOS Big Sur
   and other hardware/OS combinations the official client has dropped.
2. **Roadmap** - today: two-way E2EE sync, browsing and note editing; next:
   attachments/resources upload and more editors.
3. **Design philosophy** - fast, just-enough features, wide device
   compatibility (速度要快，功能够用，设备兼容).

## What works

- WebDAV two-way sync (PROPFIND/GET/PUT/DELETE, basicDelta algorithm like
  Joplin): downloads remote changes with a timestamp context, uploads local
  edits/creations (encrypted for E2EE vaults), physically deletes remote files
  for locally-deleted items, and refuses mass deletion with a 90% fail-safe.
  Conflicting items (the remote changed since the last sync) are skipped and
  reported, keeping the remote version.
- E2EE encrypt and decrypt:
  - master keys: `KeyV1` (PBKDF2-HMAC-SHA512 x220k + AES-256-GCM) and the legacy
    `SJCL1a/1b/3/4` password master keys used by real Joplin 1.x/2.x vaults
  - item blobs: `StringV1` (JED01 container, chunked) for both reading and
    writing (uploads use the active master key)
  - resource blobs: `FileV1` (download)
  - legacy `SJCL1a/1b/3/4` items (AES-CCM, byte-exact port of sjcl's mode)
  - item payloads encrypted with legacy SJCL (method 1a/1b/3/4) as well as `StringV1`
  - `SJCL`/`SJCL2` (OCB2) and PPK (RSA) master keys are reported as unsupported
    per item instead of failing the sync
- Local storage: SQLite (`items` + `sync_items` + `kv` + `resource_blobs` +
  `resource_fetch_times`)
- CLI: `sync` (two-way), `doctor`, `list`, `show`, `search`, `export`,
  `set-password`
- Encrypted-then-decrypt flow: sync without a password stores items
  encrypted (`pending decryption`); a later sync with the password decrypts
  and replaces them automatically
- Search: multi-word AND search over title+body (CLI `search` and GUI),
  with notebook/tag/todo filters (`--folder`, `--tag`, `--todo`,
  `--completed`), snippets (`--snippets`) and JSON output (`--json`)
- GUI (gpui-kit / Zed GPUI): Notebooks column on the left and Notes column on
  the right (each collapsible, with scrollbars and a highlighted selection),
  full-text search via a `Cmd+Option+F` prompt dialog (matches render in the
  Notes column with snippets, and an active-search bar with a clear button
  appears while a query is applied), in-note find (`Cmd+F`, `Cmd+Shift+F` to
  replace), a note-list right-click menu (new / delete / protect / move),
  Markdown rendering (GFM tables/task lists/code blocks, text selection) with
  decrypted resource images, Markdown **editing** with autosave (3s after
  typing, and immediately on blur or when switching notes) toggled with
  `Cmd+E`, create/delete notes and create/rename/delete notebooks (a notebook
  can only be deleted once its notes are), two-way sync with live status,
  rounded macOS Joplin app icon (dock/window) and About via the native
  app-name menu (in-window About on other platforms), optional reading
  password that gates notes marked as protected

## GUI usage

The GUI restores your last session on launch (last notebook + its most
recently opened note; falls back to the newest note) and syncs on launch.

**Shortcuts** (macOS / Linux+Windows):

| Action | macOS | Linux/Windows |
| --- | --- | --- |
| Toggle notebooks panel | `Cmd+B` | `Ctrl+B` |
| Toggle notes panel | `Cmd+Option+B` | `Ctrl+Alt+B` |
| Quit (syncs pending changes first) | `Cmd+Q` | `Ctrl+Q` |
| Find in current note | `Cmd+F` | `Ctrl+F` |
| Search all notes | `Cmd+Option+F` | `Ctrl+Alt+F` |
| Toggle edit/preview for the open note | `Cmd+E` | `Ctrl+E` |
| Settings | `Cmd+,` | `Ctrl+,` |
| Sync | `Cmd+R` | `Ctrl+R` |

When a list panel has focus, `j`/`k` move the highlight and `Enter` selects
(notebooks are sorted by name, notes by most-recent update).

Right-click a note in the Notes column for **新增 / 删除笔记 / 保护此笔记 /
移动**; protecting a note (or removing its lock) uses the reading password.

Global shortcuts work immediately after launch (the root view takes focus on
startup).

**Vim editing** is on by default (`vim_mode`); the note editor starts in
NORMAL mode with an indicator in the header. Supported: `Esc`, `i a I A o O`,
`h j k l w b 0 $ gg G`, `x`, `dd`, `yy`, `p`, `u`/`Ctrl-r`, and `:w`. It can be
turned off in Settings (visual mode, registers, macros and text objects are
out of scope).

**Settings** (`Cmd+,` / toolbar) mirror the CLI config surface — WebDAV
URL/username, WebDAV and E2EE passwords (stored encrypted at rest exactly
like `joplin-rs set-password`), TLS options, and toggles for Vim mode,
sync-on-startup and sync-before-quit.

**Sync lifecycle**: the status bar shows `· 本地 N 项待同步` while local notes
have unsynced changes; quitting (menu, `Cmd+Q`, or the window close button)
syncs first when `sync_on_quit` is on, then exits.

New `config.toml` keys (all default to `true` when absent): `vim_mode`,
`sync_on_startup`, `sync_on_quit`.

## Build

```
cargo build --release
```

## Versioning and releases

The build version is derived automatically from git (no manual Cargo.toml
bumps):

- Latest tag `vM.I.P`, with `feat`/`fix` commits after it -> the strongest
  level applies exactly once, release-style: `feat` -> minor (patch resets),
  `fix` -> patch, and a `!` (e.g. `feat!:`) -> major (minor and patch reset);
  other commit types do not bump, and no bump at all skips the release
- No version tag yet -> the same bump rules applied over the whole history
  on top of the Cargo.toml version
- Outside a git checkout -> the plain Cargo.toml version

`joplin-rs --version` and the GUI About panel report the same string,
`vX.Y.Z (<commit-hash>)` (the hash is omitted when built outside a git
checkout). The hash is display-only: GitHub Release tags are the plain
`vX.Y.Z` with no hash suffix.

Every push to `master` is built for Linux, Windows and macOS (x64/arm64) and
published as an official GitHub Release; CI creates the matching tag
`vM.I.P-g<hash>` on the pushed commit. To mark a milestone manually, push a
tag like `v0.2.0` (prefer bumping minor/major so the new patch base does not
go backwards relative to the automatically accumulated patch number).

Each Release body is generated from the git log: CI groups the commits since
the previous tag by their Conventional Commits prefix (`feat` -> ✨ 新增功能,
`fix` -> 🐛 修复缺陷, `perf` -> ⚡ 性能优化, everything else -> 🔧 其他变更), so
the Release page lists exactly what was added or fixed for that version.

Release artifacts: Linux and Windows ship both binaries (CLI `joplin-rs` +
GUI `joplin-rs-app`) in `tar.gz` / `zip`. On macOS the `tar.gz` carries the
CLI binary only, and the GUI additionally ships as a DMG containing a
`joplin-rs.app` bundle (drag into Applications; ad-hoc signed, not
notarized, so use right-click -> Open on first launch). To package it
locally on a Mac:

```
cargo build --release --target aarch64-apple-darwin
tools/make-dmg.sh --target aarch64-apple-darwin
```

## CLI usage

Create `config.toml` (default location: `~/.joplin/config.toml`):

```toml
webdav_url = "https://example.com/remote.php/dav/files/user/joplin"
username = "user"
password = "webdav-password"        # WebDAV account password
e2ee_password = "e2ee-password"     # optional; master key password (prompted if missing,
                                    # or leave empty + run sync to store items encrypted)
```

### Storing passwords encrypted at rest

Instead of putting a plaintext password in `config.toml`, encrypt it with a
machine-local key:

```
joplin-rs set-password              # prompts twice, no echo
joplin-rs set-password --password '...'   # non-interactive
printf '%s' "$PW" | joplin-rs set-password   # one line from stdin (scripts)
joplin-rs set-password --e2ee       # store the E2EE master-key password instead
```

This writes `password` (or `e2ee_password` with `--e2ee`) as an
`enc:v1:<base64>` value and keeps the random 256-bit key in
`~/.joplin/secret.key` (mode 0600, generated on first use). Both the CLI and
the GUI decrypt it transparently; legacy plaintext values still work. Protect
`secret.key` like the password: anyone with both files can decrypt it, and a
lost key makes the stored passwords unrecoverable.

Then:

```
joplin-rs doctor                  # checks info.json + master key password
JOPLIN_RS_PASSWORD=e2ee-password joplin-rs sync
joplin-rs list                    # or: list --item-type folder / tag
joplin-rs search "hello"          # title/body search
joplin-rs search "meeting notes"  # multi-word AND search
joplin-rs search project --folder <笔记本id> --snippets
joplin-rs search todo --todo --completed   # only completed todos
joplin-rs search xxx --json       # machine-readable output
joplin-rs show <note-id>
joplin-rs export <note-id> --output note.md
```

Data directory (SQLite db): `~/.joplin/` (override with `--data-dir` or
`JOPLIN_RS_DATA_DIR`).

## GUI usage

```
cargo run -p joplin-rs-app
```

Same config file. Press **Sync** to download; click a note to render it.

The GUI renders text with the system UI font (PingFang on macOS) and falls
back through the platform font stack for CJK. Set
`JOPLIN_RS_FONT=/path/to/font.ttf` to force a specific font family.

## Supported sync-target format

Compatible with Joplin's WebDAV target (sync version 3, `info.json`) for both
download and upload. Legacy `.sync/version.txt` targets are not supported.

## Tests

```
cargo test --workspace
```

- `crates/core/tests/crypto_fixtures.rs`: decrypts vectors generated with the
  vendored SJCL from the Joplin repo + node webcrypto
  (`tools/generate_fixtures.js`)
- `crates/core/tests/e2e_sync.rs`: full sync against a fake WebDAV server
  serving a TS-generated encrypted vault; asserts note/folder/tag/resource
  parity, no-op re-sync, deletions, fail-safe and wrong-password behaviour
- `crates/core/tests/upload_sync.rs`: two-way sync against a fake WebDAV
  server - create/edit/rename/delete upload (plaintext and E2EE), a second
  client reproducing the changes, and a remote-changed conflict being skipped
- `crates/core/tests/legacy_sjcl_sync.rs`: full sync of a legacy SJCL vault
  (SJCL4 master key + SJCL1b items)
- `crates/core/tests/plaintext_resource.rs`: unencrypted vault with a binary
  resource must round-trip byte-exact

## Not implemented (yet)

- Attachments/resources upload (resource blobs are downloaded and rendered, but
  new attachments are not created or uploaded)
- Sync targets other than WebDAV
- PPK (public/private key) master keys
- OCB2-based legacy encryption (SJCL 1/2)
- Conflict copies: a conflicting item keeps the remote version and is reported
  instead of creating a `(conflict)` note
- Note history / revisions, plugins, note locking
