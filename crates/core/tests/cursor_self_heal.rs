//! Regression: a poisoned/advanced delta cursor must not make the engine
//! skip items that were never stored locally. This happened in practice when
//! an older client advanced `filesAtTimestamp` over a listing it could not
//! resolve (e.g. absolute-href WebDAV responses): later syncs reported
//! "done" while the local database stayed nearly empty, so `list` showed
//! nothing.

use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use base64::Engine;
use joplin_rs_core::storage::Storage;
use joplin_rs_core::sync::SyncEngine;
use joplin_rs_core::webdav::WebDavClient;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const ID_A: &str = "11111111111111111111111111111111";
const ID_B: &str = "22222222222222222222222222222222";
const ID_C: &str = "33333333333333333333333333333333";

fn note_blob(id: &str, title: &str) -> String {
    format!(
        "{title}\n\nbody of {title}\n\nid: {id}\ntype_: 1\nparent_id: \n\
         created_time: 2026-01-01T00:00:00.000Z\nupdated_time: 2026-01-01T00:00:00.000Z\n"
    )
}

fn info_json() -> String {
    r#"{"version":3}"#.to_string()
}

#[tokio::test]
async fn self_heals_items_missing_despite_advanced_cursor() {
    let mut files: HashMap<String, Vec<u8>> = HashMap::new();
    files.insert("info.json".into(), info_json().into_bytes());
    files.insert(
        format!("{ID_A}.md"),
        note_blob(ID_A, "Note A").into_bytes(),
    );
    files.insert(
        format!("{ID_B}.md"),
        note_blob(ID_B, "Note B").into_bytes(),
    );
    files.insert(
        format!("{ID_C}.md"),
        note_blob(ID_C, "Note C").into_bytes(),
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

    // Poison the delta cursor exactly like the field incident: timestamp at
    // (past) the remote mtime and every remote path already recorded in
    // filesAtTimestamp, while the items table is empty.
    let poisoned = serde_json::json!({
        "timestamp": 1767225600000i64,
        "filesAtTimestamp": [format!("{ID_A}.md"), format!("{ID_B}.md"), format!("{ID_C}.md")],
        "deletedItemsProcessed": false,
    });
    storage
        .kv_set("delta_context", &poisoned.to_string())
        .unwrap();

    let client = WebDavClient::new(&format!("http://{addr}/"), "user", "pass").unwrap();
    let engine = SyncEngine::new(&client, &storage);

    let outcome = engine
        .sync(None, &mut |_| {})
        .await
        .expect("sync must succeed");
    assert_eq!(
        outcome.downloaded, 3,
        "missing items must be self-healed despite the advanced cursor: {outcome:?}"
    );
    assert_eq!(storage.notes("", true).unwrap().len(), 3);

    // A follow-up sync is still a no-op (no perpetual re-download).
    let outcome2 = engine.sync(None, &mut |_| {}).await.unwrap();
    assert_eq!(
        outcome2.downloaded, 0,
        "healed items must not re-download: {outcome2:?}"
    );
    assert_eq!(storage.notes("", true).unwrap().len(), 3);
}
