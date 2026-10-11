//! Local secret encryption for passwords stored in config.toml.
//!
//! The WebDAV (and optionally other) password is encrypted at rest with a
//! machine-local random 256-bit key kept in a separate file (`secret.key`,
//! 0600) next to the database. This protects the password from casual
//! disclosure (backups of config.toml, screen shares, world-readable home
//! dirs) without requiring the user to remember yet another passphrase. It is
//! not a defence against someone who can read the key file.
//!
//! Encrypted values are self-describing strings:
//!
//! ```text
//! enc:v1:<base64(json-envelope)>
//! ```
//!
//! The JSON envelope is the same {salt,iv,ct} form used elsewhere in this
//! crate (`crypto::native_encrypt`), AES-256-GCM. Values without the `enc:v1:`
//! prefix are treated as legacy plaintext for backward compatibility.

use crate::crypto::{native_decrypt, native_encrypt};
use crate::error::{Error, Result};
use base64::Engine;
use rand::RngCore;
use std::fs;
use std::path::Path;

/// Prefix marking an encrypted config value.
pub const ENCRYPTED_PREFIX: &str = "enc:v1:";
/// PBKDF2 iterations used to derive the per-value GCM key from the machine key.
/// Low on purpose: unlike a human passphrase, the input is already a random
/// 256-bit key, so offline brute force is infeasible regardless.
const KDF_ITERATIONS: u32 = 10_000;
const KEY_LEN: usize = 32;

/// A machine-local 256-bit key used to encrypt config secrets.
pub struct LocalKey([u8; KEY_LEN]);

impl LocalKey {
    /// Load the key from `path`, generating and persisting a new random key
    /// (with owner-only permissions) on first use.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        match fs::read(path) {
            Ok(bytes) => {
                let hex_str: String = String::from_utf8_lossy(&bytes)
                    .chars()
                    .filter(|c| !c.is_ascii_whitespace())
                    .collect();
                let raw = hex::decode(&hex_str).map_err(|_| {
                    Error::Crypto(format!(
                        "local secret key at {} is corrupt (need {KEY_LEN} hex bytes)",
                        path.display()
                    ))
                })?;
                if raw.len() != KEY_LEN {
                    return Err(Error::Crypto(format!(
                        "local secret key at {} is corrupt (need {KEY_LEN} hex bytes)",
                        path.display()
                    )));
                }
                let mut key = [0u8; KEY_LEN];
                key.copy_from_slice(&raw);
                Ok(Self(key))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let key = Self::generate();
                key.save(path)?;
                Ok(key)
            }
            Err(e) => Err(Error::Other(format!(
                "read secret key {}: {e}",
                path.display()
            ))),
        }
    }

    /// Build a key from raw bytes (mainly for tests).
    pub fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        Self(bytes)
    }

    fn generate() -> Self {
        let mut key = [0u8; KEY_LEN];
        rand::rngs::OsRng.fill_bytes(&mut key);
        Self(key)
    }

    /// Persist the key as hex with 0600 permissions.
    fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)
                .map_err(|e| Error::Other(format!("create key dir {}: {e}", dir.display())))?;
        }
        let tmp = path.with_extension("key.tmp");
        fs::write(&tmp, hex::encode(self.0))
            .map_err(|e| Error::Other(format!("write secret key: {e}")))?;
        set_owner_only(&tmp)?;
        fs::rename(&tmp, path).map_err(|e| Error::Other(format!("install secret key: {e}")))?;
        Ok(())
    }

    /// Deterministic ASCII password carrying the full 256-bit key entropy;
    /// fed into PBKDF2 by the native envelope routines.
    fn as_password(&self) -> String {
        hex::encode(self.0)
    }

    /// Encrypt a plaintext secret into an `enc:v1:` value.
    pub fn encrypt(&self, plaintext: &str) -> Result<String> {
        let mut salt = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut salt);
        let envelope = native_encrypt(
            &self.as_password(),
            &salt,
            plaintext.as_bytes(),
            KDF_ITERATIONS,
        )?;
        let b64 = base64::engine::general_purpose::STANDARD.encode(envelope);
        Ok(format!("{ENCRYPTED_PREFIX}{b64}"))
    }

    /// Decrypt a config value. Values without the `enc:v1:` prefix are
    /// returned unchanged (legacy plaintext).
    pub fn decrypt(&self, value: &str) -> Result<String> {
        let Some(b64) = value.strip_prefix(ENCRYPTED_PREFIX) else {
            return Ok(value.to_string());
        };
        let envelope = base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .map_err(|e| Error::Crypto(format!("bad secret base64: {e}")))?;
        let plain = native_decrypt(
            &self.as_password(),
            &String::from_utf8_lossy(&envelope),
            KDF_ITERATIONS,
        )?;
        String::from_utf8(plain).map_err(|_| Error::Crypto("secret not utf8".into()))
    }

    /// Whether a config value is an encrypted secret.
    pub fn is_encrypted(value: &str) -> bool {
        value.starts_with(ENCRYPTED_PREFIX)
    }
}

#[cfg(unix)]
fn set_owner_only(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|e| Error::Other(format!("chmod secret key: {e}")))
}

#[cfg(not(unix))]
fn set_owner_only(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let key = LocalKey::from_bytes([7u8; 32]);
        for secret in ["", "simple", "p@ss w0rd!#$%", "阅读密码-καλημέρα-🔑"] {
            let enc = key.encrypt(secret).unwrap();
            assert!(enc.starts_with(ENCRYPTED_PREFIX));
            assert_ne!(enc, secret);
            assert_eq!(key.decrypt(&enc).unwrap(), secret);
        }
    }

    #[test]
    fn ciphertext_is_nondeterministic() {
        let key = LocalKey::from_bytes([7u8; 32]);
        assert_ne!(key.encrypt("same").unwrap(), key.encrypt("same").unwrap());
    }

    #[test]
    fn wrong_key_fails() {
        let a = LocalKey::from_bytes([1u8; 32]);
        let b = LocalKey::from_bytes([2u8; 32]);
        let enc = a.encrypt("secret").unwrap();
        assert!(b.decrypt(&enc).is_err());
    }

    #[test]
    fn plaintext_passes_through() {
        let key = LocalKey::from_bytes([7u8; 32]);
        assert_eq!(key.decrypt("legacy-plain").unwrap(), "legacy-plain");
        assert!(!LocalKey::is_encrypted("legacy-plain"));
        assert!(LocalKey::is_encrypted("enc:v1:xxx"));
    }

    #[test]
    fn corrupt_envelope_errors() {
        let key = LocalKey::from_bytes([7u8; 32]);
        assert!(key.decrypt("enc:v1:not-base64!!!").is_err());
        assert!(key.decrypt("enc:v1:").is_err());
    }

    #[test]
    fn key_file_roundtrip_and_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("secret.key");
        let key = LocalKey::load_or_create(&path).unwrap();
        let enc = key.encrypt("hello").unwrap();
        // Second load reads the persisted key and can decrypt.
        let reloaded = LocalKey::load_or_create(&path).unwrap();
        assert_eq!(reloaded.decrypt(&enc).unwrap(), "hello");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "key file mode should be 0600, got {mode:o}");
        }
    }

    #[test]
    fn corrupt_key_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.key");
        fs::write(&path, "deadbeef").unwrap();
        assert!(LocalKey::load_or_create(&path).is_err());
    }
}
