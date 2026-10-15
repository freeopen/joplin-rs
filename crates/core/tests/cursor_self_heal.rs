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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const ID_A: &str = "11111111111111111111111111111111";
const ID_B: &str = "22222222222222222222222222222222";
const ID_C: &str = "33333333333333333333333333333333";
const ID_BAD: &str = "44444444444444444444444444444444";

fn note_blob(id: &str, title: &str) -> String {
    format!(
        "{title}\n\nbody of {title}\n\nid: {id}\ntype_: 1\nparent_id: \n\
         created_time: 2026-01-01T00:00:00.000Z\nupdated_time: 2026-01-01T00:00:00.000Z\n"
    )
}

/// Start a fake plaintext WebDAV server. Returns its base URL and a GET
/// request counter.
async fn start_server(files: Arc<Mutex<HashMap<String, Vec<u8>>>>) -> (String, Arc<AtomicUsize>) {
    let get_count = Arc::new(AtomicUsize::new(0));
    let get_count_server = get_count.clone();
    let app = axum::Router::new().fallback(move |req: Request| {
        let files = files.clone();
        let get_count = get_count_server.clone();
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
                "GET" => {
                    // info.json is fetched once per sync by design; only
                    // count item downloads.
                    if path != "info.json" {
                        get_count.fetch_add(1, Ordering::SeqCst);
                    }
                    match files.lock().unwrap().get(path) {
                        Some(data) => axum::http::Response::builder()
                            .status(StatusCode::OK)
                            .body(Body::from(data.clone()))
                            .unwrap(),
                        None => axum::http::Response::builder()
                            .status(StatusCode::NOT_FOUND)
                            .body(Body::empty())
                            .unwrap(),
                    }
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
    (format!("http://{addr}/"), get_count)
}

fn standard_files() -> HashMap<String, Vec<u8>> {
    let mut files = HashMap::new();
    files.insert("info.json".into(), br#"{"version":3}"#.to_vec());
    files.insert(format!("{ID_A}.md"), note_blob(ID_A, "Note A").into_bytes());
    files.insert(format!("{ID_B}.md"), note_blob(ID_B, "Note B").into_bytes());
    files.insert(format!("{ID_C}.md"), note_blob(ID_C, "Note C").into_bytes());
    files
}

#[tokio::test]
async fn self_heals_items_missing_despite_advanced_cursor() {
    let files = Arc::new(Mutex::new(standard_files()));
    let (url, get_count) = start_server(files).await;

    let storage = Storage::open_in_memory().unwrap();

    // Poison the delta cursor exactly like the field incident: timestamp past
    // the remote mtime and every remote path already recorded in
    // filesAtTimestamp, while the items table is empty.
    let poisoned = serde_json::json!({
        "timestamp": 1767225600000i64,
        "filesAtTimestamp": [format!("{ID_A}.md"), format!("{ID_B}.md"), format!("{ID_C}.md")],
        "deletedItemsProcessed": false,
    });
    storage
        .kv_set("delta_context", &poisoned.to_string())
        .unwrap();

    let client = WebDavClient::new(&url, "user", "pass").unwrap();
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
    assert_eq!(
        get_count.load(Ordering::SeqCst),
        3,
        "each missing item fetched exactly once"
    );

    // A follow-up sync is a no-op (no perpetual re-download).
    let outcome2 = engine.sync(None, &mut |_| {}).await.unwrap();
    assert_eq!(
        outcome2.downloaded, 0,
        "healed items must not re-download: {outcome2:?}"
    );
    assert_eq!(
        get_count.load(Ordering::SeqCst),
        3,
        "steady-state sync must not GET anything"
    );
    assert_eq!(storage.notes("", true).unwrap().len(), 3);

    // filesAtTimestamp must not carry duplicate paths.
    let ctx: serde_json::Value =
        serde_json::from_str(&storage.kv_get("delta_context").unwrap().unwrap()).unwrap();
    let mut paths: Vec<String> = ctx["filesAtTimestamp"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    let before = paths.len();
    paths.sort();
    paths.dedup();
    assert_eq!(paths.len(), before, "filesAtTimestamp has duplicates");
}

#[tokio::test]
async fn self_heals_only_missing_items_and_retries_unprocessable_blob() {
    let mut files = standard_files();
    // A blob unserialize can never accept (missing type_).
    files.insert(
        format!("{ID_BAD}.md"),
        b"this is not a joplin item blob".to_vec(),
    );
    let files = Arc::new(Mutex::new(files));
    let (url, get_count) = start_server(files).await;

    let storage = Storage::open_in_memory().unwrap();
    // First full sync: B and C land locally; BAD fails; A "vanished" from the
    // local DB (cursor already past it). Poison the cursor as in the field
    // incident while pre-seeding only B and C.
    for id in [ID_B, ID_C] {
        let item =
            joplin_rs_core::items::unserialize(&note_blob(id, &format!("Note {id}"))).unwrap();
        storage.upsert_item(&item).unwrap();
        // They "landed locally" in a prior sync, so they carry sync metadata
        // (a later edit would make them dirty; an untouched item is not).
        storage
            .save_sync_time(id, joplin_rs_core::time_ms(), 0)
            .unwrap();
    }
    let poisoned = serde_json::json!({
        "timestamp": 1767225600000i64,
        "filesAtTimestamp": [
            format!("{ID_A}.md"), format!("{ID_B}.md"),
            format!("{ID_C}.md"), format!("{ID_BAD}.md")
        ],
        "deletedItemsProcessed": false,
    });
    storage
        .kv_set("delta_context", &poisoned.to_string())
        .unwrap();

    let client = WebDavClient::new(&url, "user", "pass").unwrap();
    let engine = SyncEngine::new(&client, &storage);

    let outcome = engine.sync(None, &mut |_| {}).await.unwrap();
    assert_eq!(outcome.downloaded, 1, "only missing A heals: {outcome:?}");
    assert_eq!(
        outcome.errors.len(),
        1,
        "the bad blob is reported, not fatal: {outcome:?}"
    );
    assert!(storage.note(ID_A).unwrap().is_some(), "A self-healed");
    assert!(
        storage.note(ID_BAD).unwrap().is_none(),
        "bad blob not stored"
    );
    let gets_first = get_count.load(Ordering::SeqCst);
    assert_eq!(
        gets_first, 2,
        "GETs: missing A + bad blob (B,C exist locally)"
    );

    // Second sync: A/B/C must not be refetched; the permanently-failing blob
    // is retried deliberately (a future client/password could process it).
    let outcome2 = engine.sync(None, &mut |_| {}).await.unwrap();
    assert_eq!(outcome2.downloaded, 0);
    assert_eq!(outcome2.errors.len(), 1);
    assert_eq!(
        get_count.load(Ordering::SeqCst),
        gets_first + 1,
        "only the bad blob is retried"
    );
}
