// Live, no-mocks wire-format witness, WASM half: drives the REAL browser
// crsqlite build (vendor/cr-sqlite compiled to wasm32-unknown-emscripten via
// the wa-sqlite Emscripten tooling, see build-sqlite.sh) through the same
// sync round trip hive-crsql's sync_roundtrip example runs natively, with
// peer B executed here under Node against the node-capable proof build in
// ./.build/proof-node/ (also produced by build-sqlite.sh).
//
// Subcommands (orchestrated by prove-wire.sh):
//   node wire-proof-node.mjs run-b <a.json> <b.json> <b-final.json>
//   node wire-proof-node.mjs compare <a.json> <b.json> <a-final.json> <b-final.json>
//
// The JSON shape is byte-compatible with crates/hive-crsql/examples/wire_proof.rs:
//   {"changes":[{"table","pk"(hex),"cid","val"({"Null":null}|{"Integer":n}|
//     {"Real":f}|{"Text":s}|{"Blob":hex}),"col_version","db_version",
//     "site_id"(hex),"cl","seq","ts"}],"rows":[[id,label],...]}
// `compare` fails loudly unless every change that crossed the wire arrives
// byte-identical in ALL TEN columns -- ts included -- and both peers converge
// on the same row set. Not a test file (see AGENTS.md): a real execution
// proof, the same category as scripts/*.sh and crates/*/examples/*.

import { readFileSync, writeFileSync } from 'node:fs';

const SQLITE_ROW = 100, SQLITE_DONE = 101;
const SQLITE_INTEGER = 1, SQLITE_FLOAT = 2, SQLITE_TEXT = 3, SQLITE_BLOB = 4, SQLITE_NULL = 5;
const SQLITE_TRANSIENT = -1; // sqlite copies the bound value

const hex = (bytes) => Buffer.from(bytes).toString('hex');
const unhex = (s) => new Uint8Array(Buffer.from(s, 'hex'));

class WasmPeer {
  static async create() {
    const moduleUrl = new URL('./.build/proof-node/crsqlite-sync.mjs', import.meta.url);
    const { default: ModuleFactory } = await import(moduleUrl);
    const module = await ModuleFactory({});
    const peer = new WasmPeer();
    peer.m = module;
    const out = module._malloc(4);
    const rc = peer.ccall('sqlite3_open_v2', ['string', 'number', 'number', 'number'],
      [':memory:', out, 0x2 | 0x4, null]); // READWRITE|CREATE
    if (rc !== 0) throw new Error(`sqlite3_open_v2 rc=${rc}`);
    peer.db = module.getValue(out, 'i32');
    module._free(out);
    return peer;
  }

  ccall(name, argTypes, args, ret = 'number') {
    return this.m.ccall(name, ret, argTypes, args);
  }

  exec(sql) {
    const err = this.m._malloc(4);
    const rc = this.ccall('sqlite3_exec', ['number', 'string', 'number', 'number', 'number'],
      [this.db, sql, 0, 0, err]);
    if (rc !== 0) {
      const msgPtr = this.m.getValue(err, 'i32');
      const msg = msgPtr ? this.m.UTF8ToString(msgPtr) : '';
      this.m._free(err);
      throw new Error(`exec failed rc=${rc} for ${JSON.stringify(sql)}: ${msg}`);
    }
    this.m._free(err);
  }

  prepare(sql) {
    const out = this.m._malloc(4);
    const rc = this.ccall('sqlite3_prepare_v2', ['number', 'string', 'number', 'number', 'number'],
      [this.db, sql, -1, out, 0]);
    if (rc !== 0) {
      const msg = this.ccall('sqlite3_errmsg', ['number'], [this.db], 'string');
      this.m._free(out);
      throw new Error(`prepare failed rc=${rc} for ${JSON.stringify(sql)}: ${msg}`);
    }
    const stmt = this.m.getValue(out, 'i32');
    this.m._free(out);
    return stmt;
  }

