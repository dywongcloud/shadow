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
// Message protocol (structured-clone JSON, versioned envelope):
//   -> { proto: 'hive-sqlite-worker/1', id, op: 'open',  db?: string }
//   <- { proto, id, op: 'opened', persisted: true|false|'unknown', quota?, usage? }
//   <- { proto, id, op: 'open-error', name, message }        (terminal, typed)
//   -> { proto, id, op: 'exec', sql }                         (no params -- literal SQL only)
//   <- { proto, id, op: 'rows', rows: [][] }
//   <- { proto, id, op: 'sql-error', name, message }
//   -> { proto, id, op: 'as-crr', table }
//   -> { proto, id, op: 'set-ts', ts }        (per-transaction u64 decimal string; see hive_crsql::set_ts)
//   -> { proto, id, op: 'changes-since', since: number }
//   <- { proto, id, op: 'changes', changes: WireChange[] }  (ten-column v0.17 wire, see wire-proof-node.mjs)
//   -> { proto, id, op: 'apply', changes: WireChange[] }
//   <- { proto, id, op: 'applied', count }
// Every reply carries the request's id; failures are 'sql-error' replies to
// the failing op, never a thrown-away promise. Ops before a successful
// 'opened' get an immediate 'sql-error' -- the worker never hangs a caller.

import SQLiteESMFactory from './crsqlite-sync.mjs';
import * as SQLite from './wa-sqlite/sqlite-api.js';
import { AccessHandlePoolVFS } from './wa-sqlite/examples/AccessHandlePoolVFS.js';

const PROTO = 'hive-sqlite-worker/1';
const OPFS_DIR = '/hive-crsql'; // AccessHandlePoolVFS owns this whole flat OPFS directory
const DEFAULT_DB = 'main.db';

let sqlite3 = null;
let db = null;

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

async function open(id, dbName) {
  const persisted = await requestPersistence();

  let module, vfs;
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
    db = await sqlite3.open_v2(dbName ?? DEFAULT_DB);
  } catch (err) {
    db = null;
    errorReply(id, 'open-error', err);
    return;
  }

  const { quota, usage } = await storageEstimate();
  reply(id, 'opened', { persisted, ...(quota !== undefined ? { quota } : {}), ...(usage !== undefined ? { usage } : {}) });
}

async function exec(id, sql) {
  const rows = [];
  await sqlite3.exec(db, sql, (row) => {
    rows.push(row);
  });
  reply(id, 'rows', { rows });
}

// Prepare/bind/step-to-DONE/finalize for the scalar SQL ops (as-crr, set-ts)
// -- the pinned API's exec() takes no bind parameters.
async function execScalar(sql, bind) {
  const prepared = await sqlite3.prepare_v2(db, sql);
  if (!prepared) throw new Error(`prepare produced no statement: ${JSON.stringify(sql)}`);
  const stmt = prepared.stmt;
  try {
    bind && (await bind(stmt));
    let rc;
    while ((rc = await sqlite3.step(stmt)) === SQLite.SQLITE_ROW) { /* discard result rows */ }
    if (rc !== SQLite.SQLITE_DONE) throw new Error(`step rc=${rc}: ${JSON.stringify(sql)}`);
  } finally {
    await sqlite3.finalize(stmt);
  }
}

// The ten-column crsql_changes v0.17 wire, vtab declared order -- identical
// shape to hive_crsql::Change and wire-proof-node.mjs's dump (pk/site_id as
// hex, val tagged). Any change here is a wire break against the fleet.
async function changesSince(id, since) {
  const changes = [];
  const prepared = await sqlite3.prepare_v2(
    db,
    'SELECT "table","pk","cid","val","col_version","db_version","site_id","cl","seq","ts"' +
    ' FROM crsql_changes WHERE db_version > ?1');
  if (!prepared) throw new Error('prepare produced no statement for crsql_changes read');
  const stmt = prepared.stmt;
  try {
    await sqlite3.bind_int(stmt, 1, since | 0);
    while (await sqlite3.step(stmt) === SQLite.SQLITE_ROW) {
      const [table, pk, cid, val, col_version, db_version, site_id, cl, seq, ts] =
        sqlite3.row(stmt);
      changes.push({
        table,
        pk: toHex(pk),
        cid,
        val: taggedVal(val),
        col_version, db_version,
        site_id: toHex(site_id),
        cl, seq, ts,
      });
    }
  } finally {
    await sqlite3.finalize(stmt);
  }
  reply(id, 'changes', { changes });
}

async function applyChanges(id, changes) {
  const prepared = await sqlite3.prepare_v2(
    db,
    'INSERT INTO crsql_changes ("table","pk","cid","val","col_version","db_version","site_id","cl","seq","ts")' +
    ' VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)');
  if (!prepared) throw new Error('prepare produced no statement for crsql_changes insert');
  const stmt = prepared.stmt;
  let count = 0;
  try {
    for (const c of changes) {
      await sqlite3.bind_text(stmt, 1, c.table);
      await sqlite3.bind_blob(stmt, 2, fromHex(c.pk));
      await sqlite3.bind_text(stmt, 3, c.cid);
      await bindVal(stmt, 4, c.val);
      await sqlite3.bind_int(stmt, 5, c.col_version);
      await sqlite3.bind_int(stmt, 6, c.db_version);
      await sqlite3.bind_blob(stmt, 7, fromHex(c.site_id));
      await sqlite3.bind_int(stmt, 8, c.cl);
      await sqlite3.bind_int(stmt, 9, c.seq);
      await sqlite3.bind_text(stmt, 10, c.ts);
      const rc = await sqlite3.step(stmt);
      if (rc !== SQLite.SQLITE_DONE) throw new Error(`crsql_changes insert rc=${rc}`);
      count++;
      await sqlite3.reset(stmt);
    }
  } finally {
    await sqlite3.finalize(stmt);
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
async function bindVal(stmt, idx, v) {
  if ('Null' in v) return sqlite3.bind_null(stmt, idx);
  if ('Integer' in v) return sqlite3.bind_int(stmt, idx, v.Integer);
  if ('Real' in v) return sqlite3.bind_double(stmt, idx, v.Real);
  if ('Text' in v) return sqlite3.bind_text(stmt, idx, v.Text);
  if ('Blob' in v) return sqlite3.bind_blob(stmt, idx, fromHex(v.Blob));
  throw new Error(`bad wire val ${JSON.stringify(v)}`);
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
        return execScalar('SELECT crsql_as_crr(?1)', (stmt) => sqlite3.bind_text(stmt, 1, msg.table));
      case 'set-ts': return execScalar('SELECT crsql_set_ts(?1)', (stmt) => sqlite3.bind_text(stmt, 1, String(msg.ts)));
      case 'changes-since': return changesSince(id, msg.since ?? 0);
      case 'apply': return applyChanges(id, msg.changes ?? []);
      default: throw new Error(`unknown op ${JSON.stringify(msg.op)}`);
    }
  };
  run().catch((err) => errorReply(id, 'sql-error', err));
};
