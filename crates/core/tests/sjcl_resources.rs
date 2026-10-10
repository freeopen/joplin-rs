//! Legacy SJCL-encrypted resource blobs (Joplin attachments created before
//! FileV1 became the default file method). The header method is one of the
//! SJCL variants (3/4/5/7); each chunk decrypts with SJCL AES-CCM, 1a/1b
//! additionally unescape() the plaintext, and the concatenated result is a
//! single base64 stream of the file bytes. Fixtures are produced by
//! tools/generate_sjcl_resource_fixtures.js using Joplin's vendored SJCL,
//! mirroring EncryptionService.encryptFile.

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
        "/../../fixtures/sjcl_resources.json"
    );
    let fixtures: Fixtures = serde_json::from_str(
        &std::fs::read_to_string(path).expect("run tools/generate_sjcl_resource_fixtures.js"),
    )
    .unwrap();
    assert!(fixtures.cases.len() >= 6);

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
        "/../../fixtures/sjcl_resources.json"
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
