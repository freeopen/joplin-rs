#!/usr/bin/env node
// Generates fixtures for LEGACY SJCL-encrypted resource blobs (methods
// SJCL1a/1b/3/4), the attachment format Joplin used before FileV1. It
// replicates EncryptionService.encryptFile for those methods:
//   file bytes -> base64 -> 5000-char chunks -> sjcl.json.encrypt
// (1a/1b additionally escape() the chunk), wrapped in a JED01 chunked
// container exactly like the sync target stores .resource/<id>.
//
// Usage: node generate_resource_fixtures.js <output-dir>

const fs = require('fs');
const path = require('path');
const crypto = require('crypto').webcrypto;
const sjcl = require('/Users/linxh/proj/joplin-dev/packages/lib/vendor/sjcl.js');

const outDir = process.argv[2] || path.join(__dirname, '..', 'fixtures');
fs.mkdirSync(outDir, { recursive: true });

function encodeHeader(method, masterKeyId) {
  let metadata = method.toString(16).padStart(2, '0') + masterKeyId;
  metadata = metadata.length.toString(16).padStart(6, '0') + metadata;
  return 'JED01' + metadata;
}

// Mirror EncryptionService.encrypt() SJCL parameter sets.
function encryptChunk(method, key, block) {
  if (method === 3) {
    return sjcl.json.encrypt(key, block, { v: 1, iter: 1000, ks: 128, ts: 64, mode: 'ccm', cipher: 'aes' });
  }
  if (method === 4) {
    return sjcl.json.encrypt(key, block, { v: 1, iter: 10000, ks: 256, ts: 64, mode: 'ccm', cipher: 'aes' });
  }
  if (method === 5) {
    return sjcl.json.encrypt(key, escape(block), { v: 1, iter: 101, ks: 128, ts: 64, mode: 'ccm', cipher: 'aes' });
  }
  if (method === 7) {
    return sjcl.json.encrypt(key, escape(block), { v: 1, iter: 101, ks: 256, ts: 64, mode: 'ccm', cipher: 'aes' });
  }
  throw new Error('unsupported method ' + method);
}

// encryptFile equivalent: readFileChunk reads `chunkBytes` RAW bytes and
// base64-encodes EACH block independently (so every block but the last has
// its own '=' padding); that per-block base64 string is what gets encrypted.
// Chunk sizes: SJCL variants = 5000 raw bytes, FileV1 = 131072.
function encryptResource(method, key, masterKeyId, fileBytes, chunkBytes = 5000) {
  let out = encodeHeader(method, masterKeyId);
  for (let i = 0; i < fileBytes.length; i += chunkBytes) {
    const block = fileBytes.subarray(i, i + chunkBytes);
    const b64 = block.toString('base64'); // independently padded
    const cipher = encryptChunk(method, key, b64);
    out += cipher.length.toString(16).padStart(6, '0') + cipher;
  }
  return out;
}

// Native FileV1 chunk: AES-256-GCM, PBKDF2 SHA-512, 3 iterations (crypto.ts).
async function nativeFileV1Chunk(key, b64block) {
  const enc = new TextEncoder();
  const salt = crypto.getRandomValues(new Uint8Array(32));
  const iv = crypto.getRandomValues(new Uint8Array(12));
  const keyMaterial = await crypto.subtle.importKey('raw', enc.encode(key), { name: 'PBKDF2' }, false, ['deriveKey']);
  const aesKey = await crypto.subtle.deriveKey(
    { name: 'PBKDF2', salt, iterations: 3, hash: 'SHA-512' },
    keyMaterial,
    { name: 'AES-GCM', length: 256 },
    false,
    ['encrypt'],
  );
  const ct = await crypto.subtle.encrypt(
    { name: 'AES-GCM', iv: Buffer.from(iv), tagLength: 128 },
    aesKey,
    new Uint8Array(Buffer.from(b64block, 'binary')),
  );
  return JSON.stringify({
    salt: Buffer.from(salt).toString('base64'),
    iv: Buffer.from(iv).toString('base64'),
    ct: Buffer.from(ct).toString('base64'),
  });
}

async function encryptResourceFileV1(key, masterKeyId, fileBytes) {
  let out = encodeHeader(9, masterKeyId);
  for (let i = 0; i < fileBytes.length; i += 131072) {
    const b64 = fileBytes.subarray(i, i + 131072).toString('base64');
    const cipher = await nativeFileV1Chunk(key, b64);
    out += cipher.length.toString(16).padStart(6, '0') + cipher;
  }
  return out;
}

async function main() {
const keyMaterial = 'a'.repeat(512); // valid 256-byte hex key material
const masterKeyId = '0123456789abcdef0123456789abcdef';

// 1x1 PNG (70 bytes, base64 ends in padding '=' -> exercises unescape on 1a/1b)
const png = Buffer.from('89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c4890000000d4944415478da63640000000600023081d02f0000000049454e44ae426082', 'hex');

// Multi-chunk binary: > 5000 raw bytes so there is an interior base64
// padding boundary (5000 raw bytes is not a multiple of 3 -> first block's
// 6668-char base64 ends in '=' at offset 6667, the exact field failure).
const big = Buffer.concat([
  Buffer.from([0x89, 0x50, 0x4e, 0x47]),
  Buffer.from((() => {
    let s = '';
    for (let i = 0; i < 5000; i++) s += String.fromCharCode(i % 256);
    return s;
  })(), 'binary'),
]);

// Sizes chosen to exercise base64 padding edges around the 5000-byte block.
function sized(n) {
  return Buffer.from((() => {
    let s = '';
    for (let i = 0; i < n; i++) s += String.fromCharCode((i * 7 + 3) % 256);
    return s;
  })(), 'binary');
}

const cases = [];
for (const [label, method, data] of [
  ['png-1a', 5, png],
  ['png-1b', 7, png],
  ['png-3', 3, png],
  ['png-4', 4, png],
  ['multichunk-1a', 5, big],
  ['multichunk-3', 3, big],
  ['exact5000-1a', 5, sized(5000)],
  ['over5001-1a', 5, sized(5001)],
]) {
  cases.push({
    label,
    method,
    keyMaterial,
    masterKeyId,
    blob: encryptResource(method, keyMaterial, masterKeyId, data),
    expectedHex: data.toString('hex'),
  });
}

// FileV1 (method 9): single-chunk PNG plus a binary one byte over the
// 131072-byte block size, spanning two raw blocks whose first block's
// independently-padded base64 ends in '==' (interior padding boundary).
const fileV1Multi = sized(131073);
for (const [label, data] of [
  ['png-filev1', png],
  ['multichunk-filev1', fileV1Multi],
]) {
  cases.push({
    label,
    method: 9,
    keyMaterial,
    masterKeyId,
    blob: await encryptResourceFileV1(keyMaterial, masterKeyId, data),
    expectedHex: data.toString('hex'),
  });
}

const outPath = path.join(outDir, 'resource_blobs.json');
fs.writeFileSync(outPath, JSON.stringify({ cases }, null, 2));
console.log('Wrote', outPath, '-', cases.length, 'cases');
}

main().catch((e) => { console.error(e); process.exit(1); });
