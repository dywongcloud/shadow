// hive browser-node sqlite worker (bn-impl-sqlite-automerge, Phase A).
//
// A DedicatedWorker hosting the browser's cr-sqlite database:
//   SQLite 3.45.0 + vendored superfly/cr-sqlite v0.17 CRRs, statically
//   linked into ./crsqlite-sync.wasm by ./build-sqlite.sh.
//
// Hosting constraints this file is built around (from the PRD row and
// docs/browser-node-proposal.md §2.5/§2.7 -- do not relax them):
//   * The DB MUST live in a DedicatedWorker. FileSystemSyncAccessHandle
//     (createSyncAccessHandle) is DedicatedWorker-only, and Chrome cannot
//     nest a DedicatedWorker under a SharedWorker, so the SharedWorker /
//     Web-Locks ownership layer (bn-ui-sharedworker-owner) only brokers
//     MessagePorts and liveness -- it never opens the DB itself.
//   * Single-writer: exactly one of these workers per origin owns the DB;
//     that election is the broker's job (Web Locks), not this file's. Other
//     tabs proxy SQL to the holder over MessagePort.
//   * navigator.storage.persist() is requested BEFORE opening OPFS, and the
//     result is reported honestly -- a denied/unknown persistence grant is
//     surfaced, never silently ignored (Safari wipes script-written storage
//     after 7 days idle; a wiped CRR peer is safe by construction but the
//     operator/UI must know durability was lost).
//   * Pool/quota I/O failure is surfaced as a typed open-error with the real
//     DOMException name -- never retried-quietly, never swallowed.
//
// Storage layout: wa-sqlite's AccessHandlePoolVFS -- a fixed pool of OPFS
// sync access handles, no COOP/COEP requirement (the analog of sqlite.org's
// opfs-sahpool VFS, which the official sqlite.org wasm build cannot provide
// here because crsql must be statically linked in).
//
// SQL EXECUTION LAYER (bn-browser-fleet-crr-exchange, hard-won): every SQL
// call goes through SYNCHRONOUS `Module.ccall` primitives -- the exact
// pattern wire-proof-node.mjs runs under Node -- never wa-sqlite's async
// API. Two independent failure modes forced this: (1) the pinned
// `sqlite3.prepare_v2` takes a C-string POINTER, not a JS string, and fails
// opaquely ("not an error") on a JS string; (2) wa-sqlite's async
// (Asyncify) call machinery intermittently never resumes for the extension
// calls under AccessHandlePoolVFS (`SELECT crsql_as_crr(...)` committed its
// work but the continuation never fired). Synchronous ccall has neither
// problem, and the VFS's xRead/xWrite are synchronous by design
// (FileSystemSyncAccessHandle), so nothing needs Asyncify. wa-sqlite is
// used ONLY for what genuinely needs it: VFS registration and open/close.
//
// Message protocol (structured-clone JSON, versioned envelope):
//   -> { proto: 'hive-sqlite-worker/1', id, op: 'open',  db?: string }
//   <- { proto, id, op: 'opened', persisted: true|false|'unknown', quota?, usage? }
//   <- { proto, id, op: 'open-error', name, message }        (terminal, typed)
//   -> { proto, id, op: 'exec', sql }                         (no params -- literal SQL only)
//   <- { proto, id, op: 'rows', rows: [][] }
//   <- { proto, id, op: 'sql-error', name, message }
//   -> { proto, id, op: 'as-crr', table }                    <- { proto, id, op: 'rows', rows: [] }
//   -> { proto, id, op: 'set-ts', ts }        (per-transaction u64 decimal string; see hive_crsql::set_ts)
//                                                                <- { proto, id, op: 'rows', rows: [] }
//   -> { proto, id, op: 'changes-since', since: number }
//   <- { proto, id, op: 'changes', changes: WireChange[] }  (ten-column v0.17 wire, see wire-proof-node.mjs)
//   -> { proto, id, op: 'apply', changes: WireChange[] }
//   <- { proto, id, op: 'applied', count }
//   -- CRR exchange ops (bn-browser-fleet-crr-exchange; HCB1 batches as hex,
//      semantics mirror hive_crsql's seam exactly, see the section below):
//   -> { proto, id, op: 'sync-state' }
//   <- { proto, id, op: 'sync-state', siteId, watermarks: [{siteId, version}] }
//   -> { proto, id, op: 'sync-export', since: {siteHex: version}, maxValueBytes, maxBatchChanges? }
//   <- { proto, id, op: 'sync-batches', batches: [hex], skippedOversized, firstOversized }
//   -> { proto, id, op: 'sync-apply', batches: [hex], maxValueBytes, maxBytes }
//   <- { proto, id, op: 'sync-apply-done', outcomes: [{status: 'applied'|'replay'|'gap'|'value-too-large'|'quota-exceeded', ...}] }
//   -> { proto, id, op: 'wipe' }          (revocation consequence: close + destroy the OPFS association)
//   <- { proto, id, op: 'wiped' }
// Every reply carries the request's id; failures are 'sql-error' replies to
// the failing op, never a thrown-away promise. Ops before a successful
// 'opened' get an immediate 'sql-error' -- the worker never hangs a caller.

