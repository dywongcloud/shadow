// Browser-node key persistence (PRD bn-impl-key-persistence).
//
// Design (docs/browser-node-proposal.md §2.2): iroh's SecretKey is raw-bytes
// only (no signer hook exists), so a non-extractable WebCrypto Ed25519 key can
// NEVER feed iroh directly. All ed25519 material stays inside the wasm module;
// WebCrypto is used only as a WRAPPING layer around the 32-byte seed:
//
//   generate (first boot): wasm generates the seed; JS creates a
//     non-extractable AES-GCM-256 key (the "wrapping key", WK) via
//     subtle.generateKey — WK's raw bits never exist in JS-readable form.
//   store: one IndexedDB record { wk: <CryptoKey>, iv, ct, endpointId,
//     createdMs } written in a single transaction. subtle.encrypt (not
//     wrapKey — wrapKey needs an EXTRACTABLE key, and BCD shows no ed25519
//     wrapKey support anyway) with AAD "shadw-node-id-v1" binds the ciphertext
//     to this specific use.
//   load: subtle.decrypt with the same WK + AAD, producing the raw 32-byte
//     seed hex to hand back into BrowserNode.boot(...).
//   rotate: generate a brand-new seed + WK under a NEW record; the caller is
//     responsible for the fleet-side handover (out of scope for this module —
//     see bn-impl-mesh-admission), then this module deletes the old record.
//
// HONEST THREAT MODEL (stated once, here, not implied): this stops (a)
// plaintext-at-rest disclosure of the seed if the IndexedDB file itself is
// read off disk, and (b) off-origin replay of a scraped ciphertext blob (WK
// never leaves this origin's WebCrypto keystore). It does NOT stop live
// same-origin XSS — a script running on this page can call decrypt() itself
// and read the seed straight out of wasm memory. The real XSS levers are a
// strict CSP (script-src 'self' 'wasm-unsafe-eval', no inline/eval) and
// confining identity handling to a dedicated worker, both out of this
// module's scope. Consequence: browser EndpointIds are treated as
// low-privilege everywhere server-side (docs/browser-node-proposal.md §2.8),
// regardless of how well this wrapping holds.

const DB_NAME = 'hive-browser-identity';
const DB_VERSION = 1;
const STORE = 'seeds';
const RECORD_ID = 'current';
const AAD = new TextEncoder().encode('shadw-node-id-v1');

function openDb() {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open(DB_NAME, DB_VERSION);
    req.onupgradeneeded = () => {
      const db = req.result;
      if (!db.objectStoreNames.contains(STORE)) {
        db.createObjectStore(STORE, { keyPath: 'id' });
      }
    };
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}

function idbGet(db, id) {
  return new Promise((resolve, reject) => {
    const tx = db.transaction(STORE, 'readonly');
    const req = tx.objectStore(STORE).get(id);
    req.onsuccess = () => resolve(req.result || null);
    req.onerror = () => reject(req.error);
  });
}

function idbPut(db, record) {
  return new Promise((resolve, reject) => {
    const tx = db.transaction(STORE, 'readwrite');
    tx.objectStore(STORE).put(record);
    tx.oncomplete = () => resolve();
    tx.onerror = () => reject(tx.error);
  });
}

function idbDelete(db, id) {
  return new Promise((resolve, reject) => {
    const tx = db.transaction(STORE, 'readwrite');
    tx.objectStore(STORE).delete(id);
    tx.oncomplete = () => resolve();
    tx.onerror = () => reject(tx.error);
  });
}

function hexToBytes(hex) {
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(hex.substr(i * 2, 2), 16);
  return out;
}

function bytesToHex(bytes) {
  return Array.from(bytes).map((b) => b.toString(16).padStart(2, '0')).join('');
}

/**
 * Load the persisted seed (hex) if one exists, else generate a fresh
 * non-extractable wrapping key, wrap `freshSeedHex` under it, persist, and
 * return `freshSeedHex` back. `freshSeedHex` is only used on first boot (the
 * caller gets it from a throwaway `BrowserNode.boot()` call with no seed).
 *
 * Returns `{ seedHex, created }` — `created` is true the first time an origin
 * establishes an identity, false on every subsequent load.
 */
export async function loadOrCreateSeed(freshSeedHex) {
  const db = await openDb();
  try {
    const existing = await idbGet(db, RECORD_ID);
    if (existing) {
      const seedBytes = await crypto.subtle.decrypt(
        { name: 'AES-GCM', iv: existing.iv, additionalData: AAD },
        existing.wk,
        existing.ct
      );
      return { seedHex: bytesToHex(new Uint8Array(seedBytes)), created: false };
    }

    const wk = await crypto.subtle.generateKey({ name: 'AES-GCM', length: 256 }, false, [
      'encrypt',
      'decrypt',
    ]);
    const iv = crypto.getRandomValues(new Uint8Array(12));
    const seedBytes = hexToBytes(freshSeedHex);
    const ct = await crypto.subtle.encrypt(
      { name: 'AES-GCM', iv, additionalData: AAD },
      wk,
      seedBytes
    );
    await idbPut(db, { id: RECORD_ID, wk, iv, ct, createdMs: Date.now() });
    return { seedHex: freshSeedHex, created: true };
  } finally {
    db.close();
  }
}

/** Delete the persisted identity record (used by rotation, once the fleet
 * handover for the old id is complete — that handover is NOT this module's
 * job, see docs/browser-node-proposal.md §2.2's rotation flow). */
export async function forgetSeed() {
  const db = await openDb();
  try {
    await idbDelete(db, RECORD_ID);
  } finally {
    db.close();
  }
}
