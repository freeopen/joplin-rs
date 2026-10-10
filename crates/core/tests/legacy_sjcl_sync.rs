//! Legacy SJCL vault sync test: real Joplin 1.x/2.x vaults use SJCL4 master keys
//! and SJCL1b item payloads rather than the modern KeyV1/StringV1 scheme. This
//! test proves the Rust client can sync and decrypt such a vault.

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

#[tokio::test]
async fn legacy_sjcl_vault_syncs() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/legacy_vault.json"
    );
    let vault: Vault = serde_json::from_str(
        &std::fs::read_to_string(path).expect("run tools/generate_fixtures.js"),
    )
    .unwrap();

    let mut files: HashMap<String, Vec<u8>> = HashMap::new();
    files.insert("info.json".into(), vault.info_json.clone().into_bytes());
    for (p, c) in &vault.items {
        files.insert(p.clone(), c.as_bytes().to_vec());
    }
    for (p, c) in &vault.resources {
        files.insert(p.clone(), c.as_bytes().to_vec());
    }
    let files = Arc::new(Mutex::new(files));
    let files_srv = files.clone();

    let app = axum::Router::new().fallback(move |req: Request| {
        let files = files_srv.clone();
        async move {
            let (parts, body) = req.into_parts();
            let auth = parts.headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("");
            let expected = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("user:pass"));
            if auth != expected {
                return axum::http::Response::builder().status(StatusCode::UNAUTHORIZED).body(Body::empty()).unwrap();
            }
            let path = parts.uri.path().trim_start_matches('/');
            match parts.method.as_str() {
                "PROPFIND" => {
                    let depth = parts.headers.get("depth").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
                    let mut responses = String::from(r#"<d:response><d:href>/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"#);
                    if depth >= 1 {
                        for p in files.lock().unwrap().keys() {
                            responses.push_str(&format!("<d:response><d:href>/{p}</d:href><d:propstat><d:prop><d:getlastmodified>Thu, 01 Jan 2026 00:00:00 GMT</d:getlastmodified><d:resourcetype/></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"));
                        }
                    }
                    let xml = format!(r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:">{responses}</d:multistatus>"#);
                    axum::http::Response::builder().status(StatusCode::MULTI_STATUS).body(Body::from(xml)).unwrap()
                }
                "GET" => match files.lock().unwrap().get(path) {
                    Some(d) => axum::http::Response::builder().status(StatusCode::OK).body(Body::from(d.clone())).unwrap(),
                    None => axum::http::Response::builder().status(StatusCode::NOT_FOUND).body(Body::empty()).unwrap(),
                },
                "PUT" => {
                    let bytes = body.collect().await.unwrap().to_bytes();
                    files.lock().unwrap().insert(path.to_string(), bytes.to_vec());
                    axum::http::Response::builder().status(StatusCode::CREATED).body(Body::empty()).unwrap()
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

    let storage = Storage::open_in_memory().unwrap();
    let client = WebDavClient::new(&format!("http://{addr}/"), "user", "pass").unwrap();
    let engine = SyncEngine::new(&client, &storage);
    let outcome = engine
        .sync(Some(&vault.password), &mut |_| {})
        .await
        .expect("legacy sync failed");
    assert_eq!(outcome.errors, Vec::<String>::new(), "legacy sync errors");
    assert_eq!(
        outcome.skipped_unsupported, 0,
        "legacy vault must not be skipped: {outcome:?}"
    );

    let notes = storage.notes("", true).unwrap();
    assert_eq!(notes.len(), vault.expected.notes.len());
    let expect = &vault.expected.notes[0];
    assert_eq!(
        notes[0].props["title"].as_str().unwrap(),
        expect["title"].as_str().unwrap()
    );
    assert_eq!(
        notes[0].body,
        expect["body"].as_str().unwrap(),
        "legacy SJCL1b body must round-trip"
    );
    assert!(
        notes[0].body.contains("你好"),
        "legacy unicode must survive"
    );

    let folders = storage.folders().unwrap();
    assert_eq!(folders.len(), vault.expected.folders.len());
    assert_eq!(
        folders[0].props["title"].as_str().unwrap(),
        vault.expected.folders[0]["title"].as_str().unwrap()
    );

    // Wrong password must fail hard for SJCL master keys too.
    let fresh = Storage::open_in_memory().unwrap();
    let engine2 = SyncEngine::new(&client, &fresh);
    let err = engine2
        .sync(Some("nope"), &mut |_| {})
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("ccm") || err.contains("tag") || err.contains("decrypt"),
        "wrong pw error: {err}"
    );
}