import SQLiteESMFactory from './crsqlite-sync.mjs';
import * as SQLite from './wa-sqlite/sqlite-api.js';
import { AccessHandlePoolVFS } from './wa-sqlite/examples/AccessHandlePoolVFS.js';
import {
  encodeBatch, decodeBatch, valPayloadBytes, toHex as hcbToHex, fromHex as hcbFromHex,
} from './hcb1.js';

const PROTO = 'hive-sqlite-worker/1';
const OPFS_DIR = '/hive-crsql'; // AccessHandlePoolVFS owns this whole flat OPFS directory
const DEFAULT_DB = 'main.db';
// The seam's export chunk bound (hive_crsql::DEFAULT_MAX_BATCH_CHANGES) — the
// contract names it so the exchange never invents a second one.
const MAX_BATCH_CHANGES = 256;

const SQLITE_ROW = 100, SQLITE_DONE = 101;
const SQLITE_INTEGER = 1, SQLITE_FLOAT = 2, SQLITE_TEXT = 3, SQLITE_BLOB = 4, SQLITE_NULL = 5;
const SQLITE_TRANSIENT = -1; // sqlite copies the bound value during the call

let module = null; // the emscripten module (raw ccall surface)
let sqlite3 = null; // wa-sqlite Factory handle — VFS registration/open/close ONLY
let db = null;
let vfs = null;
let dbName = null;

function reply(id, op, fields) {
  self.postMessage({ proto: PROTO, id, op, ...fields });
}

function errorReply(id, op, err) {
  // DOMException and SQLiteError both reduce to name/message; keep the typed
  // name -- NotAllowedError vs NotFoundError vs quota errors are different
  // operator stories and the UI must be able to tell them apart.
  reply(id, op, {
    name: String(err?.name ?? 'Error'),
    message: String(err?.message ?? err),
  });
}

// ---- synchronous SQL primitives (see the header note) ----------------------

function ccall(name, argTypes, args, ret = 'number') {
  return module.ccall(name, ret, argTypes, args);
}

function errmsg() {
  return ccall('sqlite3_errmsg', ['number'], [db], 'string');
}

// Literal single-statement SQL with no result rows needed (DDL, scalar
// extension calls, tx control). For row-returning SQL use queryAll.
function execSql(sql) {
  const errPtr = module._malloc(4);
  module.setValue(errPtr, 0, 'i32');
  const rc = ccall('sqlite3_exec', ['number', 'string', 'number', 'number', 'number'], [db, sql, 0, 0, errPtr]);
  if (rc !== 0) {
    const msgPtr = module.getValue(errPtr, 'i32');
    const msg = msgPtr ? module.UTF8ToString(msgPtr) : errmsg();
    module._free(errPtr);
    throw new Error(`sqlite exec rc=${rc}: ${msg} :: ${JSON.stringify(sql)}`);
  }
  module._free(errPtr);
}

