//! End-to-end upload tests for the bidirectional sync engine: local edits,
//! new notes/folders, renames and deletes are pushed to a fake WebDAV target
//! (plaintext and E2EE), a second client re-downloads and reproduces them, and
//! remote changes win as skipped conflicts.

use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use base64::Engine;
use http_body_util::BodyExt;
use joplin_rs_core::items::{self, Item};
use joplin_rs_core::storage::Storage;
use joplin_rs_core::sync::SyncEngine;
use joplin_rs_core::webdav::WebDavClient;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

type Files = Arc<Mutex<HashMap<String, Vec<u8>>>>;

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn propfind_response(files: &HashMap<String, Vec<u8>>) -> String {
    let mut responses = String::new();
    responses.push_str(
        r#"<d:response><d:href>/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"#,
    );
    for path in files.keys() {
        responses.push_str(&format!(
            "<d:response><d:href>/{}</d:href><d:propstat><d:prop><d:getlastmodified>Thu, 01 Jan 2026 00:00:00 GMT</d:getlastmodified><d:resourcetype></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
            xml_escape(path)
        ));
    }
    format!(r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:">{responses}</d:multistatus>"#)
}

/// Spawn a fake WebDAV server, returning its base URL and the shared file map.
async fn spawn_fake_dav(files: HashMap<String, Vec<u8>>) -> (String, Files) {
    let files = Arc::new(Mutex::new(files));
    let files_clone = files.clone();
    let expected = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode("user:pass")
    );
    let app = axum::Router::new().fallback(move |req: Request| {
        let files = files_clone.clone();
        let expected = expected.clone();
        async move {
            let (parts, body) = req.into_parts();
            let auth = parts
                .headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if auth != expected {
                return axum::http::Response::builder()
                    .status(StatusCode::UNAUTHORIZED)
                    .body(Body::empty())
                    .unwrap();
            }
            let path = parts.uri.path().trim_start_matches('/').to_string();
            match parts.method.as_str() {
                "PROPFIND" => {
                    let depth = parts
                        .headers
                        .get("depth")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u32>().ok())
                        .unwrap_or(0);
                    let files = files.lock().unwrap();
                    let xml = if depth >= 1 {
                        propfind_response(&files)
                    } else {
                        propfind_response(&HashMap::new())
                    };
                    axum::http::Response::builder()
                        .status(StatusCode::MULTI_STATUS)
                        .header("content-type", "application/xml")
                        .body(Body::from(xml))
                        .unwrap()
                }
                "GET" => match files.lock().unwrap().get(&path) {
                    Some(data) => axum::http::Response::builder()
                        .status(StatusCode::OK)
                        .body(Body::from(data.clone()))
                        .unwrap(),
                    None => axum::http::Response::builder()
                        .status(StatusCode::NOT_FOUND)
                        .body(Body::empty())
                        .unwrap(),
                },
                "PUT" => {
                    let bytes = body.collect().await.unwrap().to_bytes();
                    files.lock().unwrap().insert(path, bytes.to_vec());
                    axum::http::Response::builder()
                        .status(StatusCode::CREATED)
                        .body(Body::empty())
                        .unwrap()
                }
                "MKCOL" => axum::http::Response::builder()
                    .status(StatusCode::CREATED)
                    .body(Body::empty())
                    .unwrap(),
                "DELETE" => {
                    files.lock().unwrap().remove(&path);
                    axum::http::Response::builder()
                        .status(StatusCode::NO_CONTENT)
                        .body(Body::empty())
                        .unwrap()
                }
                _ => axum::http::Response::builder()
                    .status(StatusCode::METHOD_NOT_ALLOWED)
                    .body(Body::empty())
                    .unwrap(),
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}/"), files)
}

fn remote_text(files: &Files, path: &str) -> Option<String> {
    files
        .lock()
        .unwrap()
        .get(path)
        .map(|b| String::from_utf8_lossy(b).into_owned())
}

fn note_item(id: &str, title: &str, body: &str, updated: i64) -> Item {
    let mut props = BTreeMap::new();
    props.insert("id".to_string(), Value::from(id));
    props.insert("title".to_string(), Value::from(title));
    props.insert("updated_time".to_string(), Value::from(updated));
    props.insert("created_time".to_string(), Value::from(updated));
    props.insert("parent_id".to_string(), Value::from(""));
    Item {
        item_type: items::TYPE_NOTE,
        props,
        body: body.to_string(),
    }
}

#[tokio::test]
async fn upload_plaintext_create_edit_delete_roundtrip() {
    let mut initial = HashMap::new();
    initial.insert("info.json".to_string(), br#"{"version":3}"#.to_vec());
    let (url, files) = spawn_fake_dav(initial).await;
    let client = WebDavClient::new(&url, "user", "pass").unwrap();

    // ---- Client 1: create a notebook + note locally, then upload ----
    let s1 = Storage::open_in_memory().unwrap();
    let e1 = SyncEngine::new(&client, &s1);
    let folder = s1.create_folder("Notebook A").unwrap();
    let note = s1.create_note(&folder, "Note A").unwrap();
    s1.update_note(&note, "Note A", "hello world").unwrap();

    let out1 = e1.sync(None, &mut |_| {}).await.unwrap();
    assert_eq!(out1.errors, Vec::<String>::new(), "no errors");
    assert_eq!(out1.uploaded, 2, "folder + note uploaded: {out1:?}");
    assert_eq!(out1.skipped_conflicts, Vec::<String>::new());

    let folder_path = items::system_path(&folder);
    let note_path = items::system_path(&note);
    assert!(
        remote_text(&files, &folder_path).is_some(),
        "folder uploaded"
    );
    let note_remote = remote_text(&files, &note_path).unwrap();
    assert!(note_remote.contains("hello world"), "{note_remote}");

    // ---- Client 1 again: rename notebook, edit note, create+delete a note ----
    std::thread::sleep(std::time::Duration::from_millis(5));
    s1.rename_folder(&folder, "Renamed notebook").unwrap();
    s1.update_note(&note, "Note A v2", "second body").unwrap();
    let doomed = s1.create_note(&folder, "Doomed").unwrap();
    // Upload the doomed note once, then delete it so the remote file exists.
    let _ = e1.sync(None, &mut |_| {}).await.unwrap();
    assert!(remote_text(&files, &items::system_path(&doomed)).is_some());

    s1.delete_item(&doomed).unwrap();
    let out2 = e1.sync(None, &mut |_| {}).await.unwrap();
    assert_eq!(
        out2.deleted_remote, 1,
        "doomed note deleted remotely: {out2:?}"
    );
    assert!(
        remote_text(&files, &items::system_path(&doomed)).is_none(),
        "deleted remote file must be gone"
    );
    assert!(s1.pending_deletes().unwrap().is_empty());

    // Edit of the note reached the target.
    let note_remote = remote_text(&files, &note_path).unwrap();
    assert!(note_remote.contains("second body"), "{note_remote}");
    let folder_remote = remote_text(&files, &folder_path).unwrap();
    assert!(
        folder_remote.contains("Renamed notebook"),
        "{folder_remote}"
    );

    // ---- Client 2: fresh download reproduces the state ----
    let s2 = Storage::open_in_memory().unwrap();
    let e2 = SyncEngine::new(&client, &s2);
    let out3 = e2.sync(None, &mut |_| {}).await.unwrap();
    assert_eq!(out3.errors, Vec::<String>::new(), "client2 errors");
    let folders = s2.folders().unwrap();
    assert_eq!(folders.len(), 1);
    assert_eq!(folders[0].props["title"].as_str(), Some("Renamed notebook"));
    let notes = s2.notes("", true).unwrap();
    assert_eq!(notes.len(), 1, "only the surviving note: {notes:?}");
    assert_eq!(notes[0].props["id"].as_str(), Some(note.as_str()));
    assert_eq!(notes[0].body, "second body");
    assert!(s2.note(&doomed).unwrap().is_none());
}

#[tokio::test]
async fn upload_skips_conflicting_remote_change() {
    let id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let mut initial = HashMap::new();
    initial.insert("info.json".to_string(), br#"{"version":3}"#.to_vec());
    // Remote already holds the note with a far-future updated_time.
    let remote_updated = 4_000_000_000_000i64;
    initial.insert(
        items::system_path(id),
        items::serialize(&note_item(id, "Remote", "remote body", remote_updated)).into_bytes(),
    );
    let (url, files) = spawn_fake_dav(initial).await;
    let client = WebDavClient::new(&url, "user", "pass").unwrap();

    // Local copy "already synced" at sync_time=1, then edited (updated_time=1000).
    let storage = Storage::open_in_memory().unwrap();
    let engine = SyncEngine::new(&client, &storage);
    storage
        .upsert_item(&note_item(id, "Local", "local body", 1000))
        .unwrap();
    storage.save_sync_time(id, 1, 0).unwrap();

    let outcome = engine.sync(None, &mut |_| {}).await.unwrap();
    // No merge base exists for a row that predates the base columns, so the
    // fallback is a Joplin-style conflict copy of the local version.
    assert_eq!(outcome.conflict_copies, 1, "{outcome:?}");
    assert_eq!(
        outcome.uploaded, 1,
        "only the conflict copy is uploaded: {outcome:?}"
    );
    assert_eq!(outcome.unresolved_conflicts, 0, "{outcome:?}");
    assert!(outcome.skipped_conflicts.is_empty(), "{outcome:?}");
    // The remote version was left untouched.
    let remote = remote_text(&files, &items::system_path(id)).unwrap();
    assert!(remote.contains("remote body"), "{remote}");
    // The local edit survives in a new conflict copy, uploaded to the target.
    let copy = {
        let files = files.lock().unwrap();
        files
            .iter()
            .filter(|(path, _)| path.ends_with(".md") && *path != &items::system_path(id))
            .find_map(|(path, data)| {
                let text = String::from_utf8_lossy(data);
                (text.contains("local body") && text.contains("is_conflict: 1"))
                    .then(|| (path.clone(), text.into_owned()))
            })
            .expect("conflict copy uploaded")
    };
    assert!(
        copy.1.contains(&format!("conflict_original_id: {id}")),
        "{}",
        copy.1
    );
    let local = storage.note(id).unwrap().unwrap();
    assert_eq!(
        local.body, "remote body",
        "original takes the remote version"
    );
}

#[tokio::test]
async fn upload_e2ee_edit_roundtrip() {
    let vault_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/test_vault.json"
    );
    #[derive(serde::Deserialize)]
    struct Vault {
        info_json: String,
        items: HashMap<String, String>,
        password: String,
    }
    let vault: Vault = serde_json::from_str(
        &std::fs::read_to_string(vault_path).expect("run tools/generate_fixtures.js"),
    )
    .unwrap();

    let mut initial: HashMap<String, Vec<u8>> = HashMap::new();
    initial.insert("info.json".into(), vault.info_json.clone().into_bytes());
    for (path, content) in &vault.items {
        initial.insert(path.clone(), content.as_bytes().to_vec());
    }
    let (url, files) = spawn_fake_dav(initial).await;
    let client = WebDavClient::new(&url, "user", "pass").unwrap();

    // Download the encrypted vault.
    let s1 = Storage::open_in_memory().unwrap();
    let e1 = SyncEngine::new(&client, &s1);
    e1.sync(Some(&vault.password), &mut |_| {}).await.unwrap();
    let note_id = s1.notes("", true).unwrap()[0]
        .props
        .get("id")
        .and_then(Value::as_str)
        .unwrap()
        .to_string();

    // Edit it and upload.
    std::thread::sleep(std::time::Duration::from_millis(5));
    s1.update_note(&note_id, "Edited title", "a brand new encrypted body")
        .unwrap();
    let out = e1.sync(Some(&vault.password), &mut |_| {}).await.unwrap();
    assert_eq!(out.errors, Vec::<String>::new(), "{out:?}");
    assert!(out.uploaded >= 1, "note uploaded: {out:?}");

    // The remote blob is an E2EE envelope: no plaintext leaked.
    let remote = remote_text(&files, &items::system_path(&note_id)).unwrap();
    assert!(remote.contains("encryption_applied"), "{remote}");
    assert!(!remote.contains("a brand new encrypted body"), "{remote}");
    // The envelope must carry created_time/user_* lines even for items whose
    // inner value is empty: Joplin's Synchronizer does
    // `content.user_created_time = content.created_time` and BaseModel.filter
    // turns a missing value into NaN, which aborts its whole sync with a
    // NOT NULL constraint error. See envelope_carries_timestamps_so_joplin_shim_is_safe.
    assert!(
        remote.contains("\ncreated_time: "),
        "envelope must carry a created_time line: {remote}"
    );
    assert!(
        remote.contains("\nuser_created_time: "),
        "envelope must carry a user_created_time line: {remote}"
    );

    // A second client re-downloads and reproduces the edit.
    let s2 = Storage::open_in_memory().unwrap();
    let e2 = SyncEngine::new(&client, &s2);
    e2.sync(Some(&vault.password), &mut |_| {}).await.unwrap();
    let n = s2.note(&note_id).unwrap().expect("note present");
    assert_eq!(n.body, "a brand new encrypted body");
    assert_eq!(n.props["title"].as_str(), Some("Edited title"));
}

#[tokio::test]
async fn upload_auto_merges_disjoint_note_changes() {
    let id = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let mut initial = HashMap::new();
    initial.insert("info.json".to_string(), br#"{"version":3}"#.to_vec());
    initial.insert(
        items::system_path(id),
        items::serialize(&note_item(id, "Title", "a\nb\nc\n", 4_000_000_000_000)).into_bytes(),
    );
    let (url, files) = spawn_fake_dav(initial).await;
    let client = WebDavClient::new(&url, "user", "pass").unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let engine = SyncEngine::new(&client, &storage);

    storage
        .upsert_item(&note_item(id, "Title", "a\nLOCAL\nc\n", 1000))
        .unwrap();
    // Common ancestor from the last sync.
    storage
        .save_sync_base(id, 1, 0, "Title", "a\nb\nc\n")
        .unwrap();

    let outcome = engine.sync(None, &mut |_| {}).await.unwrap();
    assert_eq!(outcome.auto_merged, 1, "{outcome:?}");
    assert_eq!(outcome.uploaded, 1, "{outcome:?}");
    assert_eq!(outcome.unresolved_conflicts, 0, "{outcome:?}");
    assert_eq!(outcome.conflict_copies, 0, "{outcome:?}");

    let remote = remote_text(&files, &items::system_path(id)).unwrap();
    assert!(remote.contains("a\nLOCAL\nc"), "{remote}");
    assert!(!remote.contains("<<<<<<<"), "{remote}");
    let local = storage.note(id).unwrap().unwrap();
    assert_eq!(local.body, "a\nLOCAL\nc\n");
}

#[tokio::test]
async fn upload_writes_local_conflict_markers_then_uploads_resolution() {
    let id = "cccccccccccccccccccccccccccccccc";
    let mut initial = HashMap::new();
    initial.insert("info.json".to_string(), br#"{"version":3}"#.to_vec());
    initial.insert(
        items::system_path(id),
        items::serialize(&note_item(
            id,
            "Title",
            "line1 remote\nline2\n",
            4_000_000_000_000,
        ))
        .into_bytes(),
    );
    let (url, files) = spawn_fake_dav(initial).await;
    let client = WebDavClient::new(&url, "user", "pass").unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let engine = SyncEngine::new(&client, &storage);

    storage
        .upsert_item(&note_item(id, "Title", "line1 local\nline2\n", 1000))
        .unwrap();
    storage
        .save_sync_base(id, 1, 0, "Title", "line1\nline2\n")
        .unwrap();

    let outcome = engine.sync(None, &mut |_| {}).await.unwrap();
    assert_eq!(outcome.unresolved_conflicts, 1, "{outcome:?}");
    assert_eq!(outcome.auto_merged, 0, "{outcome:?}");
    assert_eq!(outcome.uploaded, 0, "{outcome:?}");
    assert_eq!(outcome.conflict_copies, 0, "{outcome:?}");

    let local = storage.note(id).unwrap().unwrap();
    assert!(local.body.contains("<<<<<<< LOCAL"), "{}", local.body);
    assert!(local.body.contains("======="), "{}", local.body);
    assert!(local.body.contains(">>>>>>> REMOTE"), "{}", local.body);
    assert!(local.body.contains("line1 local"), "{}", local.body);
    assert!(local.body.contains("line1 remote"), "{}", local.body);
    assert!(storage.note_conflict(id).unwrap().is_some());

    // The remote version is untouched while the conflict is unresolved.
    let remote = remote_text(&files, &items::system_path(id)).unwrap();
    assert!(remote.contains("line1 remote"), "{remote}");
    assert!(!remote.contains("<<<<<<<"), "{remote}");

    // Removing the markers resolves the conflict and the next sync uploads it.
    storage
        .update_note(id, "Title", "line1 resolved\nline2\n")
        .unwrap();
    assert!(storage.note_conflict(id).unwrap().is_none());

    let outcome = engine.sync(None, &mut |_| {}).await.unwrap();
    assert_eq!(outcome.unresolved_conflicts, 0, "{outcome:?}");
    assert_eq!(outcome.uploaded, 1, "{outcome:?}");
    let remote = remote_text(&files, &items::system_path(id)).unwrap();
    assert!(remote.contains("line1 resolved"), "{remote}");
    assert!(!remote.contains("<<<<<<<"), "{remote}");
}

#[tokio::test]
async fn unresolved_conflict_is_not_overwritten_by_later_remote_change() {
    let id = "dddddddddddddddddddddddddddddddd";
    let path = items::system_path(id);
    let mut initial = HashMap::new();
    initial.insert("info.json".to_string(), br#"{"version":3}"#.to_vec());
    initial.insert(
        path.clone(),
        items::serialize(&note_item(
            id,
            "Title",
            "line1 remote\nline2\n",
            4_000_000_000_000,
        ))
        .into_bytes(),
    );
    let (url, files) = spawn_fake_dav(initial).await;
    let client = WebDavClient::new(&url, "user", "pass").unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let engine = SyncEngine::new(&client, &storage);

    storage
        .upsert_item(&note_item(id, "Title", "line1 local\nline2\n", 1000))
        .unwrap();
    storage
        .save_sync_base(id, 1, 0, "Title", "line1\nline2\n")
        .unwrap();

    let outcome = engine.sync(None, &mut |_| {}).await.unwrap();
    assert_eq!(outcome.unresolved_conflicts, 1, "{outcome:?}");
    let marked = storage.note(id).unwrap().unwrap().body;

    // The remote side advances while the local conflict is unresolved.
    files.lock().unwrap().insert(
        path.clone(),
        items::serialize(&note_item(
            id,
            "Title",
            "line1 newer remote\nline2\n",
            5_000_000_000_000,
        ))
        .into_bytes(),
    );
    let outcome = engine.sync(None, &mut |_| {}).await.unwrap();
    assert_eq!(outcome.unresolved_conflicts, 1, "{outcome:?}");
    assert_eq!(outcome.uploaded, 0, "{outcome:?}");
    let local = storage.note(id).unwrap().unwrap();
    assert_eq!(
        local.body, marked,
        "local markers must survive the download"
    );
    assert!(storage.note_conflict(id).unwrap().is_some());
}

#[tokio::test]
async fn e2ee_conflict_auto_merges_and_reencrypts() {
    let vault_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/test_vault.json"
    );
    #[derive(serde::Deserialize)]
    struct Vault {
        info_json: String,
        items: HashMap<String, String>,
        password: String,
    }
    let vault: Vault = serde_json::from_str(
        &std::fs::read_to_string(vault_path).expect("run tools/generate_fixtures.js"),
    )
    .unwrap();

    let mut initial: HashMap<String, Vec<u8>> = HashMap::new();
    initial.insert("info.json".into(), vault.info_json.clone().into_bytes());
    for (path, content) in &vault.items {
        initial.insert(path.clone(), content.as_bytes().to_vec());
    }
    let (url, files) = spawn_fake_dav(initial).await;
    let client = WebDavClient::new(&url, "user", "pass").unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let engine = SyncEngine::new(&client, &storage);

    engine
        .sync(Some(&vault.password), &mut |_| {})
        .await
        .unwrap();
    let id = "11111111111111111111111111111111";
    let local = storage.note(id).unwrap().unwrap();
    assert_eq!(
        local.body,
        "Hello from the encrypted vault\n\nWith a second paragraph"
    );

    // Local changes the second paragraph; remote changes the first line.
    storage
        .update_note(
            id,
            "First note",
            "Hello from the encrypted vault\n\nLOCAL paragraph",
        )
        .unwrap();

    let info = engine.fetch_sync_info().await.unwrap();
    let (key_hex, key_id) = engine
        .unlock_master_key_any(&info, &vault.password)
        .unwrap();
    let key = items::SyncKey { key_hex, key_id };
    let mut remote_item = local.clone();
    remote_item
        .props
        .insert("title".into(), Value::from("First note"));
    remote_item.props.insert(
        "updated_time".into(),
        Value::from(joplin_rs_core::time_ms() + 60_000),
    );
    remote_item.body = "REMOTE first line\n\nWith a second paragraph".into();
    let remote_blob = items::serialize_for_sync(&remote_item, Some(&key)).unwrap();
    files
        .lock()
        .unwrap()
        .insert(items::system_path(id), remote_blob.into_bytes());

    let outcome = engine
        .sync(Some(&vault.password), &mut |_| {})
        .await
        .unwrap();
    assert_eq!(outcome.auto_merged, 1, "{outcome:?}");
    assert_eq!(outcome.unresolved_conflicts, 0, "{outcome:?}");
    assert_eq!(outcome.uploaded, 1, "{outcome:?}");

    // A second client sees the merged content, still encrypted on the target.
    let remote_raw = remote_text(&files, &items::system_path(id)).unwrap();
    assert!(
        !remote_raw.contains("REMOTE first line"),
        "remote must stay encrypted"
    );
    let s2 = Storage::open_in_memory().unwrap();
    let e2 = SyncEngine::new(&client, &s2);
    e2.sync(Some(&vault.password), &mut |_| {}).await.unwrap();
    let seen = s2.note(id).unwrap().unwrap();
    assert_eq!(seen.body, "REMOTE first line\n\nLOCAL paragraph");
}

/// Reporter bug: in an E2EE vault with no password supplied, local changes must
/// NOT be uploaded (no plaintext leak into the target) but must ALSO not be
/// deleted locally; the run reports them via `upload_blocked`. Supplying the
/// password then uploads them as ciphertext.
#[tokio::test]
async fn e2ee_upload_blocked_reports_and_preserves_then_uploads_with_password() {
    let vault_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/test_vault.json"
    );
    #[derive(serde::Deserialize)]
    struct Vault {
        info_json: String,
        items: HashMap<String, String>,
        password: String,
    }
    let vault: Vault = serde_json::from_str(
        &std::fs::read_to_string(vault_path).expect("run tools/generate_fixtures.js"),
    )
    .unwrap();

    let mut initial: HashMap<String, Vec<u8>> = HashMap::new();
    initial.insert("info.json".into(), vault.info_json.clone().into_bytes());
    for (path, content) in &vault.items {
        initial.insert(path.clone(), content.as_bytes().to_vec());
    }
    let (url, files) = spawn_fake_dav(initial).await;
    let client = WebDavClient::new(&url, "user", "pass").unwrap();

    let s = Storage::open_in_memory().unwrap();
    let e = SyncEngine::new(&client, &s);
    // Download + decrypt with the password so the vault is present locally.
    e.sync(Some(&vault.password), &mut |_| {}).await.unwrap();

    // A brand-new local notebook + note, never uploaded.
    let folder = s.create_folder("Local only").unwrap();
    let note = s.create_note(&folder, "Local note").unwrap();
    s.update_note(&note, "Local note", "not on the server yet")
        .unwrap();
    assert!(remote_text(&files, &items::system_path(&note)).is_none());

    // Password-less sync: uploads skipped, reported, and NOTHING deleted locally.
    let out = e.sync(None, &mut |_| {}).await.unwrap();
    assert_eq!(
        out.uploaded, 0,
        "no plaintext upload without a password: {out:?}"
    );
    assert_eq!(
        out.deleted, 0,
        "never-uploaded items must not be deleted: {out:?}"
    );
    assert!(
        out.upload_blocked >= 2,
        "folder + note are pending: {out:?}"
    );
    assert!(
        s.note(&note).unwrap().is_some(),
        "the new note must survive the password-less sync"
    );

    // With the password the same pending changes are uploaded as ciphertext.
    let out2 = e.sync(Some(&vault.password), &mut |_| {}).await.unwrap();
    assert!(out2.uploaded >= 2, "folder + note uploaded: {out2:?}");
    assert_eq!(out2.upload_blocked, 0, "nothing left blocked: {out2:?}");
    let remote = remote_text(&files, &items::system_path(&note)).unwrap();
    assert!(
        remote.contains("encryption_applied"),
        "encrypted envelope: {remote}"
    );
    assert!(
        !remote.contains("not on the server yet"),
        "no plaintext leak: {remote}"
    );
}
