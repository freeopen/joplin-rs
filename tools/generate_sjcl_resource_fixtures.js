#!/usr/bin/env node
// Generates fixtures for LEGACY SJCL-encrypted resource blobs (methods
// SJCL1a/1b/3/4), the attachment format Joplin used before FileV1. It
// replicates EncryptionService.encryptFile for those methods:
//   file bytes -> base64 -> 5000-char chunks -> sjcl.json.encrypt
// (1a/1b additionally escape() the chunk), wrapped in a JED01 chunked
// container exactly like the sync target stores .resource/<id>.
//
// Usage: node generate_sjcl_resource_fixtures.js <output-dir>

const fs = require('fs');
const path = require('path');
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

// encryptFile equivalent: read base64, chunk at 5000 CHARS, emit JED container.
function encryptResource(method, key, masterKeyId, fileBytes) {
  const b64 = fileBytes.toString('base64');
  let out = encodeHeader(method, masterKeyId);
  for (let i = 0; i < b64.length; i += 5000) {
    const block = b64.substring(i, i + 5000);
    const cipher = encryptChunk(method, key, block);
    out += cipher.length.toString(16).padStart(6, '0') + cipher;
  }
  return out;
}

const keyMaterial = 'a'.repeat(512); // valid 256-byte hex key material
const masterKeyId = '0123456789abcdef0123456789abcdef';

// 1x1 PNG (70 bytes, base64 ends in padding '=' -> exercises unescape on 1a/1b)
const png = Buffer.from('89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c4890000000d4944415478da63640000000600023081d02f0000000049454e44ae426082', 'hex');

// Multi-chunk binary: > 3750 raw bytes so base64 exceeds 5000 chars.
const big = Buffer.concat([
  Buffer.from([0x89, 0x50, 0x4e, 0x47]),
  Buffer.from((() => {
    let s = '';
    for (let i = 0; i < 5000; i++) s += String.fromCharCode(i % 256);
    return s;
  })(), 'binary'),
]);

const cases = [];
for (const [label, method, data] of [
  ['png-1a', 5, png],
  ['png-1b', 7, png],
  ['png-3', 3, png],
  ['png-4', 4, png],
  ['multichunk-1a', 5, big],
  ['multichunk-3', 3, big],
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

const outPath = path.join(outDir, 'sjcl_resources.json');
fs.writeFileSync(outPath, JSON.stringify({ cases }, null, 2));
console.log('Wrote', outPath, '-', cases.length, 'cases');
