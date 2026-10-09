//! End-to-end sync test: an in-memory fake WebDAV server serves a
//! TS-generated encrypted vault (fixtures/test_vault.json), and the Rust sync
//! engine must reproduce the original notes/folders/tags/resources exactly.

use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use base64::Engine;
use http_body_util::BodyExt;
use joplin_rs_core::storage::Storage;
use joplin_rs_core::sync::SyncEngine;
use joplin_rs_core::webdav::WebDavClient;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tower::Service;

#[derive(Debug, Deserialize)]
struct Vault {
    info_json: String,
    items: HashMap<String, String>,
    resources: HashMap<String, String>,
    password: String,
    expected: Expected,
}

#[derive(Debug, Deserialize)]
struct Expected {
    notes: Vec<Value>,
    folders: Vec<Value>,
    tags: Vec<Value>,
    #[serde(rename = "noteTags")]
    note_tags: Vec<Value>,
    resources: Vec<Value>,
}

/// Fake WebDAV: handles PROPFIND (depth 1), GET, PUT, MKCOL with Basic auth.
#[derive(Clone)]
struct FakeDav {
    files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    username: String,
    password: String,
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

impl FakeDav {
    fn propfind_response(&self, depth: u32) -> String {
        let files = self.files.lock().unwrap();
        let mut responses = String::new();
        // Root collection
        responses.push_str(
            r#"<d:response><d:href>/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"#,
        );
        if depth >= 1 {
            for path in files.keys() {
                let is_dir = false;
                responses.push_str(&format!(
                    "<d:response><d:href>/{}{}</d:href><d:propstat><d:prop><d:getlastmodified>Thu, 01 Jan 2026 00:00:00 GMT</d:getlastmodified><d:resourcetype>{}</d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
                    xml_escape(path),
                    "",
                    if is_dir { "<d:collection/>" } else { "" }
                ));
            }
        }
        format!(r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:">{responses}</d:multistatus>"#)
    }
}

#[tokio::test]
async fn e2e_sync_encrypted_vault() {
    let vault_path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/test_vault.json");
    let vault: Vault =
        serde_json::from_str(&std::fs::read_to_string(vault_path).expect("run tools/generate_fixtures.js")).unwrap();

    // Build the fake server filesystem.
    let mut files: HashMap<String, Vec<u8>> = HashMap::new();
    files.insert("info.json".to_string(), vault.info_json.clone().into_bytes());
    for (path, content) in &vault.items {
        files.insert(path.clone(), content.as_bytes().to_vec());
    }
    for (path, content) in &vault.resources {
        files.insert(path.clone(), content.as_bytes().to_vec());
    }
    let files = Arc::new(Mutex::new(files));
    let dav = FakeDav { files: files.clone(), username: "user".into(), password: "pass".into() };
    let dav_clone = dav.clone();

    let app = axum::Router::new().fallback(move |req: Request| {
        let dav = dav_clone.clone();
        async move {
            let (parts, body) = req.into_parts();
            // Basic auth check
            let auth = parts.headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("");
            let expected = format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", dav.username, dav.password))
            );
            if auth != expected {
                return axum::http::Response::builder().status(StatusCode::UNAUTHORIZED).body(Body::empty()).unwrap();
            }
            let path = parts.uri.path().trim_start_matches('/');
            match parts.method.as_str() {
                "PROPFIND" => {
                    let depth = parts
                        .headers
                        .get("depth")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u32>().ok())
                        .unwrap_or(0);
                    let xml = dav.propfind_response(depth);
                    axum::http::Response::builder()
                        .status(StatusCode::MULTI_STATUS)
                        .header("content-type", "application/xml")
                        .body(Body::from(xml))
                        .unwrap()
                }
                "GET" => match dav.files.lock().unwrap().get(path) {
                    Some(data) => axum::http::Response::builder()
                        .status(StatusCode::OK)
                        .body(Body::from(data.clone()))
                        .unwrap(),
                    None => axum::http::Response::builder().status(StatusCode::NOT_FOUND).body(Body::empty()).unwrap(),
                },
                "PUT" => {
                    let bytes = body.collect().await.unwrap().to_bytes();
                    dav.files.lock().unwrap().insert(path.to_string(), bytes.to_vec());
                    axum::http::Response::builder().status(StatusCode::CREATED).body(Body::empty()).unwrap()
                }
                "MKCOL" => axum::http::Response::builder().status(StatusCode::CREATED).body(Body::empty()).unwrap(),
                "DELETE" => {
                    dav.files.lock().unwrap().remove(path);
                    axum::http::Response::builder().status(StatusCode::NO_CONTENT).body(Body::empty()).unwrap()
                }
                _ => axum::http::Response::builder().status(StatusCode::METHOD_NOT_ALLOWED).body(Body::empty()).unwrap(),
            }
        }
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Run the sync.
    let storage = Storage::open_in_memory().unwrap();
    let client = WebDavClient::new(&format!("http://{addr}/"), "user", "pass").unwrap();
    let engine = SyncEngine::new(&client, &storage);
    let mut messages = Vec::new();
    let outcome = engine
        .sync(Some(&vault.password), &mut |msg| messages.push(msg.to_string()))
        .await
        .expect("sync failed");
    println!("messages: {messages:?}");
    println!("outcome: {outcome:?}");

    assert_eq!(outcome.errors, Vec::<String>::new(), "sync errors");
    assert_eq!(outcome.downloaded, vault.items.len(), "all items downloaded");
    assert_eq!(outcome.resources_downloaded, vault.resources.len(), "resources downloaded");

    // ---- Parity assertions vs the TS-side expected values ----
    let notes = storage.notes("", true).unwrap();
    assert_eq!(notes.len(), vault.expected.notes.len());
    for expect in &vault.expected.notes {
        let id = expect["id"].as_str().unwrap();
        let found = notes.iter().find(|n| n.props["id"].as_str() == Some(id));
        let note = found.unwrap_or_else(|| panic!("note {id} missing"));
        assert_eq!(note.props["title"].as_str().unwrap(), expect["title"].as_str().unwrap(), "title of {id}");
        if let Some(body) = expect["body"].as_str() {
            assert_eq!(note.body, body, "body of {id}");
        }
        if let Some(body_len) = expect["body_len"].as_i64() {
            // An emoji whose UTF-16 surrogate pair straddles a 64k chunk
            // boundary decodes as two U+FFFD chars (same as Joplin/JS
            // TextDecoder per-chunk decoding), so allow a small drift.
            let count = note.body.chars().count() as i64;
            assert!(
                count >= body_len && count <= body_len + 32,
                "multi-chunk body length of {id}: {count} vs {body_len}"
            );
            assert!(note.body.contains("한글"), "multi-chunk unicode of {id}");
            assert!(note.body.contains("🎉"), "multi-chunk emoji of {id}");
            assert!(note.body.contains("Paragraph line 0 "), "chunk 0 content of {id}");
            assert!(note.body.contains("Paragraph line 3999"), "last chunk content of {id}");
        }
        if let Some(link) = expect["resource_link"].as_str() {
            assert!(note.body.contains(link), "resource link in body of {id}");
        }
        if let Some(ml) = expect["markup_language"].as_i64() {
            assert_eq!(note.props["markup_language"].as_i64().unwrap(), ml, "markup_language of {id}");
        }
        if let Some(parent) = expect["parent_id"].as_str() {
            assert_eq!(note.props["parent_id"].as_str().unwrap(), parent);
        }
    }

    let folders = storage.folders().unwrap();
    assert_eq!(folders.len(), vault.expected.folders.len());
    assert_eq!(folders[0].props["title"].as_str().unwrap(), vault.expected.folders[0]["title"].as_str().unwrap());

    let tags = storage.tags().unwrap();
    assert_eq!(tags.len(), vault.expected.tags.len());
    assert_eq!(tags[0].props["title"].as_str().unwrap(), vault.expected.tags[0]["title"].as_str().unwrap());

    let note_tags = storage.note_tags().unwrap();
    assert_eq!(note_tags.len(), vault.expected.note_tags.len());

    // Resource blob content parity
    for expect in &vault.expected.resources {
        let id = expect["id"].as_str().unwrap();
        let blob = storage
            .resource_blob(id)
            .unwrap()
            .unwrap_or_else(|| panic!("resource {id} missing"));
        // Starts with PNG magic
        assert_eq!(&blob[..8], b"\x89PNG\r\n\x1a\n", "resource {id} must be the decrypted PNG");
        let res = storage.resource(id).unwrap().unwrap();
        assert_eq!(res.props["mime"].as_str().unwrap(), expect["mime"].as_str().unwrap());
    }

    // ---- Second sync must be a no-op ----
    let mut messages2 = Vec::new();
    let outcome2 = engine.sync(Some(&vault.password), &mut |m| messages2.push(m.to_string())).await.unwrap();
    println!("messages2: {messages2:?}");
    println!("outcome2: {outcome2:?}");
    assert_eq!(outcome2.downloaded, 0, "second sync should download nothing");
    assert_eq!(outcome2.deleted, 0, "second sync should delete nothing");

    // ---- Deletion: remove a note remotely, sync, verify local delete ----
    let note2_path = format!("{}.md", vault.expected.notes[1]["id"].as_str().unwrap());
    files.lock().unwrap().remove(&note2_path);
    let outcome3 = engine.sync(Some(&vault.password), &mut |_| {}).await.unwrap();
    assert_eq!(outcome3.deleted, 1, "deleted note should be removed locally");
    assert!(storage.note(vault.expected.notes[1]["id"].as_str().unwrap()).unwrap().is_none());
    assert!(storage.note(vault.expected.notes[0]["id"].as_str().unwrap()).unwrap().is_some());

    // ---- Failsafe: wipe the whole remote, sync must refuse ----
    files.lock().unwrap().clear();
    let result = engine.sync(Some(&vault.password), &mut |_| {}).await;
    assert!(result.is_err(), "failsafe must refuse mass deletion");
    let err = result.unwrap_err().to_string();
    assert!(err.contains("fail-safe"), "error should mention fail-safe: {err}");
    // Local data intact
    assert!(storage.note(vault.expected.notes[0]["id"].as_str().unwrap()).unwrap().is_some());

    // ---- No-password sync: items stored encrypted, then retried with password ----
    files.lock().unwrap().insert("info.json".to_string(), vault.info_json.clone().into_bytes());
    for (p_, c_) in &vault.items {
        files.lock().unwrap().insert(p_.clone(), c_.as_bytes().to_vec());
    }
    for (p_, c_) in &vault.resources {
        files.lock().unwrap().insert(p_.clone(), c_.as_bytes().to_vec());
    }
    let storage_no_pw = Storage::open_in_memory().unwrap();
    let engine_no_pw = SyncEngine::new(&client, &storage_no_pw);
    let outcome4 = engine_no_pw.sync(None, &mut |_| {}).await.expect("no-password sync should not fail");
    assert!(outcome4.pending_decryption.len() >= vault.expected.notes.len(), "notes must be pending: {outcome4:?}");
    assert_eq!(outcome4.resources_downloaded, 0, "no resource plaintext without password");
    // Encrypted notes must NOT expose plaintext
    for expect in &vault.expected.notes {
        let id = expect["id"].as_str().unwrap();
        let note = storage_no_pw.note(id).unwrap().expect("placeholder stored");
        let title = note.props.get("title").and_then(Value::as_str).unwrap_or("");
        assert_ne!(title, expect["title"].as_str().unwrap(), "title must not be plaintext without password");
        assert!(note.body.is_empty() || !note.body.contains(expect["body"].as_str().unwrap().split('\n').next().unwrap()));
    }

    // Retry with password on the same storage: everything decrypts.
    let outcome5 = engine_no_pw.sync(Some(&vault.password), &mut |_| {}).await.expect("retry sync failed");
    assert!(outcome5.downloaded >= vault.expected.notes.len(), "retry must decrypt pending items: {outcome5:?}");
    for expect in &vault.expected.notes {
        let id = expect["id"].as_str().unwrap();
        let note = storage_no_pw.note(id).unwrap().expect("note present");
        assert_eq!(note.props["title"].as_str().unwrap(), expect["title"].as_str().unwrap(), "retry: title of {id}");
        if let Some(body) = expect["body"].as_str() {
            assert_eq!(note.body, body, "retry: body of {id}");
        }
        if expect["body_len"].as_i64().is_some() {
            assert!(note.body.contains("Paragraph line 3999"), "retry: big note content of {id}");
        }
    }
    let blob = storage_no_pw
        .resource_blob(vault.expected.resources[0]["id"].as_str().unwrap())
        .unwrap()
        .expect("resource decrypted on retry");
    assert_eq!(&blob[..8], b"\x89PNG\r\n\x1a\n", "resource must decrypt on retry");

    // ---- Wrong password must fail with a crypto error ----
    // (restore the remote info.json; the failsafe step cleared everything)
    files.lock().unwrap().insert("info.json".to_string(), vault.info_json.clone().into_bytes());
    let storage_fresh = Storage::open_in_memory().unwrap();
    let engine2 = SyncEngine::new(&client, &storage_fresh);
    let result = engine2.sync(Some("wrong-password"), &mut |_| {}).await;
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("decrypt") || msg.contains("AES-GCM") || msg.contains("password"),
        "wrong-password error should be about decryption: {msg}"
    );
}
