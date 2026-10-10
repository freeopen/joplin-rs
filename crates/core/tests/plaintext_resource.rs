//! Regression test: unencrypted vaults serve resource blobs as raw bytes.
//! The sync engine must fetch them binary-safe (no UTF-8 lossy decoding);
//! a PNG must round-trip byte-exact into resource_blobs.

use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use joplin_rs_core::storage::Storage;
use joplin_rs_core::sync::SyncEngine;
use joplin_rs_core::webdav::WebDavClient;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0xFF, 0xFE, 0x00, 0x01, // deliberately non-UTF8 tail to catch any lossy decode
];

#[tokio::test]
async fn plaintext_binary_resource_round_trips() {
    let note = "Note\n\nimg: :/55555555555555555555555555555555\n\nid: 11111111111111111111111111111111\ntype_: 1".to_string();
    let res_meta = "pixel\n\nid: 55555555555555555555555555555555\ntype_: 4\nmime: image/png\nblob_updated_time: 1000".to_string();
    let files: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::new(Mutex::new(HashMap::from([
        ("info.json".to_string(), b"{\"version\": 3}".to_vec()),
        ("11111111111111111111111111111111.md".to_string(), note.into_bytes()),
        ("55555555555555555555555555555555.md".to_string(), res_meta.into_bytes()),
        (".resource/55555555555555555555555555555555".to_string(), PNG.to_vec()),
    ])));
    let files_srv = files.clone();

    let app = axum::Router::new().fallback(move |req: Request| {
        let files = files_srv.clone();
        async move {
            let method = req.method().clone();
            let path = req.uri().path().trim_start_matches('/').to_string();
            let _ = req.into_body(); // drain request body (PROPFIND carries an XML doc)
            if method == axum::http::Method::from_bytes(b"PROPFIND").unwrap() {
                let mut parts = String::from("<d:response><d:href>/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>");
                for p in files.lock().unwrap().keys() {
                    parts.push_str(&format!(
                        "<d:response><d:href>/{p}</d:href><d:propstat><d:prop><d:getlastmodified>Thu, 01 Jan 2026 00:00:00 GMT</d:getlastmodified><d:resourcetype/></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"));
                }
                let xml = format!("<?xml version=\"1.0\"?><d:multistatus xmlns:d=\"DAV:\">{parts}</d:multistatus>");
                return axum::http::Response::builder()
                    .status(StatusCode::MULTI_STATUS)
                    .header("content-type", "application/xml")
                    .body(Body::from(xml))
                    .unwrap();
            }
            match files.lock().unwrap().get(&path) {
                Some(d) => axum::http::Response::builder().status(StatusCode::OK).body(Body::from(d.clone())).unwrap(),
                None => axum::http::Response::builder().status(StatusCode::NOT_FOUND).body(Body::empty()).unwrap(),
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let storage = Storage::open_in_memory().unwrap();
    let client = WebDavClient::new(&format!("http://{addr}/"), "", "").unwrap();
    let engine = SyncEngine::new(&client, &storage);
    let outcome = engine.sync(None, &mut |_| {}).await.expect("sync failed");
    assert_eq!(outcome.downloaded, 2, "note + resource meta: {outcome:?}");
    assert_eq!(outcome.resources_downloaded, 1, "binary resource must download: {outcome:?}");

    let blob = storage
        .resource_blob("55555555555555555555555555555555")
        .unwrap()
        .expect("blob stored");
    assert_eq!(blob, PNG.to_vec(), "resource blob must be byte-exact (no lossy UTF-8)");
}
