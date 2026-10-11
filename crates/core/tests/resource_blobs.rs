//! Encrypted resource blob decryption. Covers both the legacy SJCL family
//! (methods 3/4/5/7) and native FileV1 (method 9), including MULTI-CHUNK
//! blobs. Each encrypted chunk is an independently base64-padded slice of
//! the file, so decryption must base64-decode every chunk separately and
//! concatenate the bytes; decoding the concatenated base64 string fails at
//! the first interior '=' padding.
//!
//! Fixtures are produced by tools/generate_resource_fixtures.js using
//! Joplin's vendored SJCL and node webcrypto, mirroring
//! EncryptionService.encryptFile (readFileChunk reads RAW bytes and
//! base64-encodes each block on its own).

use joplin_rs_core::sync::SyncEngine;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Fixtures {
    cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Case {
    #[allow(dead_code)]
    label: String,
    method: i64,
    key_material: String,
    #[allow(dead_code)]
    master_key_id: String,
    blob: String,
    expected_hex: String,
}

#[test]
fn decrypt_legacy_sjcl_resource_blobs() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/resource_blobs.json"
    );
    let fixtures: Fixtures = serde_json::from_str(
        &std::fs::read_to_string(path).expect("run tools/generate_resource_fixtures.js"),
    )
    .unwrap();
    assert!(fixtures.cases.len() >= 10);

    for case in &fixtures.cases {
        let bytes = SyncEngine::decrypt_resource_blob(&case.blob, &case.key_material)
            .unwrap_or_else(|e| panic!("case {} (method {}): {e}", case.label, case.method));
        let got_hex = hex::encode(&bytes);
        assert_eq!(
            got_hex, case.expected_hex,
            "case {} (method {}) round-trips to the original bytes",
            case.label, case.method
        );
        // PNG cases keep the PNG magic.
        if case.label.starts_with("png") {
            assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "png magic");
        }
    }
}

#[test]
fn sjcl_resource_blob_rejects_wrong_key() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/resource_blobs.json"
    );
    let fixtures: Fixtures = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let case = fixtures
        .cases
        .iter()
        .find(|c| c.method == 5)
        .expect("an SJCL1a case exists");
    let wrong = "b".repeat(512);
    assert!(
        SyncEngine::decrypt_resource_blob(&case.blob, &wrong).is_err(),
        "a wrong key must fail authentication, not return garbage"
    );
}
