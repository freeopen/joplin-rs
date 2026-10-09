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

/// Decrypt an SJCL AES-CCM payload. Mirrors sjcl.mode.ccm: the stored 16-byte
/// iv is truncated to a (15-L)-byte nonce where L is the byte length of the
/// message length field (2..4, chosen from the plaintext size).
pub fn sjcl_ccm_decrypt(password: &str, params: &SjclParams) -> Result<Vec<u8>> {
    if params.mode != "ccm" {
        return Err(Error::Unsupported(format!("sjcl mode {} not supported", params.mode)));
    }
    if params.cipher != "aes" || (params.ks != 128 && params.ks != 256) {
        return Err(Error::Crypto(format!("unsupported sjcl cipher {} ks {}", params.cipher, params.ks)));
    }

    let key_len = (params.ks / 8) as usize;
    let mut key = vec![0u8; key_len];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(password.as_bytes(), &params.salt, params.iter, &mut key);

    // sjcl.mode.ccm.encrypt: iv must be >= 7 bytes; L (called f) = smallest
    // number of bytes needed for the message length, then clamped up so the
    // nonce is at most 15 bytes. With a 16-byte stored iv, 15-iv_len < 2, so
    // the clamp never applies and f is purely message-length driven.
    let msg_len = params.ct.len() - (params.ts as usize / 8);
    let f = {
        let mut f = 2usize;
        while f < 4 && (msg_len >> (8 * f)) != 0 {
            f += 1;
        }
        f
    };
    let nonce_len = 15 - f;
    if params.iv.len() < nonce_len {
        return Err(Error::Crypto(format!("sjcl iv too short: {} < {nonce_len}", params.iv.len())));
    }
    let nonce = &params.iv[..nonce_len];

    let tag_len = (params.ts / 8) as usize;
    match params.ks {
        128 => ccm_decrypt_inner::<aes::Aes128>(&key, nonce, tag_len, &params.ct),
        256 => ccm_decrypt_inner::<aes::Aes256>(&key, nonce, tag_len, &params.ct),
        _ => Err(Error::Crypto(format!("unsupported key size {}", params.ks))),
    }
}
fn ccm_decrypt_inner<A>(key: &[u8], nonce: &[u8], tag_len: usize, ct: &[u8]) -> Result<Vec<u8>>
where
    A: aes::cipher::BlockCipher + aes::cipher::BlockEncrypt + aes::cipher::KeyInit + aes::cipher::BlockSizeUser<BlockSize = aes::cipher::typenum::U16>,
{
    // SJCL CCM (NOT standard CCM). Mirrors sjcl.mode.ccm.decrypt:
    // 1. data = CTR-decrypt(msg) with keystream blocks E((L-1) || nonce || counter) for counter = 1..
    // 2. mac = CBC-MAC over B0 = ((t-2)<<2)|(L-1) || nonce || msg_len_be(L bytes), then the PLAINTEXT
    // 3. expected stored tag = mac XOR E((L-1) || nonce || counter=0)[..tag_len]
    // 4. tag is APPENDED to the ciphertext
    use aes::cipher::{BlockEncrypt, KeyInit};
    type Block = aes::cipher::generic_array::GenericArray<u8, aes::cipher::typenum::U16>;

    let l = 15 - nonce.len(); // length field size in bytes
    if l < 2 || l > 8 {
        return Err(Error::Crypto(format!("invalid ccm L {l}")));
    }
    if ct.len() < tag_len {
        return Err(Error::Crypto("ciphertext shorter than tag".into()));
    }

    let cipher = A::new_from_slice(key).map_err(|e| Error::Crypto(format!("bad aes key: {e}")))?;
    let msg_len = ct.len() - tag_len;
    let (msg, tag) = ct.split_at(msg_len);

    let ctr_block = |counter: u64| -> Block {
        let mut a = [0u8; 16];
        a[0] = (l - 1) as u8;
        a[1..1 + nonce.len()].copy_from_slice(nonce);
        let c = counter.to_be_bytes();
        a[16 - l..].copy_from_slice(&c[8 - l..]);
        Block::clone_from_slice(&a)
    };

    // 1. CTR decryption, counters starting at 1.
    let mut plaintext = Vec::with_capacity(msg_len);
    let mut keystream = Vec::new();
    let mut counter: u64 = 1;
    while keystream.len() < msg_len {
        let mut blk = ctr_block(counter);
        cipher.encrypt_block(&mut blk);
        keystream.extend_from_slice(blk.as_slice());
        counter += 1;
    }
    for (idx, m) in msg.iter().enumerate() {
        plaintext.push(m ^ keystream[idx]);
    }

    // 2. CBC-MAC over B0 + plaintext.
    let mut b0 = [0u8; 16];
    b0[0] = (((tag_len - 2) << 2) | (l - 1)) as u8;
    b0[1..1 + nonce.len()].copy_from_slice(nonce);
    let len_bytes = (msg_len as u64).to_be_bytes();
    b0[16 - l..].copy_from_slice(&len_bytes[8 - l..]);
    let mut mac = Block::clone_from_slice(&b0);
    cipher.encrypt_block(&mut mac);
    let mut i = 0;
    while i < msg_len {
        let n = (msg_len - i).min(16);
        let mut cur = [0u8; 16];
        for j in 0..n {
            cur[j] = mac[j] ^ plaintext[i + j];
        }
        for j in n..16 {
            cur[j] = mac[j];
        }
        mac = Block::clone_from_slice(&cur);
        cipher.encrypt_block(&mut mac);
        i += n;
    }

    // 3. Unmask and compare: stored_tag == mac XOR s0[..tag_len].
    let mut s0 = ctr_block(0);
    cipher.encrypt_block(&mut s0);
    for idx in 0..tag_len {
        if mac[idx] ^ s0[idx] != tag[idx] {
            return Err(Error::Crypto("AES-CCM tag mismatch (wrong password or corrupted data)".into()));
        }
    }
    Ok(plaintext)
}

