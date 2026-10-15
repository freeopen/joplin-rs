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
    assert_eq!(outcome.uploaded, 0, "conflict must not upload: {outcome:?}");
    assert_eq!(outcome.skipped_conflicts.len(), 1, "{outcome:?}");
    assert!(outcome.skipped_conflicts[0].contains(id));
    // The remote version was left untouched.
    let remote = remote_text(&files, &items::system_path(id)).unwrap();
    assert!(remote.contains("remote body"), "{remote}");
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

    // A second client re-downloads and reproduces the edit.
    let s2 = Storage::open_in_memory().unwrap();
    let e2 = SyncEngine::new(&client, &s2);
    e2.sync(Some(&vault.password), &mut |_| {}).await.unwrap();
    let n = s2.note(&note_id).unwrap().expect("note present");
    assert_eq!(n.body, "a brand new encrypted body");
    assert_eq!(n.props["title"].as_str(), Some("Edited title"));
}