function prepareStmt(sql) {
  const out = module._malloc(4);
  module.setValue(out, 0, 'i32');
  const rc = ccall('sqlite3_prepare_v2', ['number', 'string', 'number', 'number', 'number'], [db, sql, -1, out, 0]);
  if (rc !== 0) {
    const msg = errmsg();
    module._free(out);
    throw new Error(`prepare rc=${rc}: ${msg} :: ${JSON.stringify(sql)}`);
  }
  const stmt = module.getValue(out, 'i32');
  module._free(out);
  if (!stmt) throw new Error(`prepare produced no statement: ${JSON.stringify(sql)}`);
  return stmt;
}

function columnValue(stmt, i) {
  const t = ccall('sqlite3_column_type', ['number', 'number'], [stmt, i]);
  if (t === SQLITE_TEXT) return ccall('sqlite3_column_text', ['number', 'number'], [stmt, i], 'string');
  if (t === SQLITE_INTEGER) return ccall('sqlite3_column_int64', ['number', 'number'], [stmt, i]);
  if (t === SQLITE_FLOAT) return ccall('sqlite3_column_double', ['number', 'number'], [stmt, i]);
  if (t === SQLITE_BLOB) {
    const n = ccall('sqlite3_column_bytes', ['number', 'number'], [stmt, i]);
    const p = ccall('sqlite3_column_blob', ['number', 'number'], [stmt, i]);
    return new Uint8Array(module.HEAPU8.buffer, p, n).slice();
  }
  return null; // SQLITE_NULL (and anything unexpected — never fabricate a value)
}

function queryAll(sql, bind) {
  const stmt = prepareStmt(sql);
  const rows = [];
  try {
    bind && bind(stmt);
    let rc;
    while ((rc = ccall('sqlite3_step', ['number'], [stmt])) === SQLITE_ROW) {
      const ncol = ccall('sqlite3_column_count', ['number'], [stmt]);
      const row = [];
      for (let i = 0; i < ncol; i++) row.push(columnValue(stmt, i));
      rows.push(row);
    }
    if (rc !== SQLITE_DONE) throw new Error(`step rc=${rc}: ${JSON.stringify(sql)}`);
  } finally {
    ccall('sqlite3_finalize', ['number'], [stmt]);
  }
  return rows;
}

function bindText(stmt, idx, s) {
  ccall('sqlite3_bind_text', ['number', 'number', 'string', 'number', 'number'], [stmt, idx, s, -1, SQLITE_TRANSIENT]);
}
function bindBlob(stmt, idx, bytes) {
  const p = module._malloc(bytes.length || 1);
  module.HEAPU8.set(bytes, p);
  ccall('sqlite3_bind_blob', ['number', 'number', 'number', 'number', 'number'], [stmt, idx, p, bytes.length, SQLITE_TRANSIENT]);
  module._free(p); // SQLITE_TRANSIENT copied it during bind
}
function bindInt(stmt, idx, v) {
  ccall('sqlite3_bind_int64', ['number', 'number', 'number'], [stmt, idx, v]);
}
function bindDouble(stmt, idx, v) {
  ccall('sqlite3_bind_double', ['number', 'number', 'number'], [stmt, idx, v]);
}
function bindNull(stmt, idx) {
  ccall('sqlite3_bind_null', ['number', 'number'], [stmt, idx]);
}

async function requestPersistence() {
  try {
    if (!navigator?.storage?.persist) return 'unknown';
    return (await navigator.storage.persist()) ? true : false;
  } catch {
    return 'unknown'; // a rejected persist() means unknown, not persisted
  }
}

async function storageEstimate() {
  try {
    if (!navigator?.storage?.estimate) return {};
    const { quota, usage } = await navigator.storage.estimate();
    return { quota, usage };
  } catch {
    return {}; // unknown stays unknown, never fabricated
  }
}

async function open(id, name) {
  const persisted = await requestPersistence();

  try {
    module = await SQLiteESMFactory();
    sqlite3 = SQLite.Factory(module);
    // Throws (NotAllowedError / NotFoundError / InvalidStateError) if OPFS or
    // sync access handles are unavailable, or the pool cannot be provisioned
    // (quota). That failure is the caller's signal to pick another host or
    // surface degraded mode -- forward it typed, do not retry into a
    // half-open pool.
    vfs = new AccessHandlePoolVFS(OPFS_DIR);
    await vfs.isReady;
    sqlite3.vfs_register(vfs, true);
    dbName = name ?? DEFAULT_DB;
    db = await sqlite3.open_v2(dbName);
  } catch (err) {
    db = null;
    errorReply(id, 'open-error', err);
    return;
  }

  const { quota, usage } = await storageEstimate();
  reply(id, 'opened', { persisted, ...(quota !== undefined ? { quota } : {}), ...(usage !== undefined ? { usage } : {}) });
}

