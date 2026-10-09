//! Joplin E2EE encryption formats.
//!
//! Wire format (hex-encoded ASCII), see packages/lib/services/e2ee/EncryptionService.ts:
//!   JED01 + <6-hex metadata size> + <2-hex method> + <32-hex master key id>
//!   followed by chunks: <6-hex length> + <payload>
//!
//! Methods handled here:
//! - KeyV1   (8): master key content, PBKDF2-SHA512 iter 220000, AES-256-GCM, hex payload
//! - FileV1  (9): base64 payload, PBKDF2-SHA512 iter 3
//! - StringV1(10): utf16le payload, PBKDF2-SHA512 iter 3
//! - SJCL 1a/1b/3/4: AES-CCM with parameters embedded in the SJCL JSON blob
//!
//! OCB2 (SJCL 1 and 2) is not supported and surfaces as Error::Unsupported.

use crate::error::{Error, Result};
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use serde_json::Value;

pub const HEADER_IDENTIFIER: &str = "JED";
pub const HEADER_VERSION: u8 = 1;

pub const METHOD_SJCL: i64 = 1;
pub const METHOD_SJCL2: i64 = 2;
pub const METHOD_SJCL3: i64 = 3;
pub const METHOD_SJCL4: i64 = 4;
pub const METHOD_SJCL1A: i64 = 5;
pub const METHOD_KEY_V1: i64 = 8;
pub const METHOD_FILE_V1: i64 = 9;
pub const METHOD_STRING_V1: i64 = 10;
pub const METHOD_CUSTOM: i64 = 6;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptionHeader {
    pub method: i64,
    pub master_key_id: String,
}

fn left_pad(s: &str, len: usize, c: char) -> String {
    let mut out = String::with_capacity(len.max(s.len()));
    for _ in 0..(len - s.len().min(len)) {
        out.push(c);
    }
    out.push_str(s);
    out
}

fn is_valid_header_identifier(id: &str) -> bool {
    id.len() == 5 && id.starts_with("JED") && id.ends_with("01")
}

/// Decode the JED01 header from the start of an encrypted blob.
pub fn decode_header(data: &str) -> Result<EncryptionHeader> {
    let invalid = || Error::Crypto(format!("invalid encryption identifier: {}", &data.chars().take(16).collect::<String>()));
    if data.len() < 5 + 6 {
        return Err(invalid());
    }
    let identifier = &data[..5];
    if !is_valid_header_identifier(identifier) {
        return Err(invalid());
    }
    let md_size_hex = &data[5..11];
    let md_size = usize::from_str_radix(md_size_hex, 16).map_err(|_| invalid())?;
    if md_size == 0 || data.len() < 11 + md_size {
        return Err(Error::Crypto(format!("invalid header metadata size: {md_size_hex}")));
    }
    let metadata = &data[11..11 + md_size];
    if metadata.len() < 2 + 32 {
        return Err(Error::Crypto("header metadata too short".into()));
    }
    let method = i64::from_str_radix(&metadata[..2], 16).map_err(|_| Error::Crypto("bad method hex".into()))?;
    Ok(EncryptionHeader { method, master_key_id: metadata[2..34].to_string() })
}

