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
//
// CUSTODY MODES (bn-safari-sharedworker-cryptokey-dataclone): a CryptoKey is
// NOT structured-cloneable in every context that can host this module.
// Witnessed live on Safari 26.3.1 (WebKit 21623.2.7.111.2): inside a
// SharedWorker, IDB-put of a CryptoKey-bearing record throws DataCloneError
// and IDB-get of one HANGS forever (never settles), so the v1 scheme can
// neither persist NOR read back an identity there — the production node never
// booted. DedicatedWorker and main-thread contexts on the same build are
// unaffected. This module therefore probes ONCE per context (structuredClone
// of a freshly generated non-extractable AES-GCM key — the same serialization
// an IDB write would perform) and picks a custody mode:
//
//   persistent (probe passes): the v1 scheme described above, unchanged — the
//     WK is a non-extractable CryptoKey stored INSIDE the IndexedDB record.
//   memory (probe fails): the WK lives in a module-scope variable for this
//     context's lifetime; IndexedDB carries ONLY {iv, ct} under a SEPARATE v2
//     record id. The v1 record is never read or written in this mode —
//     reading one is exactly the hang being avoided, and leaving an existing
//     one untouched lets a future fixed engine recover that older identity.
//
// The at-rest guarantee is IDENTICAL in both modes: no key material at rest,
// ever. The honest degradation of memory custody is durability, not secrecy:
// the identity survives this context's lifetime (a SharedWorker outlives any
// single page reload — it is the node's own lifetime) but NOT a worker
// restart on affected engines. Callers get `persistence` back and must
// surface the mode in status rather than hide it.

const DB_NAME = 'hive-browser-identity';
const DB_VERSION = 1;
const STORE = 'seeds';
const RECORD_ID = 'current';
// Memory-custody records live under a SEPARATE id and carry only plain
// structured-clone-safe data ({iv, ct} — never a CryptoKey), so reading them
// can never trigger the Safari SharedWorker CryptoKey deserialization hang.
const RECORD_ID_V2 = 'current-v2';
const AAD = new TextEncoder().encode('shadw-node-id-v1');

// One capability probe per context (module-scope memo): can THIS global
// structured-clone a CryptoKey? generateKey alone never throws for this —
// Safari's SharedWorker generates keys happily and only fails at clone/store
// time — so the probe must exercise the clone itself. The clone is also what
// postMessage and IDB-put share, so one probe covers every persistence path.
let custodyPromise = null;
function detectCustody() {
  if (!custodyPromise) {
    custodyPromise = (async () => {
      try {
        if (typeof structuredClone !== 'function') return 'memory';
        const probe = await crypto.subtle.generateKey(
          { name: 'AES-GCM', length: 256 },
          false,
          ['encrypt', 'decrypt']
        );
        structuredClone(probe);
        return 'persistent';
      } catch {
        return 'memory';
      }
    })();
  }
  return custodyPromise;
}

// Memory-custody wrapping key. Module scope = this worker/context's lifetime;
// it is never persisted anywhere — that is the at-rest guarantee this module
// exists for, preserved word-for-word in memory mode.
let memoryWk = null;

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
 * Best-effort persistent-storage request (bn-ui-mobile-lifecycle's
 * permission-flows item): without this, the browser can silently evict
 * this origin's IndexedDB-stored identity record under storage pressure —
 * mobile eviction heuristics are tighter than desktop's — quietly
 * regenerating a "new" node identity on next boot, exactly the continuity
 * this whole module exists to provide. Many browsers auto-grant based on
 * site-engagement heuristics with no visible prompt; some show one either
 * way. Never blocks or fails identity load/creation: an unsupported API or
 * a denied/ignored request just leaves the pre-existing eviction risk
 * unchanged, never regressed.
 */
async function requestPersistentStorage() {
  try {
    if (navigator.storage?.persist) {
      await navigator.storage.persist();
    }
  } catch {
    /* best-effort — never block identity load/creation on this */
  }
}

/**
 * Load the persisted seed (hex) if one exists, else generate a fresh
 * non-extractable wrapping key, wrap `freshSeedHex` under it, persist, and
 * return `freshSeedHex` back. `freshSeedHex` is only used on first boot (the
 * caller gets it from a throwaway `BrowserNode.boot()` call with no seed).
 *
 * Returns `{ seedHex, created, persistence }` — `created` is true the first
 * time a context establishes an identity, false on every subsequent load.
 * `persistence` is 'persistent' (WK held by IndexedDB; identity survives any
 * restart) or 'memory' (WK held only by this context; identity survives this
 * context's lifetime but NOT its restart — the honest degradation a caller
 * must surface, see the header's CUSTODY MODES).
 */
export async function loadOrCreateSeed(freshSeedHex) {
  await requestPersistentStorage();
  const custody = await detectCustody();
  const db = await openDb();
  try {
    if (custody === 'memory') {
      // NEVER read or write the v1 CryptoKey-bearing record in this context:
      // putting one throws DataCloneError and GETting one hangs forever
      // (Safari 26 SharedWorker, witnessed). The v2 record is plain data.
      const existing = await idbGet(db, RECORD_ID_V2);
      if (existing && memoryWk) {
        try {
          const seedBytes = await crypto.subtle.decrypt(
            { name: 'AES-GCM', iv: existing.iv, additionalData: AAD },
            memoryWk,
            existing.ct
          );
          return {
            seedHex: bytesToHex(new Uint8Array(seedBytes)),
            created: false,
            persistence: 'memory',
          };
        } catch {
          // A v2 ciphertext this context's WK cannot open (corruption, or one
          // written by a different context) — fall through and establish a
          // fresh identity rather than fail the boot.
        }
      }
      // First boot in this context, or the context (SharedWorker process) was
      // restarted: any prior v2 ciphertext's WK died with the previous
      // context and is unrecoverable BY DESIGN. This is memory custody's
      // documented degradation, surfaced to the caller via `persistence`.
      memoryWk = await crypto.subtle.generateKey({ name: 'AES-GCM', length: 256 }, false, [
        'encrypt',
        'decrypt',
      ]);
      const iv = crypto.getRandomValues(new Uint8Array(12));
      const ct = await crypto.subtle.encrypt(
        { name: 'AES-GCM', iv, additionalData: AAD },
        memoryWk,
        hexToBytes(freshSeedHex)
      );
      await idbPut(db, { id: RECORD_ID_V2, iv, ct, custody: 'memory', createdMs: Date.now() });
      return { seedHex: freshSeedHex, created: true, persistence: 'memory' };
    }

    const existing = await idbGet(db, RECORD_ID);
    if (existing) {
      const seedBytes = await crypto.subtle.decrypt(
        { name: 'AES-GCM', iv: existing.iv, additionalData: AAD },
        existing.wk,
        existing.ct
      );
      return { seedHex: bytesToHex(new Uint8Array(seedBytes)), created: false, persistence: 'persistent' };
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
    return { seedHex: freshSeedHex, created: true, persistence: 'persistent' };
  } finally {
    db.close();
  }
}

/** Delete the persisted identity record (used by rotation, once the fleet
 * handover for the old id is complete — that handover is NOT this module's
 * job, see docs/browser-node-proposal.md §2.2's rotation flow). In memory
 * custody only the v2 record is touched: an intact v1 record is exactly what
 * a future FIXED engine would recover, and deleting records is untested
 * surface for the same context bug this mode exists to avoid. */
export async function forgetSeed() {
  const custody = await detectCustody();
  const db = await openDb();
  try {
    await idbDelete(db, custody === 'memory' ? RECORD_ID_V2 : RECORD_ID);
  } finally {
    db.close();
  }
}
