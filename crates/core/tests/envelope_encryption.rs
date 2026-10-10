//! Regression for the envelope encryption layout used by real Joplin vaults:
//! the synced file keeps plaintext envelope props and carries the JED
//! container in `encryption_cipher_text` (rather than the whole file being a
//! JED blob). A password-less sync must store the envelope verbatim
//! (`encryption_applied=1`); a later sync with the password must decrypt it
//! via the encrypted-item retry pass. Before the fix the retry pass ran the
//! whole file through the JED decoder and failed on every envelope item.

use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use base64::Engine;
use joplin_rs_core::storage::Storage;
use joplin_rs_core::sync::SyncEngine;
use joplin_rs_core::webdav::WebDavClient;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Debug, Deserialize)]
struct Vault {
    info_json: String,
    items: HashMap<String, String>,
    password: String,
    expected: Expected,
}

#[derive(Debug, Deserialize)]
struct Expected {
    notes: Vec<serde_json::Value>,
}

/// Wrap an existing fully-encrypted item JED blob in a plaintext envelope,
/// the way Joplin writes `encryption_cipher_text` items to the sync target.
fn envelope_for(id: &str, jed_blob: &str) -> String {
    format!(
        "id: {id}\n\
         type_: 1\n\
         parent_id: 00000000000000000000000000000001\n\
         encryption_cipher_text: {jed_blob}\n\
         encryption_applied: 1\n\
         updated_time: 2026-01-01T00:00:00.000Z\n\
         created_time: 2026-01-01T00:00:00.000Z\n"
    )
}

#[tokio::test]
async fn envelope_encrypted_item_decrypts_on_retry_with_password() {
    let vault_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/test_vault.json"
    );
    let vault: Vault = serde_json::from_str(
        &std::fs::read_to_string(vault_path).expect("run tools/generate_fixtures.js"),
    )
    .unwrap();
    let note = &vault.expected.notes[0];
    let note_id = note["id"].as_str().unwrap();
    let expected_title = note["title"].as_str().unwrap();

    // The fixture stores the note as a full-JED file; reuse that container as
    // the envelope's cipher text.
    let jed_blob = vault
        .items
        .get(&format!("{note_id}.md"))
        .expect("note blob present in fixture");
    assert!(jed_blob.starts_with("JED"), "fixture note must be JED");

    let mut files: HashMap<String, Vec<u8>> = HashMap::new();
    files.insert("info.json".into(), vault.info_json.clone().into_bytes());
    files.insert(
        format!("{note_id}.md"),
        envelope_for(note_id, jed_blob).into_bytes(),
    );
    let files = Arc::new(Mutex::new(files));
    let files_for_server = files.clone();

    let app = axum::Router::new().fallback(move |req: Request| {
        let files = files_for_server.clone();
        async move {
            let (parts, _body) = req.into_parts();
            let auth = parts
                .headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            let expected = format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode("user:pass")
            );
            if auth != expected {
                return axum::http::Response::builder()
                    .status(StatusCode::UNAUTHORIZED)
                    .body(Body::empty())
                    .unwrap();
            }
            let path = parts.uri.path().trim_start_matches('/');
            match parts.method.as_str() {
                "PROPFIND" => {
                    let mut responses = String::from(
                        r#"<d:response><d:href>/</d:href><d:propstat><d:prop>
                           <d:resourcetype><d:collection/></d:resourcetype></d:prop>
                           <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"#,
                    );
                    for p in files.lock().unwrap().keys() {
                        responses.push_str(&format!(
                            "<d:response><d:href>/{p}</d:href><d:propstat><d:prop>\
                             <d:getlastmodified>Thu, 01 Jan 2026 00:00:00 GMT</d:getlastmodified>\
                             <d:resourcetype/></d:prop>\
                             <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"
                        ));
                    }
                    axum::http::Response::builder()
                        .status(StatusCode::MULTI_STATUS)
                        .header("content-type", "application/xml")
                        .body(Body::from(format!(
                            r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:">{responses}</d:multistatus>"#
                        )))
                        .unwrap()
                }
                "GET" => match files.lock().unwrap().get(path) {
                    Some(data) => axum::http::Response::builder()
                        .status(StatusCode::OK)
                        .body(Body::from(data.clone()))
                        .unwrap(),
                    None => axum::http::Response::builder()
                        .status(StatusCode::NOT_FOUND)
                        .body(Body::empty())
                        .unwrap(),
                },
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

    let storage = Storage::open_in_memory().unwrap();
    let client = WebDavClient::new(&format!("http://{addr}/"), "user", "pass").unwrap();
    let engine = SyncEngine::new(&client, &storage);

    // 1) No password: envelope is stored encrypted, title is not plaintext.
    let outcome_no_pw = engine.sync(None, &mut |_| {}).await.unwrap();
    assert_eq!(
        outcome_no_pw.downloaded, 0,
        "encrypted envelope cannot be decrypted without a password: {outcome_no_pw:?}"
    );
    assert!(
        outcome_no_pw
            .pending_decryption
            .contains(&note_id.to_string()),
        "envelope item must be pending decryption: {outcome_no_pw:?}"
    );
    assert_eq!(
        storage.items_still_encrypted().unwrap(),
        vec![note_id.to_string()],
        "envelope must be stored with encryption_applied=1"
    );

    // 2) With the password, the retry pass must decrypt the envelope layout.
    let outcome_pw = engine
        .sync(Some(&vault.password), &mut |_| {})
        .await
        .expect("password sync must succeed");
    assert!(
        outcome_pw.errors.is_empty(),
        "envelope retry must not error: {outcome_pw:?}"
    );
    assert!(
        outcome_pw.downloaded >= 1,
        "the envelope item must be decrypted on retry: {outcome_pw:?}"
    );
    assert!(
        storage.items_still_encrypted().unwrap().is_empty(),
        "no item should remain encrypted: {outcome_pw:?}"
    );
    let note = storage.note(note_id).unwrap().expect("note present");
    assert_eq!(
        note.props["title"].as_str().unwrap(),
        expected_title,
        "envelope title decrypted"
    );
    assert!(!note.body.is_empty(), "envelope body decrypted");
}
