//! Reading-password (app launch lock) hashing and verification.
//!
//! The reading password gates access to the GUI on every launch. Only a salted
//! PBKDF2-HMAC-SHA512 hash is stored in config.toml, never the plaintext.
//!
//! Envelope format (one line, stored as `reader_password_hash` in config.toml):
//!
//! ```text
//! joplin-rs-readpw$1$<iterations>$<salt_hex>$<hash_hex>
//! ```
//!
//! `$1$` is the envelope version; iterations are recorded so they can be raised
//! later without invalidating the format. Hash comparison is constant-time.

use sha2::Sha512;

const SCHEME: &str = "joplin-rs-readpw";
const VERSION: u32 = 1;
const ITERATIONS: u32 = 220_000;
const SALT_LEN: usize = 16;
const HASH_LEN: usize = 64; // SHA-512

/// Hash a reading password into a self-describing envelope string.
pub fn hash_password(password: &str) -> String {
    use rand::RngCore;
    let mut salt = [0u8; SALT_LEN];
    rand::rngs::OsRng.fill_bytes(&mut salt);
    let mut hash = [0u8; HASH_LEN];
    pbkdf2::pbkdf2_hmac::<Sha512>(password.as_bytes(), &salt, ITERATIONS, &mut hash);
    format!(
        "{SCHEME}${VERSION}${ITERATIONS}${salt}${hash}",
        salt = hex::encode(salt),
        hash = hex::encode(hash),
    )
}

/// Verify a candidate password against an envelope produced by [`hash_password`].
///
/// Returns false for any malformed/unknown envelope rather than erroring: a
/// corrupt stored value must simply fail closed (deny access).
pub fn verify_password(password: &str, envelope: &str) -> bool {
    let Some((scheme, rest)) = envelope.split_once('$') else {
        return false;
    };
    if scheme != SCHEME {
        return false;
    }
    let mut parts = rest.split('$');
    let (Some(version), Some(iterations), Some(salt_hex), Some(hash_hex), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return false;
    };
    let (Ok(version), Ok(iterations)) = (
        version.parse::<u32>(),
        iterations.parse::<u32>(),
    ) else {
        return false;
    };
    if version != VERSION {
        return false;
    }
    let (Ok(salt), Ok(expected)) = (
        hex::decode(salt_hex),
        hex::decode(hash_hex),
    ) else {
        return false;
    };
    if expected.len() != HASH_LEN || salt.is_empty() {
        return false;
    }
    let mut actual = [0u8; HASH_LEN];
    pbkdf2::pbkdf2_hmac::<Sha512>(password.as_bytes(), &salt, iterations, &mut actual);
    // Constant-time comparison; length was checked above.
    let mut diff = 0u8;
    for (a, b) in actual.iter().zip(expected.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_and_verify_roundtrip() {
        let env = hash_password("hunter2");
        assert!(verify_password("hunter2", &env));
        assert!(!verify_password("hunter", &env));
        assert!(!verify_password("", &env));
    }

    #[test]
    fn hashes_use_distinct_salts() {
        assert_ne!(hash_password("same"), hash_password("same"));
    }

    #[test]
    fn unicode_password_roundtrip() {
        let pw = "阅读密码-🔒-καλημέρα";
        let env = hash_password(pw);
        assert!(verify_password(pw, &env));
        assert!(!verify_password("阅读密码", &env));
    }

    #[test]
    fn rejects_malformed_envelopes() {
        for bad in [
            "",
            "plaintext",
            "wrong-scheme$1$1$00$00",
            "$1$1$00$00",
            "joplin-rs-readpw$2$1$00$00",
            "joplin-rs-readpw$1$notnum$00$00",
            "joplin-rs-readpw$1$1$xyz$00",
            "joplin-rs-readpw$1$1$00$zz",
            "joplin-rs-readpw$1$1$00$00$extra",
        ] {
            assert!(!verify_password("anything", bad), "should reject: {bad}");
        }
    }
}
