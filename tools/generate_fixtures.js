#!/usr/bin/env node
// Generates crypto + serialization fixtures for joplin-rs by replicating
// Joplin's EncryptionService and BaseItem.serialize logic using the vendored
// sjcl from the joplin repo and node's webcrypto.
//
// Usage: node generate_fixtures.js <output-dir>
const fs = require('fs');
const path = require('path');
const crypto = require('crypto').webcrypto;
const sjcl = require('/Users/linxh/proj/joplin-dev/packages/lib/vendor/sjcl.js');

const outDir = process.argv[2] || path.join(__dirname, 'fixtures');
fs.mkdirSync(outDir, { recursive: true });

const fixtures = { sjcl: [], native: [], masterKeys: [], items: [] };

// ---- SJCL methods (encrypt with fixed parameters like EncryptionService) ----
function sjclEncrypt(method, key, plainText) {
  if (method === 'SJCL3') {
    return sjcl.json.encrypt(key, plainText, { v: 1, iter: 1000, ks: 128, ts: 64, mode: 'ccm', cipher: 'aes' });
  }
  if (method === 'SJCL4') {
    return sjcl.json.encrypt(key, plainText, { v: 1, iter: 10000, ks: 256, ts: 64, mode: 'ccm', cipher: 'aes' });
  }
  if (method === 'SJCL1a') {
    return sjcl.json.encrypt(key, escape(plainText), { v: 1, iter: 101, ks: 128, ts: 64, mode: 'ccm', cipher: 'aes' });
  }
  if (method === 'SJCL1b') {
    return sjcl.json.encrypt(key, escape(plainText), { v: 1, iter: 101, ks: 256, ts: 64, mode: 'ccm', cipher: 'aes' });
  }
  throw new Error('unknown method ' + method);
}

const sjclMethods = ['SJCL3', 'SJCL4', 'SJCL1a', 'SJCL1b'];
const plainTexts = [
  'Hello world',
  'Unicode: héllo wörld 你好世界 🎉',
  'Multi\nline\nwith escapes \\n and "quotes"',
  '', // empty
  'x'.repeat(100000), // multi-chunk (StringV1 chunk = 64k; SJCL not chunked in single call)
];

for (const method of sjclMethods) {
  for (const pt of plainTexts) {
    const key = 'masterkeyhexvalue' + method;
    const cipher = sjclEncrypt(method, key, pt);
    fixtures.sjcl.push({ method, key, plainText: pt, cipher });
  }
}

// ---- Native methods (KeyV1 / FileV1 / StringV1) via node webcrypto ----
// Replicates packages/lib/services/e2ee/crypto.ts
async function nativeEncrypt(password, salt, data, params) {
  const iv = Buffer.from(crypto.getRandomValues(new Uint8Array(12)));
  const enc = new TextEncoder();
  const keyMaterial = await crypto.subtle.importKey('raw', enc.encode(password), { name: 'PBKDF2' }, false, ['deriveKey']);
  const key = await crypto.subtle.deriveKey(
    { name: 'PBKDF2', salt, iterations: params.iterationCount, hash: 'SHA-512' },
    keyMaterial,
    { name: 'AES-GCM', length: 256 },
    false,
    ['encrypt', 'decrypt'],
  );
  const encrypted = Buffer.from(await crypto.subtle.encrypt({ name: 'AES-GCM', iv, tagLength: 128 }, key, new Uint8Array(data)));
  return {
    salt: Buffer.from(salt).toString('base64'),
    iv: iv.toString('base64'),
    ct: encrypted.toString('base64'),
  };
}