/// Parse the remaining chunks after the header: (6-hex length, payload) pairs.
pub fn decode_chunks(data: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut index = 0;
    let bytes = data.as_bytes();
    while index < bytes.len() {
        if index + 6 > bytes.len() {
            return Err(Error::Crypto(format!("invalid block size at {index}")));
        }
        let length_hex = &data[index..index + 6];
        let length = usize::from_str_radix(length_hex, 16).map_err(|_| Error::Crypto(format!("invalid block size: {length_hex}")))?;
        index += 6;
        if length == 0 {
            continue;
        }
        if index + length > bytes.len() {
            return Err(Error::Crypto("truncated chunk".into()));
        }
        out.push(data[index..index + length].to_string());
        index += length;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Native methods (KeyV1 / FileV1 / StringV1)
// ---------------------------------------------------------------------------

/// JSON envelope produced by Joplin's native crypto: {salt, iv, ct} base64 fields.
#[derive(Debug, Clone)]
pub struct NativeEnvelope {
    pub salt: Vec<u8>,
    pub iv: Vec<u8>,
    pub ct: Vec<u8>,
}

pub fn parse_native_envelope(json: &str) -> Result<NativeEnvelope> {
    let v: Value = serde_json::from_str(json).map_err(|e| Error::Crypto(format!("bad envelope json: {e}")))?;
    let b64 = |key: &str| -> Result<Vec<u8>> {
        let s = v.get(key).and_then(Value::as_str).ok_or_else(|| Error::Crypto(format!("missing {key}")))?;
        base64::engine::general_purpose::STANDARD.decode(s).map_err(|e| Error::Crypto(format!("bad base64 in {key}: {e}")))
    };
    Ok(NativeEnvelope { salt: b64("salt")?, iv: b64("iv")?, ct: b64("ct")? })
}

fn derive_key(password: &str, salt: &[u8], iterations: u32) -> Result<[u8; 32]> {
    let mut key = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<sha2::Sha512>(password.as_bytes(), salt, iterations, &mut key);
    Ok(key)
}

/// Decrypt one native-method payload (KeyV1/FileV1/StringV1 share the same
/// AES-256-GCM scheme; they differ only in payload encoding and iterations).
pub fn native_decrypt(password: &str, envelope_json: &str, iterations: u32) -> Result<Vec<u8>> {
    let env = parse_native_envelope(envelope_json)?;
    let key = derive_key(password, &env.salt, iterations)?;
    let cipher = Aes256Gcm::new((&key).into());
    let nonce = Nonce::from_slice(&env.iv);
    // Joplin uses an empty associated data payload.
    let plain = cipher
        .decrypt(nonce, Payload { msg: &env.ct, aad: &[] })
        .map_err(|_| Error::Crypto("AES-GCM decrypt failed (wrong password or corrupted data)".into()))?;
    Ok(plain)
}

pub fn native_encrypt(password: &str, salt: &[u8], data: &[u8], iterations: u32) -> Result<String> {
    let key = derive_key(password, salt, iterations)?;
    let cipher = Aes256Gcm::new((&key).into());
    let mut iv = [0u8; 12];
    use rand::RngCore;
    rand::rngs::OsRng.fill_bytes(&mut iv);
    let ct = cipher
        .encrypt(Nonce::from_slice(&iv), Payload { msg: data, aad: &[] })
        .map_err(|e| Error::Crypto(format!("AES-GCM encrypt failed: {e}")))?;
    let b64 = base64::engine::general_purpose::STANDARD;
    Ok(serde_json::json!({
        "salt": b64.encode(salt),
        "iv": b64.encode(iv),
        "ct": b64.encode(ct),
    })
    .to_string())
}

// ---------------------------------------------------------------------------
// SJCL methods (AES-CCM)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SjclParams {
    pub iv: Vec<u8>,
    pub salt: Vec<u8>,
    pub iter: u32,
    pub ks: u32,
    pub ts: u32,
    pub ct: Vec<u8>,
    pub mode: String,
    pub cipher: String,
}

pub fn parse_sjcl(json: &str) -> Result<SjclParams> {
    let v: Value = serde_json::from_str(json).map_err(|e| Error::Crypto(format!("bad sjcl json: {e}")))?;
    let get = |key: &str| v.get(key).cloned().unwrap_or(Value::Null);
    let b64 = |val: &Value| -> Result<Vec<u8>> {
        let s = val.as_str().ok_or_else(|| Error::Crypto("sjcl field not a string".into()))?;
        // SJCL uses base64url-ish charset without padding; canonical base64 also decodes it.
        base64::engine::general_purpose::STANDARD
            .decode(s)
            .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(s.trim_end_matches('=')))
            .map_err(|e| Error::Crypto(format!("bad sjcl base64: {e}")))
    };
    let salt_value = get("salt");
    let empty_salt = Value::String(String::new());
    let salt_ref = if salt_value.is_null() { &empty_salt } else { &salt_value };
    Ok(SjclParams {
        iv: b64(&get("iv"))?,
        salt: b64(salt_ref)?,
        iter: get("iter").as_u64().unwrap_or(1000) as u32,
        ks: get("ks").as_u64().unwrap_or(128) as u32,
        ts: get("ts").as_u64().unwrap_or(64) as u32,
        ct: b64(&get("ct"))?,
        mode: get("mode").as_str().unwrap_or("ccm").to_string(),
        cipher: get("cipher").as_str().unwrap_or("aes").to_string(),
    })
}

