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
- CLI: `sync`, `doctor`, `list`, `show`, `export`
- Encrypted-then-decrypt flow: sync without a password stores items
  encrypted (`pending decryption`); a later sync with the password decrypts
  and replaces them automatically
- GUI (egui): notebook list, note list (todos/tags), full-text search,
  Markdown rendering (GFM via egui_commonmark) with decrypted resource
  images, in-app E2EE password field, background sync

## Build

```
cargo build --release
```

## CLI usage

Create `config.toml` (default location: `~/Library/Application Support/joplin-rs/config.toml`
on macOS, `$XDG_CONFIG_HOME/joplin-rs/config.toml` on Linux):

```toml
webdav_url = "https://example.com/remote.php/dav/files/user/joplin"
username = "user"
password = "webdav-password"        # WebDAV account password
e2ee_password = "e2ee-password"     # optional; master key password (prompted if missing,
                                    # or leave empty + run sync to store items encrypted)
```

Then:

```
joplin-rs doctor                  # checks info.json + master key password
JOPLIN_RS_PASSWORD=e2ee-password joplin-rs sync
joplin-rs list                    # or: list --item-type folder / tag
joplin-rs list --query "hello"
joplin-rs show <note-id>
joplin-rs export <note-id> --output note.md
```

Data directory (SQLite db): `~/Library/Application Support/joplin-rs/` on macOS.

## GUI usage

```
cargo run -p joplin-rs-app
```

Same config file. Press **Sync** to download; click a note to render it.

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