async function exec(id, sql) {
  reply(id, 'rows', { rows: queryAll(sql) });
}

// The scalar extension ops (as-crr, set-ts) via the synchronous exec path.
// execSql takes no bind parameters, so call sites interpolate ONLY
// pre-validated values: as-crr's table name is regex-checked to a bare
// identifier first, set-ts's value to decimal digits — no free text ever
// reaches this interpolation. These ops REPLY with an empty 'rows' like
// 'exec' — a silent success is indistinguishable from a hung op to the
// caller (that exact confusion cost a debugging round here).
async function execScalar(id, sql) {
  execSql(sql);
  reply(id, 'rows', { rows: [] });
}

// The ten-column crsql_changes v0.17 wire, vtab declared order -- identical
// shape to hive_crsql::Change and wire-proof-node.mjs's dump (pk/site_id as
// hex, val tagged). Any change here is a wire break against the fleet.
async function changesSince(id, since) {
  const rows = queryAll(
    'SELECT "table","pk","cid","val","col_version","db_version","site_id","cl","seq","ts"' +
    ' FROM crsql_changes WHERE db_version > ?1',
    (stmt) => bindInt(stmt, 1, since | 0));
  const changes = rows.map(([table, pk, cid, val, col_version, db_version, site_id, cl, seq, ts]) => ({
    table,
    pk: toHex(pk),
    cid,
    val: taggedVal(val),
    col_version, db_version,
    site_id: toHex(site_id),
    cl, seq, ts,
  }));
  reply(id, 'changes', { changes });
}

async function applyChanges(id, changes) {
  const stmt = prepareStmt(
    'INSERT INTO crsql_changes ("table","pk","cid","val","col_version","db_version","site_id","cl","seq","ts")' +
    ' VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)');
  let count = 0;
  try {
    for (const c of changes) {
      bindText(stmt, 1, c.table);
      bindBlob(stmt, 2, fromHex(c.pk));
      bindText(stmt, 3, c.cid);
      bindVal(stmt, 4, c.val);
      bindInt(stmt, 5, c.col_version);
      bindInt(stmt, 6, c.db_version);
      bindBlob(stmt, 7, fromHex(c.site_id));
      bindInt(stmt, 8, c.cl);
      bindInt(stmt, 9, c.seq);
      bindText(stmt, 10, c.ts);
      const rc = ccall('sqlite3_step', ['number'], [stmt]);
      if (rc !== SQLITE_DONE) throw new Error(`crsql_changes insert rc=${rc}`);
      count++;
      ccall('sqlite3_reset', ['number'], [stmt]);
    }
  } finally {
    ccall('sqlite3_finalize', ['number'], [stmt]);
  }
  reply(id, 'applied', { count });
}

