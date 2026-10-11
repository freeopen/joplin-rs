# joplin-rs

A read-only Joplin client written in Rust:

1. **E2EE sync, WebDAV only** - downloads a Joplin vault from a WebDAV sync
   target and decrypts end-to-end-encrypted content (password master key).
2. **Read-only notes** - browse notes in a GUI with Markdown rendering, or use
   the CLI. Nothing is ever uploaded; sync is download-only.

## What works

- WebDAV sync (PROPFIND/GET based, basicDelta algorithm like Joplin):
  initial download, incremental sync via timestamp context, deletion
  detection with a 90% fail-safe.
- E2EE decryption:
  - master keys: `KeyV1` (PBKDF2-HMAC-SHA512 x220k + AES-256-GCM) and the legacy
    `SJCL1a/1b/3/4` password master keys used by real Joplin 1.x/2.x vaults
  - item blobs: `StringV1` (JED01 container, chunked)
  - resource blobs: `FileV1`
  - legacy `SJCL1a/1b/3/4` items (AES-CCM, byte-exact port of sjcl's mode)
  - item payloads encrypted with legacy SJCL (method 1a/1b/3/4) as well as `StringV1`
  - `SJCL`/`SJCL2` (OCB2) and PPK (RSA) master keys are reported as unsupported
    per item instead of failing the sync
- Local storage: SQLite (`items` + `sync_items` + `kv` + `resource_blobs` +
  `resource_fetch_times`)
- CLI: `sync`, `doctor`, `list`, `show`, `export`, `set-password`
- Encrypted-then-decrypt flow: sync without a password stores items
  encrypted (`pending decryption`); a later sync with the password decrypts
  and replaces them automatically
- Search: multi-word AND search over title+body (CLI `search` and GUI),
  with notebook/tag/todo filters (`--folder`, `--tag`, `--todo`,
  `--completed`), snippets (`--snippets`) and JSON output (`--json`)
- GUI (egui): notebook list, note list (todos/tags), full-text search,
  Markdown rendering (GFM via egui_commonmark) with decrypted resource
  images, two-stage sync (download first with live status, then a separate
  E2EE decryption pass; notes without a password stay in the list as locked
  placeholders), rounded macOS Joplin app icon (dock/window) and About via
  the native app-name menu (in-window app menu on other platforms),
  optional reading password that locks the app on every launch

## Build

```
cargo build --release
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

The GUI uses one system CJK-capable font (PingFang/Hiragino on macOS,
Noto/WenQuanYi on Linux, Microsoft YaHei/SimHei on Windows) for both Latin
and CJK text, so mixed-script lines share a single baseline. egui's
bundled font is kept only as a fallback. Set
`JOPLIN_RS_FONT=/path/to/font.ttf` to force a specific font.

## Supported sync-target format

Read-only compatible with Joplin's WebDAV target (sync version 3, `info.json`).
Legacy `.sync/version.txt` targets are not supported.

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
- `crates/core/tests/legacy_sjcl_sync.rs`: full sync of a legacy SJCL vault
  (SJCL4 master key + SJCL1b items)
- `crates/core/tests/plaintext_resource.rs`: unencrypted vault with a binary
  resource must round-trip byte-exact

## Not implemented (by design)

- Uploading / editing (read-only client)
- Sync targets other than WebDAV
- PPK (public/private key) master keys
- OCB2-based legacy encryption (SJCL 1/2)
- Note history / revisions, plugins, note locking