  queryAll(sql) {
    const stmt = this.prepare(sql);
    const ncol = this.ccall('sqlite3_column_count', ['number'], [stmt]);
    const rows = [];
    let rc;
    while ((rc = this.ccall('sqlite3_step', ['number'], [stmt])) === SQLITE_ROW) {
      const row = [];
      for (let i = 0; i < ncol; i++) {
        const t = this.ccall('sqlite3_column_type', ['number', 'number'], [stmt, i]);
        if (t === SQLITE_TEXT) {
          row.push(this.ccall('sqlite3_column_text', ['number', 'number'], [stmt, i], 'string'));
        } else if (t === SQLITE_INTEGER) {
          row.push(this.ccall('sqlite3_column_int64', ['number', 'number'], [stmt, i]));
        } else if (t === SQLITE_FLOAT) {
          row.push(this.ccall('sqlite3_column_double', ['number', 'number'], [stmt, i]));
        } else if (t === SQLITE_BLOB) {
          const n = this.ccall('sqlite3_column_bytes', ['number', 'number'], [stmt, i]);
          const p = this.ccall('sqlite3_column_blob', ['number', 'number'], [stmt, i]);
          row.push(new Uint8Array(this.m.HEAPU8.buffer, p, n).slice());
        } else if (t === SQLITE_NULL) {
          row.push(null);
        } else {
          throw new Error(`unexpected column type ${t}`);
        }
      }
      rows.push(row);
    }
    this.ccall('sqlite3_finalize', ['number'], [stmt]);
    if (rc !== SQLITE_DONE) throw new Error(`step rc=${rc} for ${JSON.stringify(sql)}`);
    return rows;
  }

  // One crsql_changes row -> wire change (ten columns, vtab declared order).
  dumpChanges() {
    const rows = this.queryAll(
      'SELECT "table","pk","cid","val","col_version","db_version","site_id","cl","seq","ts" FROM crsql_changes');
    return rows.map(([table, pk, cid, val, col_version, db_version, site_id, cl, seq, ts]) => ({
      table, pk: hex(pk), cid, val: wireVal(val), col_version, db_version,
      site_id: hex(site_id), cl, seq, ts,
    }));
  }

  dumpRows() {
    return this.queryAll('SELECT id, label FROM items ORDER BY id');
  }

  applyChanges(changes) {
    const stmt = this.prepare(
      'INSERT INTO crsql_changes ("table","pk","cid","val","col_version","db_version","site_id","cl","seq","ts")' +
      ' VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)');
    for (const c of changes) {
      this.bindText(stmt, 1, c.table);
      this.bindBlob(stmt, 2, unhex(c.pk));
      this.bindText(stmt, 3, c.cid);
      this.bindWireVal(stmt, 4, c.val);
      this.ccall('sqlite3_bind_int', ['number', 'number', 'number'], [stmt, 5, c.col_version]);
      this.ccall('sqlite3_bind_int', ['number', 'number', 'number'], [stmt, 6, c.db_version]);
      this.bindBlob(stmt, 7, unhex(c.site_id));
      this.ccall('sqlite3_bind_int', ['number', 'number', 'number'], [stmt, 8, c.cl]);
      this.ccall('sqlite3_bind_int', ['number', 'number', 'number'], [stmt, 9, c.seq]);
      this.bindText(stmt, 10, c.ts);
      const rc = this.ccall('sqlite3_step', ['number'], [stmt]);
      if (rc !== SQLITE_DONE) {
        this.ccall('sqlite3_finalize', ['number'], [stmt]);
        throw new Error(`apply step rc=${rc} for change ${JSON.stringify(c)}`);
      }
      this.ccall('sqlite3_reset', ['number'], [stmt]);
      this.ccall('sqlite3_clear_bindings', ['number'], [stmt]);
    }
    this.ccall('sqlite3_finalize', ['number'], [stmt]);
  }

  bindText(stmt, idx, s) {
    this.ccall('sqlite3_bind_text', ['number', 'number', 'string', 'number', 'number'],
      [stmt, idx, s, -1, SQLITE_TRANSIENT]);
  }

  bindBlob(stmt, idx, bytes) {
    const p = this.m._malloc(bytes.length);
    this.m.HEAPU8.set(bytes, p);
    this.ccall('sqlite3_bind_blob', ['number', 'number', 'number', 'number', 'number'],
      [stmt, idx, p, bytes.length, SQLITE_TRANSIENT]);
    this.m._free(p); // SQLITE_TRANSIENT copied it during bind
  }

  bindWireVal(stmt, idx, v) {
    if ('Null' in v) this.ccall('sqlite3_bind_null', ['number', 'number'], [stmt, idx]);
    else if ('Integer' in v) this.ccall('sqlite3_bind_int', ['number', 'number', 'number'], [stmt, idx, v.Integer]);
    else if ('Real' in v) this.ccall('sqlite3_bind_double', ['number', 'number', 'number'], [stmt, idx, v.Real]);
    else if ('Text' in v) this.bindText(stmt, idx, v.Text);
    else if ('Blob' in v) this.bindBlob(stmt, idx, unhex(v.Blob));
    else throw new Error(`bad wire val ${JSON.stringify(v)}`);
  }
}

function wireVal(v) {
  if (v === null) return { Null: null };
  if (v instanceof Uint8Array) return { Blob: hex(v) };
  if (typeof v === 'number') return Number.isInteger(v) ? { Integer: v } : { Real: v };
  if (typeof v === 'string') return { Text: v };
  throw new Error(`cannot encode value of type ${typeof v}`);
}

