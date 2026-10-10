//! Regression test: servers like dufs return PROPFIND hrefs as absolute
//! server paths (/base/xxx.md) even when the WebDAV root is /base/.
//! The client must strip its own base path so GET paths match.

use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use joplin_rs_core::storage::Storage;
use joplin_rs_core::sync::SyncEngine;
use joplin_rs_core::webdav::WebDavClient;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const NOTE: &[u8] = b"Hello\n\nworld\n\nid: 11111111111111111111111111111111\ntype_: 1";

#[tokio::test]
async fn syncs_against_server_prefixed_hrefs() {
    let files: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::new(Mutex::new(HashMap::from([
        ("info.json".to_string(), b"{\"version\": 3}".to_vec()),
        (
            "11111111111111111111111111111111.md".to_string(),
            NOTE.to_vec(),
        ),
    ])));
    let files_srv = files.clone();

    let app = axum::Router::new().fallback(move |req: Request| {
        let files = files_srv.clone();
        async move {
            let method = req.method().clone();
            // The WebDAV root is /dufs/src/; simulate a server that always
            // prefixes hrefs with the full path and only answers under it.
            let full = req.uri().path().to_string();
            let rel = full
                .strip_prefix("/dufs/src/")
                .map(|s| s.to_string())
                .unwrap_or_default();
            let _ = req.into_body();
            if method == axum::http::Method::from_bytes(b"PROPFIND").unwrap() {
                let mut parts = String::from(
                    "<d:response><d:href>/dufs/src/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
                );
                for p in files.lock().unwrap().keys() {
                    parts.push_str(&format!(
                        "<d:response><d:href>/dufs/src/{p}</d:href><d:propstat><d:prop><d:getlastmodified>Thu, 01 Jan 2026 00:00:00 GMT</d:getlastmodified><d:resourcetype/></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"
                    ));
                }
                let xml = format!(
                    "<?xml version=\"1.0\"?><d:multistatus xmlns:d=\"DAV:\">{parts}</d:multistatus>"
                );
                return axum::http::Response::builder()
                    .status(StatusCode::MULTI_STATUS)
                    .header("content-type", "application/xml")
                    .body(Body::from(xml))
                    .unwrap();
            }
            match files.lock().unwrap().get(&rel) {
                Some(d) => axum::http::Response::builder()
                    .status(StatusCode::OK)
                    .body(Body::from(d.clone()))
                    .unwrap(),
                None => axum::http::Response::builder()
                    .status(StatusCode::NOT_FOUND)
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
    let client = WebDavClient::new(&format!("http://{addr}/dufs/src/"), "", "").unwrap();
    let engine = SyncEngine::new(&client, &storage);
    let outcome = engine.sync(None, &mut |_| {}).await.expect("sync failed");
    assert_eq!(outcome.downloaded, 1, "note must download: {outcome:?}");
    assert!(outcome.errors.is_empty(), "no errors: {outcome:?}");
    assert!(storage
        .note("11111111111111111111111111111111")
        .unwrap()
        .is_some());
}