#[doc(hidden)]
pub fn debug_ccm_trace(key: &[u8], nonce: &[u8], tag_len: usize, ct: &[u8]) -> Vec<String> {
    let mut log = Vec::new();
    let l = 15 - nonce.len();
    log.push(format!("l={l} ct_len={} tag_len={tag_len}", ct.len()));
    let msg_len = ct.len() - tag_len;
    let (msg, tag) = ct.split_at(msg_len);
    log.push(format!("msg={} tag={}", hex::encode(msg), hex::encode(tag)));
    use aes::cipher::{BlockEncrypt, KeyInit};
    type Block = aes::cipher::generic_array::GenericArray<u8, aes::cipher::typenum::U16>;
    let cipher = aes::Aes128::new_from_slice(key).unwrap();
    let ctr_block = |counter: u64| -> Block {
        let mut a = [0u8; 16];
        a[0] = (l - 1) as u8;
        a[1..1 + nonce.len()].copy_from_slice(nonce);
        let c = counter.to_be_bytes();
        a[16 - l..].copy_from_slice(&c[8 - l..]);
        Block::clone_from_slice(&a)
    };
    let mut s0 = ctr_block(0);
    cipher.encrypt_block(&mut s0);
    log.push(format!("s0={}", hex::encode(&s0)));
    let mut b0 = [0u8; 16];
    b0[0] = (((tag_len - 2) << 2) | (l - 1)) as u8;
    b0[1..1 + nonce.len()].copy_from_slice(nonce);
    let len_bytes = (msg_len as u64).to_be_bytes();
    b0[16 - l..].copy_from_slice(&len_bytes[8 - l..]);
    log.push(format!("b0={}", hex::encode(b0)));
    let mut mac = Block::clone_from_slice(&b0);
    cipher.encrypt_block(&mut mac);
    log.push(format!("e_b0={}", hex::encode(&mac)));
    let mut i = 0;
    while i < msg_len {
        let n = (msg_len - i).min(16);
        let mut cur = [0u8; 16];
        for j in 0..n { cur[j] = mac[j] ^ msg[i + j]; }
        for j in n..16 { cur[j] = mac[j]; }
        mac = Block::clone_from_slice(&cur);
        cipher.encrypt_block(&mut mac);
        i += n;
    }
    log.push(format!("mac={}", hex::encode(&mac)));
    let masked: Vec<u8> = (0..tag_len).map(|idx| mac[idx] ^ s0[idx]).collect();
    log.push(format!("masked={}", hex::encode(&masked)));
    log
}