async function main() {
  // Native single-block round-trip data. Replicates Joplin's crypto.ts encodings:
  // - KeyV1: the input is a HEX STRING (master key material); Buffer.from(s, 'hex')
  // - FileV1: the input is a BASE64 STRING (file content); Buffer.from(s, 'base64')
  // - StringV1: the input is the item text itself; Buffer.from(s, 'utf16le')
  for (const [method, mkInput, iter] of [
    ['KeyV1', (pt) => Buffer.from(pt, 'utf8').toString('hex'), 220000],
    ['FileV1', (pt) => Buffer.from(pt, 'utf8').toString('base64'), 3],
    ['StringV1', (pt) => Buffer.from(pt, 'utf16le'), 3],
  ]) {
    for (const pt of plainTexts.slice(0, 4)) {
      const key = 'masterkeyhex' + method;
      const dataBytes = Buffer.from(mkInput(pt));
      const salt = Buffer.from(crypto.getRandomValues(new Uint8Array(32)));
      const env = await nativeEncrypt(key, salt, dataBytes, { iterationCount: iter });
      // expectedPlain is the string that Joplin's EncryptionService.decrypt returns
      // (bufferToString with the method's encoding).
      const expectedPlain = method === 'KeyV1' ? Buffer.from(pt, 'utf8').toString('hex')
        : method === 'FileV1' ? Buffer.from(pt, 'utf8').toString('base64')
        : pt;
      fixtures.native.push({ method, key, plainText: expectedPlain, cipher: JSON.stringify(env), iterations: iter });
    }
  }

  // ---- Master key contents (KeyV1): plainText is 512-hex-char key material ----
  for (let i = 0; i < 2; i++) {
    const keyMaterial = Buffer.from(crypto.getRandomValues(new Uint8Array(256))).toString('hex');
    const password = i === 0 ? 'hunter2' : 'p@ss w0rd!';
    const salt = Buffer.from(crypto.getRandomValues(new Uint8Array(32)));
    const env = await nativeEncrypt(password, salt, Buffer.from(keyMaterial, 'utf8'), { iterationCount: 220000 });
    fixtures.masterKeys.push({ password, keyMaterial, content: JSON.stringify(env) });
  }

  // ---- Full encrypted item blobs (JED01 container, StringV1 chunks) ----
  // Replicates EncryptionService.encryptString with StringV1: chunks of 64k,
  // header JED01 + metadata.
  function encodeHeader(method, masterKeyId) {
    let metadata = method.toString(16).padStart(2, '0') + masterKeyId;
    metadata = metadata.length.toString(16).padStart(6, '0') + metadata;
    return 'JED01' + metadata;
  }

  for (const pt of ['A short secret note', 'x'.repeat(150000), 'Line1\nLine2\nLine3 with émojis 🚀']) {
    const keyMaterial = Buffer.from(crypto.getRandomValues(new Uint8Array(256))).toString('hex');
    const password = 'container-pw';
    const masterKeyId = Buffer.from(crypto.getRandomValues(new Uint8Array(16))).toString('hex');
    const method = 10; // StringV1
    let header = encodeHeader(method, masterKeyId);
    let chunks = '';
    for (let i = 0; i < pt.length; i += 65536) {
      const block = pt.substring(i, i + 65536);
      const dataBytes = Buffer.from(block, 'utf16le');
      const salt = Buffer.from(crypto.getRandomValues(new Uint8Array(32)));
      const env = await nativeEncrypt(keyMaterial, salt, dataBytes, { iterationCount: 3 });
      const cipher = JSON.stringify(env);
      chunks += cipher.length.toString(16).padStart(6, '0') + cipher;
    }
    fixtures.items.push({ password, masterKeyId, keyMaterial, plainText: pt, blob: header + chunks });
  }

  // ---- Item serialization fixtures (BaseItem.serialize format) ----
  fixtures.items.push({
    type: 'serialized-note',
    serialized: [
      'My synced note',
      '',
      'Note body line 1',
      'Line 2 with unicode: héllo 🎉',
      'Line 3',
      '',
      'id: 0f1e2d3c4b5a69788796a5b4c3d2e1f0',
      'parent_id: 00000000000000000000000000000001',
      'created_time: 2024-01-02T03:04:05.678Z',
      'updated_time: 2024-06-07T08:09:10.123Z',
      'is_conflict: 0',
      'is_todo: 0',
      'markup_language: 1',
      'type_: 1',
    ].join('\n'),
    expect: {
      title: 'My synced note',
      body: 'Note body line 1\nLine 2 with unicode: héllo 🎉\nLine 3',
      created_time_ms: 1704164645678,
      updated_time_ms: 1717747750123,
    },
  });

  // Encrypted item as it appears on the sync target (keepKeys + cipher text)
  fixtures.items.push({
    type: 'serialized-encrypted-note',
    serialized: [
      'encryption_cipher_text: JED01000022abcd0123456789abcdef0123456789abcdef0123456789abcdef01234567',
      'encryption_applied: 1',
      'id: 11112222333344445555666677778888',
      'updated_time: 2024-01-02T03:04:05.678Z',
      'type_: 1',
    ].join('\n'),
    expect: {
      encryption_applied: 1,
      updated_time_ms: 1704164645678,
      master_key_id: 'abcd0123456789abcdef0123456789ab',
      method: 10,
    },
  });

  const outPath = path.join(outDir, 'crypto_fixtures.json');
  fs.writeFileSync(outPath, JSON.stringify(fixtures, null, 2));
  console.log('Wrote', outPath);
  console.log('sjcl cases:', fixtures.sjcl.length, 'native:', fixtures.native.length, 'masterKeys:', fixtures.masterKeys.length, 'items:', fixtures.items.length);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