/// Decrypt an SJCL AES-CCM payload. `plaintext_encoding` describes how the
/// resulting bytes are interpreted by Joplin (utf8 for plain SJCL, latin1 for
/// escape()'d 1a/1b).
pub fn sjcl_ccm_decrypt(password: &str, params: &SjclParams) -> Result<Vec<u8>> {
    if params.mode != "ccm" {
        return Err(Error::Unsupported(format!("sjcl mode {} not supported", params.mode)));
    }
    if params.cipher != "aes" || params.ks != 128 && params.ks != 256 {
        return Err(Error::Crypto(format!("unsupported sjcl cipher {} ks {}", params.cipher, params.ks)));
    }

    let key_len = (params.ks / 8) as usize;
    let mut key = vec![0u8; key_len];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(password.as_bytes(), &params.salt, params.iter, &mut key);

    match params.ks {
        128 => ccm_decrypt_inner::<aes::Aes128>(&key, &params.iv, params.ts as usize, &params.ct),
        256 => ccm_decrypt_inner::<aes::Aes256>(&key, &params.iv, params.ts as usize, &params.ct),
        _ => Err(Error::Crypto(format!("unsupported key size {}", params.ks))),
    }
}
fn ccm_decrypt_inner<A>(key: &[u8], nonce: &[u8], tag_len: usize, ct: &[u8]) -> Result<Vec<u8>>
where
    A: aes::cipher::BlockCipher + aes::cipher::BlockEncrypt + aes::cipher::KeyInit + aes::cipher::BlockSizeUser<BlockSize = aes::cipher::typenum::U16>,
{
    // Hand-rolled CCM (NIST SP 800-38C) against aes::BlockCipher to avoid
    // trait-version mismatches between the ccm crate and aes 0.8.
    // SJCL always uses 13-byte nonces (L = 15 - 13 = 2) and 8-byte tags.
    use aes::cipher::{BlockEncrypt, KeyInit};
    type Block = aes::cipher::generic_array::GenericArray<u8, aes::cipher::typenum::U16>;

    if nonce.len() != 13 {
        return Err(Error::Crypto(format!("unexpected sjcl iv length {} (expected 13)", nonce.len())));
    }
    if tag_len != 8 {
        return Err(Error::Crypto(format!("unexpected sjcl tag length {} (expected 8)", tag_len)));
    }
    if ct.len() < tag_len {
        return Err(Error::Crypto("ciphertext shorter than tag".into()));
    }

    let cipher = A::new_from_slice(key).map_err(|e| Error::Crypto(format!("bad aes key: {e}")))?;
    let (tag, msg) = ct.split_at(tag_len);
    let msg_len = msg.len();

    // B0: flags = (64*(t-2)/2) + (15-nonce_len-1)  -> adata=0, t=8 -> 0x40, L=2
    let mut b0 = [0u8; 16];
    b0[0] = 0x40 | ((tag_len - 2) * 2) as u8 | (15 - nonce.len() - 1) as u8;
    b0[1..1 + nonce.len()].copy_from_slice(nonce);
    let len_bytes = (msg_len as u64).to_be_bytes();
    b0[16 - 2..].copy_from_slice(&len_bytes[6..8]);

    let mut mac_block = Block::from(b0);
    cipher.encrypt_block(&mut mac_block);

    // CBC-MAC over message (no associated data in SJCL default)
    let mut i = 0;
    while i < msg_len {
        let mut block = Block::from([0u8; 16]);
        let n = (msg_len - i).min(16);
        for j in 0..n {
            block[j] ^= msg[i + j] ^ mac_block[j];
        }
        // block = (mac xor msg_chunk)
        let mut cur = [0u8; 16];
        for j in 0..16 {
            cur[j] = mac_block[j] ^ block[j];
        }
        mac_block = Block::from(cur);
        cipher.encrypt_block(&mut mac_block);
        i += n;
    }

    let mut computed_tag = [0u8; 8];
    computed_tag.copy_from_slice(&mac_block.as_slice()[..tag_len]);
    if computed_tag[..] != tag[..] {
        return Err(Error::Crypto("AES-CCM tag mismatch (wrong password or corrupted data)".into()));
    }

    // CTR mode decryption. A_i: flags=1 (L-1=1), nonce, counter i big-endian in last 2 bytes.
    let mut out = Vec::with_capacity(msg_len);
    let mut counter: u64 = 1;
    let mut keystream = Vec::new();
    while keystream.len() < msg_len {
        let mut a = [0u8; 16];
        a[0] = 1;
        a[1..1 + nonce.len()].copy_from_slice(nonce);
        let c = (counter as u64).to_be_bytes();
        a[14..].copy_from_slice(&c[6..8]);
        let mut blk = Block::from(a);
        cipher.encrypt_block(&mut blk);
        keystream.extend_from_slice(blk.as_slice());
        counter += 1;
    }
    for (idx, m) in msg.iter().enumerate() {
        out.push(m ^ keystream[idx]);
    }
    Ok(out)
}

/// SJCL "escape()" equivalent: encodeURIComponent + the %-encoding quirks
/// Joplin relies on for methods 1a/1b.
pub fn sjcl_unescape(s: &str) -> Result<String> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() + 1 {
                return Err(Error::Crypto("bad escape sequence".into()));
            }
            let hex = &s[i + 1..(i + 3).min(s.len())];
            if hex.len() != 2 {
                return Err(Error::Crypto("bad escape hex".into()));
            }
            let byte = u8::from_str_radix(hex, 16).map_err(|_| Error::Crypto("bad escape hex".into()))?;
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    // escape() produces latin1-style bytes for chars >= 256 encoded as %uXXXX.
    // Joplin only round-trips standard escape() so UTF-8 interpretation is correct here.
    String::from_utf8(out).map_err(|_| Error::Crypto("unescape produced invalid utf-8".into()))
}

pub fn sjcl_escape(s: &str) -> String {
    let mut out = String::new();
    for &b in s.as_bytes() {
        let c = b as char;
        let keep = c.is_ascii_alphanumeric() || "@*_+-./".contains(c);
        if keep {
            out.push(c);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        // JED01 + metadata size hex (2 method + 32 id = 34 = 0x22) + method 0a + 32-hex id
        let id = "a1b2c3d4e5f6a7b8a1b2c3d4e5f6a7b8";
        let header = format!("JED01{size}0a{id}", size = "000022");
        let h = decode_header(&header).unwrap();
        assert_eq!(h.method, METHOD_STRING_V1);
        assert_eq!(h.master_key_id, id);
        assert!(decode_header("NOT01").is_err());
    }

    #[test]
    fn escape_roundtrip() {
        assert_eq!(sjcl_unescape(&sjcl_escape("héllo world+./")).unwrap(), "héllo world+./");
    }
}