function toHex(v) {
  if (v instanceof Uint8Array) return Array.from(v, b => b.toString(16).padStart(2, '0')).join('');
  return String(v);
}
function fromHex(s) {
  const out = new Uint8Array(s.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(s.slice(2 * i, 2 * i + 2), 16);
  return out;
}
function taggedVal(v) {
  if (v === null) return { Null: null };
  if (v instanceof Uint8Array) return { Blob: toHex(v) };
  if (typeof v === 'number') return Number.isInteger(v) ? { Integer: v } : { Real: v };
  return { Text: String(v) };
}
function bindVal(stmt, idx, v) {
  if ('Null' in v) return bindNull(stmt, idx);
  if ('Integer' in v) return bindInt(stmt, idx, v.Integer);
  if ('Real' in v) return bindDouble(stmt, idx, v.Real);
  if ('Text' in v) return bindText(stmt, idx, v.Text);
  if ('Blob' in v) return bindBlob(stmt, idx, fromHex(v.Blob));
  throw new Error(`bad wire val ${JSON.stringify(v)}`);
}

// ---- CRR exchange ops (bn-browser-fleet-crr-exchange) ----------------------
// The worker half of the browser↔fleet sync: it owns the OPFS replica and
// speaks HCB1 batches (hcb1.js, byte-identical to hive_crsql's canonical
// form); the page-side glue (sync-client.js) owns the wire transport. The
// semantics here mirror hive_crsql's seam EXACTLY — per-origin-site durable
// watermarks from crsql_db_versions, gap/replay, one transaction per batch —
// this is the second implementation of the same contract, not a new one.

// Wire versions are Numbers at the SQL boundary (bind/row APIs take f64) and
// BigInt inside HCB1 frames; conversions happen at exactly those two seams.
// A db_version past 2^53 is outside anything a browser replica can produce.

function watermarkForSite(siteBytes) {
  const rows = queryAll(
    'SELECT db_version FROM crsql_db_versions WHERE site_id = ?1',
    (stmt) => bindBlob(stmt, 1, siteBytes));
  return rows.length ? Number(rows[0][0]) : 0;
}

function dbBytes() {
  const pages = queryAll('PRAGMA page_count');
  const size = queryAll('PRAGMA page_size');
  return Number(pages[0][0]) * Number(size[0][0]);
}

// -> { proto, id, op: 'sync-state' }
// <- { proto, id, op: 'sync-state', siteId, watermarks: [{siteId, version}] }
async function syncState(id) {
  const siteRows = queryAll('SELECT crsql_site_id()');
  const siteId = siteRows.length ? toHex(siteRows[0][0]) : null;
  const rows = queryAll(
    'SELECT site_id, db_version FROM crsql_db_versions ORDER BY site_id ASC');
  reply(id, 'sync-state', {
    siteId,
    watermarks: rows.map(([site, version]) => ({ siteId: toHex(site), version: Number(version) })),
  });
}

// The worker's taggedVal ({Null}/{Integer}/{Real}/{Text}/{Blob:hex}) -> the
// hcb1 val tag form. Both directions of the conversion stay inside this file.
function hcbVal(v) {
  if ('Null' in v) return { tag: 0 };
  if ('Integer' in v) return { tag: 1, int: BigInt(v.Integer) };
  if ('Real' in v) return { tag: 2, real: v.Real };
  if ('Text' in v) return { tag: 3, text: v.Text };
  return { tag: 4, blob: fromHex(v.Blob) };
}

// -> { proto, id, op: 'sync-export', since: {siteHex: version}, maxValueBytes }
// <- { proto, id, op: 'sync-batches', batches: [hcb1 hex], skippedOversized,
//      firstOversized: {table, pk} | null }
// Per-site bounded export with the seam's chunking (never split one origin
// db_version across batches) and the value cap applied at the EXPORT
// boundary: an oversized value stays local, loudly — never truncated.
async function syncExport(id, since, maxValueBytes, maxBatchChanges) {
  const chunkBound = maxBatchChanges > 0 ? maxBatchChanges : MAX_BATCH_CHANGES;
  const batches = [];
  let skippedOversized = 0;
  let firstOversized = null;
  const sites = queryAll(
    'SELECT site_id, db_version FROM crsql_db_versions ORDER BY site_id ASC');
  for (const [siteBlob, version] of sites) {
    const siteHex = toHex(siteBlob);
    const sinceV = Number(since?.[siteHex] ?? 0);
    if (Number(version) <= sinceV) continue;
    const rows = queryAll(
      'SELECT "table","pk","cid","val","col_version","db_version","site_id","cl","seq","ts"' +
      ' FROM crsql_changes WHERE site_id = ?1 AND db_version > ?2 ORDER BY db_version, seq ASC',
      (stmt) => {
        bindBlob(stmt, 1, siteBlob);
        bindInt(stmt, 2, sinceV);
      });
    const changes = [];
    for (const [table, pk, cid, val, col_version, db_version, site_id, cl, seq, ts] of rows) {
      const change = {
        table, pk, cid,
        val: hcbVal(taggedVal(val)),
        col_version: BigInt(col_version), db_version: BigInt(db_version),
        site_id, cl: BigInt(cl), seq: BigInt(seq), ts,
      };
      if (valPayloadBytes(change.val) > maxValueBytes) {
        skippedOversized++;
        if (!firstOversized) firstOversized = { table, pk: toHex(pk) };
        continue;
      }
      changes.push(change);
    }
    let cur = [];
    let batchSince = BigInt(sinceV);
    let groupEndV = null;
    const close = () => {
      if (!cur.length) return;
      batches.push(hcbToHex(encodeBatch({ site_id: siteBlob, since_db_version: batchSince, changes: cur })));
      batchSince = cur[cur.length - 1].db_version;
      cur = [];
    };
    for (const c of changes) {
      const newGroup = groupEndV !== c.db_version;
      if (newGroup && cur.length > 0 && cur.length >= chunkBound) close();
      groupEndV = c.db_version;
      cur.push(c);
    }
    close();
  }
  reply(id, 'sync-batches', { batches, skippedOversized, firstOversized });
}

// -> { proto, id, op: 'sync-apply', batches: [hcb1 hex], maxValueBytes, maxBytes }
// <- { proto, id, op: 'sync-apply-done', outcomes: [...] }
// The seam's apply_batch semantics, one transaction per batch: watermark
// read, gap/replay checks, capped inserts, COMMIT — any failure ROLLBACKs the
// whole batch (never a half-applied batch, never a truncation). Stops at the
// first non-ok outcome; the caller re-requests from its durable watermarks.
async function syncApply(id, batchesHex, maxValueBytes, maxBytes) {
  const outcomes = [];
  for (const hex of batchesHex ?? []) {
    const batch = decodeBatch(hcbFromHex(hex));
    const outcome = applyOneBatch(batch, maxValueBytes, maxBytes);
    outcomes.push(outcome);
    if (outcome.status !== 'applied' && outcome.status !== 'replay') break;
  }
  reply(id, 'sync-apply-done', { outcomes });
}

function applyOneBatch(batch, maxValueBytes, maxBytes) {
  const siteHex = toHex(batch.site_id);
  if (!batch.site_id.length) throw new Error('batch site_id must be non-empty');
  for (const c of batch.changes) {
    if (toHex(c.site_id) !== siteHex)
      throw new Error('mixed-site batch refused: change site differs from batch site');
  }
  execSql('BEGIN IMMEDIATE');
  const rollback = () => { try { execSql('ROLLBACK'); } catch { /* already out */ } };
  try {
    const w = watermarkForSite(batch.site_id);
    const batchMax = batch.changes.length
      ? batch.changes.reduce((m, c) => (c.db_version > m ? c.db_version : m), batch.changes[0].db_version)
      : null;
    const since = Number(batch.since_db_version);
    if (batchMax === null || Number(batchMax) <= w) {
      if (since > w) {
        rollback();
        return { status: 'gap', siteId: siteHex, watermark: w, batchSince: since };
      }
      rollback(); // nothing to write; replay is a deterministic no-op
      return { status: 'replay', siteId: siteHex, at: w, skipped: batch.changes.length };
    }
    if (since > w) {
      rollback();
      return { status: 'gap', siteId: siteHex, watermark: w, batchSince: since };
    }
    for (const c of batch.changes) {
      if (valPayloadBytes(c.val) > maxValueBytes) {
        rollback();
        return { status: 'value-too-large', siteId: siteHex, table: c.table, pk: toHex(c.pk), maxValueBytes };
      }
    }
    const size = dbBytes();
    const estimate = batch.changes.reduce((n, c) => n + valPayloadBytes(c.val) + 64, 0);
    if (size >= maxBytes || size + estimate > maxBytes) {
      rollback();
      return { status: 'quota-exceeded', siteId: siteHex, size, maxBytes };
    }
    const stmt = prepareStmt(
      'INSERT INTO crsql_changes ("table","pk","cid","val","col_version","db_version","site_id","cl","seq","ts")' +
      ' VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)');
    let applied = 0;
    try {
      for (const c of batch.changes) {
        if (Number(c.db_version) <= w) continue; // overlap prefix: already merged
        bindText(stmt, 1, c.table);
        bindBlob(stmt, 2, c.pk);
        bindText(stmt, 3, c.cid);
        bindHcbVal(stmt, 4, c.val);
        bindInt(stmt, 5, Number(c.col_version));
        bindInt(stmt, 6, Number(c.db_version));
        bindBlob(stmt, 7, c.site_id);
        bindInt(stmt, 8, Number(c.cl));
        bindInt(stmt, 9, Number(c.seq));
        bindText(stmt, 10, c.ts);
        const rc = ccall('sqlite3_step', ['number'], [stmt]);
        if (rc !== SQLITE_DONE) throw new Error(`crsql_changes insert rc=${rc}`);
        applied++;
        ccall('sqlite3_reset', ['number'], [stmt]);
      }
    } finally {
      ccall('sqlite3_finalize', ['number'], [stmt]);
    }
    const to = watermarkForSite(batch.site_id);
    execSql('COMMIT');
    return { status: 'applied', siteId: siteHex, from: w, to, count: applied };
  } catch (err) {
    rollback();
    throw err;
  }
}

function bindHcbVal(stmt, idx, v) {
  if (v.tag === 0) return bindNull(stmt, idx);
  if (v.tag === 1) return bindInt(stmt, idx, Number(v.int));
  if (v.tag === 2) return bindDouble(stmt, idx, v.real);
  if (v.tag === 3) return bindText(stmt, idx, v.text);
  return bindBlob(stmt, idx, v.blob);
}

// -> { proto, id, op: 'wipe' }
// <- { proto, id, op: 'wiped' }
// The contract's revocation consequence (§5): close the replica and destroy
// its OPFS association (the VFS's own xDelete — the same call SQLite itself
// makes to delete a database). After this the replica is GONE: a later
// 'open' starts from an empty file with a fresh site id (safe by
// construction — a wiped CRR peer re-syncs from watermark 0).
async function wipe(id) {
  if (db) {
    try { ccall('sqlite3_close_v2', ['number'], [db]); } catch { /* closing is best-effort here */ }
    db = null;
  }
  if (vfs && dbName) vfs.xDelete(dbName, 1);
  // Release the pool's sync access handles too: a later 'open' builds a NEW
  // pool, and Chrome refuses createSyncAccessHandle while ANY earlier handle
  // on the same file is still open (NoModificationAllowedError).
  if (vfs) {
    try { await vfs.close(); } catch { /* best-effort */ }
    vfs = null;
  }
  reply(id, 'wiped', {});
}

self.onmessage = (ev) => {
  const msg = ev.data;
  if (!msg || msg.proto !== PROTO || typeof msg.id === 'undefined') return; // not ours
  const { id } = msg;

  // Everything after construction needs an open DB; refuse fast and loud.
  if (msg.op !== 'open' && !db) {
    errorReply(id, 'sql-error', new Error('database is not open (see open-error status)'));
    return;
  }

  const run = async () => {
    switch (msg.op) {
      case 'open': return open(id, msg.db);
      case 'exec': return exec(id, msg.sql);
      case 'as-crr':
        if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(msg.table ?? ''))
          throw new Error(`as-crr: refusing non-identifier table name ${JSON.stringify(msg.table)}`);
        return execScalar(id, `SELECT crsql_as_crr('${msg.table}')`);
      case 'set-ts':
        if (!/^[0-9]+$/.test(String(msg.ts ?? '')))
          throw new Error(`set-ts: refusing non-decimal ts ${JSON.stringify(msg.ts)}`);
        return execScalar(id, `SELECT crsql_set_ts('${String(msg.ts)}')`);
      case 'changes-since': return changesSince(id, msg.since ?? 0);
      case 'apply': return applyChanges(id, msg.changes ?? []);
      case 'sync-state': return syncState(id);
      case 'sync-export': return syncExport(id, msg.since ?? {}, msg.maxValueBytes ?? (1 << 20), msg.maxBatchChanges ?? 0);
      case 'sync-apply': return syncApply(id, msg.batches ?? [], msg.maxValueBytes ?? (1 << 20), msg.maxBytes ?? (64 << 20));
      case 'wipe': return wipe(id);
      default: throw new Error(`unknown op ${JSON.stringify(msg.op)}`);
    }
  };
  run().catch((err) => errorReply(id, 'sql-error', err));
};