const readJson = (p) => JSON.parse(readFileSync(p, 'utf8'));
const writeJson = (p, v) => writeFileSync(p, JSON.stringify(v, null, 2));

async function runB(aPath, bPath, bFinalPath) {
  const b = await WasmPeer.create();
  b.exec('CREATE TABLE items (id INTEGER PRIMARY KEY NOT NULL, label TEXT);');
  b.queryAll("SELECT crsql_as_crr('items')");
  b.exec("SELECT crsql_set_ts('1754000000002')");
  b.exec("INSERT INTO items (id, label) VALUES (2, 'from-b')");
  const bDump = { changes: b.dumpChanges(), rows: b.dumpRows() };
  writeJson(bPath, bDump);
  console.log(`run-b: ${bDump.changes.length} change(s), site_id=${bDump.changes[0].site_id}, ts=${bDump.changes[0].ts}`);

  const a = readJson(aPath);
  b.applyChanges(a.changes);
  const bFinal = { changes: b.dumpChanges(), rows: b.dumpRows() };
  writeJson(bFinalPath, bFinal);
  console.log(`run-b: applied ${a.changes.length} change(s) from native peer; b now holds ${bFinal.changes.length} change(s), ${bFinal.rows.length} row(s)`);
}

function fail(msg) {
  console.error(`WIRE_PROOF_FAIL: ${msg}`);
  process.exit(1);
}

function compare(aPath, bPath, aFinalPath, bFinalPath) {
  const a = readJson(aPath), b = readJson(bPath);
  const aFinal = readJson(aFinalPath), bFinal = readJson(bFinalPath);

  // Convergence: both peers' row sets identical.
  const expectRows = [[1, 'from-a'], [2, 'from-b']];
  if (JSON.stringify(aFinal.rows) !== JSON.stringify(expectRows))
    fail(`native peer rows diverged: ${JSON.stringify(aFinal.rows)}`);
  if (JSON.stringify(bFinal.rows) !== JSON.stringify(expectRows))
    fail(`wasm peer rows diverged: ${JSON.stringify(bFinal.rows)}`);

  // Byte-level wire agreement: the change each side exported must arrive at
  // the other peer byte-identical in all ten columns -- ts included.
  const aSent = JSON.stringify(a.changes[0]);
  const bSent = JSON.stringify(b.changes[0]);
  const aGot = bFinal.changes.find(c => c.site_id === a.changes[0].site_id);
  const bGot = aFinal.changes.find(c => c.site_id === b.changes[0].site_id);
  if (!aGot) fail('wasm peer does not hold the native change after merge');
  if (!bGot) fail('native peer does not hold the wasm change after merge');
  if (JSON.stringify(aGot) !== aSent)
    fail(`native->wasm change mutated on the wire:\nsent: ${aSent}\ngot:  ${JSON.stringify(aGot)}`);
  if (JSON.stringify(bGot) !== bSent)
    fail(`wasm->native change mutated on the wire:\nsent: ${bSent}\ngot:  ${JSON.stringify(bGot)}`);

  // Column-shape audit on what arrived: ten columns, declared vtab order,
  // pk/site_id blobs of the crsql shapes, ts a non-empty u64 decimal string.
  for (const [label, c] of [['wasm->native', bGot], ['native->wasm', aGot]]) {
    const keys = Object.keys(c);
    const want = ['table', 'pk', 'cid', 'val', 'col_version', 'db_version', 'site_id', 'cl', 'seq', 'ts'];
    if (JSON.stringify(keys) !== JSON.stringify(want))
      fail(`${label}: wire columns ${JSON.stringify(keys)} != ${JSON.stringify(want)}`);
    if (!/^[0-9a-f]{32}$/.test(c.site_id)) fail(`${label}: site_id not a 16-byte hex blob: ${c.site_id}`);
    if (!/^[0-9]+$/.test(c.ts) || c.ts === '0') fail(`${label}: ts not a real u64 decimal string: ${c.ts}`);
  }

  console.log(`native->wasm: ${aSent}`);
  console.log(`wasm->native: ${bSent}`);
  console.log(`rows both peers: ${JSON.stringify(aFinal.rows)}`);
  console.log('WIRE_PROOF_OK');
}

const [cmd, ...rest] = process.argv.slice(2);
if (cmd === 'run-b' && rest.length === 3) await runB(...rest);
else if (cmd === 'compare' && rest.length === 4) compare(...rest);
else {
  console.error('usage: wire-proof-node.mjs run-b <a.json> <b.json> <b-final.json>');
  console.error('       wire-proof-node.mjs compare <a.json> <b.json> <a-final.json> <b-final.json>');
  process.exit(2);
}