/// JS escape(): keeps alphanumerics and @*_+-./; encodes each UTF-16 code unit
/// as %XX (<= 255) or %uXXXX (> 255). Mirrors Joplin's escape(plainText).
pub fn sjcl_escape(s: &str) -> String {
    let mut out = String::new();
    for unit in s.encode_utf16() {
        if unit < 128 {
            let c = unit as u8 as char;
            if c.is_ascii_alphanumeric() || "@*_+-./".contains(c) {
                out.push(c);
            } else {
                out.push_str(&format!("%{unit:02X}"));
            }
        } else if unit < 256 {
            out.push_str(&format!("%{unit:02X}"));
        } else {
            out.push_str(&format!("%u{unit:04X}"));
        }
    }
    out
}

/// Reverse of JS unescape(): %uXXXX -> U+XXXX char, %XX -> U+00XX char,
/// everything else literal. UTF-16 surrogate pairs (%uD83C%uDF89) are combined.
pub fn sjcl_unescape(s: &str) -> Result<String> {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && bytes[i + 1] == b'u'
            && i + 5 < bytes.len() + 1
            && i + 6 <= bytes.len()
        {
            // %uXXXX (with surrogate pair combining)
            let hex_str = std::str::from_utf8(&bytes[i + 2..i + 6]).map_err(|_| Error::Crypto("bad escape".into()))?;
            if let Ok(unit) = u32::from_str_radix(hex_str, 16) {
                if (0xD800..0xDC00).contains(&unit) {
                    // High surrogate: try to consume a following %uXXXX low surrogate.
                    if i + 12 <= bytes.len() && bytes[i + 6] == b'%' && bytes[i + 7] == b'u' {
                        let low_hex = std::str::from_utf8(&bytes[i + 8..i + 12]).map_err(|_| Error::Crypto("bad escape".into()))?;
                        if let Ok(low) = u32::from_str_radix(low_hex, 16) {
                            if (0xDC00..0xE000).contains(&low) {
                                let cp = 0x10000 + ((unit - 0xD800) << 10) + (low - 0xDC00);
                                if let Some(ch) = char::from_u32(cp) {
                                    out.push(ch);
                                    i += 12;
                                    continue;
                                }
                            }
                        }
                    }
                    // Unpaired surrogate: replacement char.
                    out.push('\u{FFFD}');
                    i += 6;
                    continue;
                }
                if let Some(ch) = char::from_u32(unit) {
                    out.push(ch);
                    i += 6;
                    continue;
                }
            }
            out.push('%');
            i += 1;
        } else if bytes[i] == b'%' && i + 2 < bytes.len() + 1 && i + 3 <= bytes.len() {
            let hex_str = std::str::from_utf8(&bytes[i + 1..i + 3]).map_err(|_| Error::Crypto("bad escape".into()))?;
            match u8::from_str_radix(hex_str, 16) {
                Ok(byte) => {
                    // JS unescape maps %XX to char U+00XX (latin-1 range).
                    out.push(byte as char);
                    i += 3;
                }
                Err(_) => {
                    out.push('%');
                    i += 1;
                }
            }
        } else {
            // Copy as a UTF-8 sequence (multi-byte chars pass through unescaped).
            let ch_len = utf8_len(bytes[i]);
            let end = (i + ch_len).min(bytes.len());
            out.push_str(std::str::from_utf8(&bytes[i..end]).map_err(|_| Error::Crypto("bad utf8".into()))?);
            i = end;
        }
    }
    Ok(out)
}

fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else if b >> 3 == 0b11110 {
        4
    } else {
        1
    }
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
        let original = "héllo wörld 你好世界 🎉 +./@*_";
        let unescaped = sjcl_unescape(&sjcl_escape(original)).unwrap();
        assert_eq!(unescaped, original);
    }
}
