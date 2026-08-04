// hcb1.js — the browser-side implementation of the TWO wire contracts the
// browser↔fleet CRR exchange rides (bn-browser-fleet-crr-exchange):
//
//   1. HCB1 change batches — the canonical binary form whose ONLY other
//      implementation is `hive_crsql::ChangeBatch::encode`/`decode`. The two
//      must agree byte-for-byte; the layout comment below is copied from that
//      Rust doc comment on purpose, and prove-wire.sh's HCB1 round-trip
//      compares this encoder against the Rust decoder on real exports.
//   2. The `Op::CrrSync` request/reply envelope — canonical definition in
//      `hive-browser-proto` (`encode_crr_sync_request`/`split_crr_sync_reply`),
//      mirrored here field-for-field (version byte, flags, u32/u16/i64
//      big-endian, exact-EOF consume, trailing-byte rejection, the same
//      count/length bounds).
//
// Integers: counts and lengths are Numbers (<= 2^32); every i64 field
// (versions, cl, seq) is a BigInt so the canonical form is exact beyond 2^53.
// pk/site_id are Uint8Array. val is a tagged object:
//   {tag:0} | {tag:1, int:BigInt} | {tag:2, real:number} |
//   {tag:3, text:string} | {tag:4, blob:Uint8Array}

export const CRR_SYNC_VERSION = 1;
export const CRR_FLAG_PUSH_MORE = 1;
export const CRR_FLAG_MORE = 1;
export const CRR_SITE_ID_MAX = 64;
export const CRR_MAX_WATERMARKS = 1024;
export const CRR_MAX_BATCHES = 4096;
export const CRR_MAX_MESSAGE = 2048;
export const CRR_MAX_DB_FILE = 256;
export const BROWSER_MAX_CRR_FRAME = 4 << 20;

// The canonical payload-size measure of one change's val — THE formula
// `hive_crsql::val_payload_bytes` carries; both sides must agree byte-for-byte
// because the sync boundary's max_value_bytes cap is enforced against it.
export function valPayloadBytes(v) {
  switch (v.tag) {
    case 0: return 1;
    case 1: case 2: return 9;
    case 3: return 5 + new TextEncoder().encode(v.text).length;
    case 4: return 5 + v.blob.length;
    default: throw new Error(`bad val tag ${v.tag}`);
  }
}

class Writer {
  constructor() { this.parts = []; this.len = 0; }
  u8(n) { this._push(Uint8Array.of(n & 0xff)); }
  u16(n) { const b = new DataView(new ArrayBuffer(2)); b.setUint16(0, n); this._push(new Uint8Array(b.buffer)); }
  u32(n) { const b = new DataView(new ArrayBuffer(4)); b.setUint32(0, n); this._push(new Uint8Array(b.buffer)); }
  i64(n) { const b = new DataView(new ArrayBuffer(8)); b.setBigInt64(0, BigInt(n)); this._push(new Uint8Array(b.buffer)); }
  f64bits(n) { const b = new DataView(new ArrayBuffer(8)); b.setFloat64(0, n); this._push(new Uint8Array(b.buffer)); }
  bytes(b) { this._push(b); }
  str(s) {
    const utf8 = new TextEncoder().encode(s);
    if (utf8.length > 0xffff) throw new Error(`string too long for HCB1 (${utf8.length} bytes)`);
    this.u16(utf8.length); this._push(utf8);
  }
  blob(b) { this.u32(b.length); this._push(b); }
  _push(b) { this.parts.push(b); this.len += b.length; }
  finish() {
    const out = new Uint8Array(this.len);
    let at = 0;
    for (const p of this.parts) { out.set(p, at); at += p.length; }
    return out;
  }
}

class Reader {
  constructor(bytes) { this.view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength); this.b = bytes; this.at = 0; }
  take(n) {
    if (this.b.length - this.at < n) throw new Error(`truncated frame at offset ${this.at}`);
    const s = this.b.subarray(this.at, this.at + n);
    this.at += n;
    return s;
  }
  u8() { return this.take(1)[0]; }
  u16() { const v = this.view.getUint16(this.at); this.at += 2; return v; }
  u32() { const v = this.view.getUint32(this.at); this.at += 4; return v; }
  i64() { const v = this.view.getBigInt64(this.at); this.at += 8; return v; }
  blob() { return this.take(this.u32()); }
  str() { return new TextDecoder().decode(this.take(this.u16())); }
  finish() { if (this.at !== this.b.length) throw new Error('trailing bytes after frame'); }
}

// HCB1 layout (mirrors hive_crsql::ChangeBatch::encode):
//   "HCB1" | site_id (u8 len + bytes) | since_db_version (i64)
//   | count (u32) | per change:
//     table (u16 + utf8) | pk (u32 + bytes) | cid (u16 + utf8)
//     | val (tag u8; 1=i64, 2=f64 bits, 3=u32+utf8, 4=u32+bytes)
//     | col_version (i64) | db_version (i64) | site_id (u8 len + bytes)
//     | cl (i64) | seq (i64) | ts (u16 + utf8)
export function encodeBatch(batch) {
  const w = new Writer();
  w.bytes(Uint8Array.of(0x48, 0x43, 0x42, 0x31)); // "HCB1"
  w.u8(batch.site_id.length);
  w.bytes(batch.site_id);
  w.i64(batch.since_db_version);
  w.u32(batch.changes.length);
  for (const c of batch.changes) {
    w.str(c.table);
    w.blob(c.pk);
    w.str(c.cid);
    const v = c.val;
    w.u8(v.tag);
    if (v.tag === 1) w.i64(v.int);
    else if (v.tag === 2) w.f64bits(v.real);
    else if (v.tag === 3) w.blob(new TextEncoder().encode(v.text));
    else if (v.tag === 4) w.blob(v.blob);
    else if (v.tag !== 0) throw new Error(`bad val tag ${v.tag}`);
    w.i64(c.col_version);
    w.i64(c.db_version);
    w.u8(c.site_id.length);
    w.bytes(c.site_id);
    w.i64(c.cl);
    w.i64(c.seq);
    w.str(c.ts);
  }
  return w.finish();
}

export function decodeBatch(bytes) {
  const r = new Reader(bytes);
  const magic = String.fromCharCode(...r.take(4));
  if (magic !== 'HCB1') throw new Error('bad magic: not an HCB1 change-batch frame');
  const siteLen = r.u8();
  const site_id = r.take(siteLen);
  const since_db_version = r.i64();
  const count = r.u32();
  const changes = [];
  let prev = null;
  for (let i = 0; i < count; i++) {
    const table = r.str();
    const pk = r.blob();
    const cid = r.str();
    const tag = r.u8();
    let val;
    if (tag === 0) val = { tag: 0 };
    else if (tag === 1) val = { tag: 1, int: r.i64() };
    else if (tag === 2) val = { tag: 2, real: (() => { const v = r.view.getFloat64(r.at); r.at += 8; return v; })() };
    else if (tag === 3) val = { tag: 3, text: new TextDecoder().decode(r.blob()) };
    else if (tag === 4) val = { tag: 4, blob: r.blob() };
    else throw new Error(`unknown val tag ${tag}`);
    const col_version = r.i64();
    const db_version = r.i64();
    const clen = r.u8();
    const csite = r.take(clen);
    const cl = r.i64();
    const seq = r.i64();
    const ts = r.str();
    if (csite.length !== site_id.length || !csite.every((b, j) => b === site_id[j]))
      throw new Error('mixed-site batch: change carries a different site_id than the batch');
    if (prev !== null && db_version < prev)
      throw new Error(`batch out of deterministic order: db_version ${db_version} after ${prev}`);
    prev = db_version;
    changes.push({ table, pk, cid, val, col_version, db_version, site_id: csite, cl, seq, ts });
  }
  r.finish();
  return { site_id, since_db_version, changes };
}

// ---- Op::CrrSync envelope (mirrors hive-browser-proto) ----

export function encodeCrrSyncRequest({ dbFile, pushMore = false, watermarks = [], batches = [] }) {
  const w = new Writer();
  w.u8(CRR_SYNC_VERSION);
  w.u8(pushMore ? CRR_FLAG_PUSH_MORE : 0);
  const name = new TextEncoder().encode(dbFile ?? '');
  if (name.length === 0 || name.length > CRR_MAX_DB_FILE) throw new Error('dbFile grant identifier out of bounds');
  w.u16(name.length);
  w.bytes(name);
  w.u32(watermarks.length);
  for (const [site, version] of watermarks) {
    w.u8(site.length);
    w.bytes(site);
    w.i64(version);
  }
  w.u32(batches.length);
  for (const b of batches) { w.u32(b.length); w.bytes(b); }
  return w.finish();
}

export function splitCrrSyncRequest(bytes) {
  const r = new Reader(bytes);
  if (r.u8() !== CRR_SYNC_VERSION) throw new Error('bad crr sync version');
  const flags = r.u8();
  const dbLen = r.u16();
  if (dbLen === 0 || dbLen > CRR_MAX_DB_FILE) throw new Error('db_file length out of bounds');
  const dbFile = new TextDecoder().decode(r.take(dbLen));
  const watermarks = readWatermarks(r);
  const batches = readBatches(r);
  r.finish();
  return { dbFile, pushMore: (flags & CRR_FLAG_PUSH_MORE) !== 0, watermarks, batches };
}

export function encodeCrrSyncReply({ status = 0, more = false, message = '', watermarks = [], batches = [] }) {
  const w = new Writer();
  w.u8(CRR_SYNC_VERSION);
  w.u8(status);
  w.u8(more ? CRR_FLAG_MORE : 0);
  const msg = new TextEncoder().encode(message).subarray(0, CRR_MAX_MESSAGE);
  w.u16(msg.length);
  w.bytes(msg);
  w.u32(watermarks.length);
  for (const [site, version] of watermarks) {
    w.u8(site.length);
    w.bytes(site);
    w.i64(version);
  }
  w.u32(batches.length);
  for (const b of batches) { w.u32(b.length); w.bytes(b); }
  return w.finish();
}

export function splitCrrSyncReply(bytes) {
  const r = new Reader(bytes);
  if (r.u8() !== CRR_SYNC_VERSION) throw new Error('bad crr sync version');
  const status = r.u8();
  if (status > 5) throw new Error(`bad crr sync status ${status}`);
  const flags = r.u8();
  const msgLen = r.u16();
  if (msgLen > CRR_MAX_MESSAGE) throw new Error('crr sync message over bound');
  const message = new TextDecoder().decode(r.take(msgLen));
  const watermarks = readWatermarks(r);
  const batches = readBatches(r);
  r.finish();
  return { status, more: (flags & CRR_FLAG_MORE) !== 0, message, watermarks, batches };
}

function readWatermarks(r) {
  const count = r.u32();
  if (count > CRR_MAX_WATERMARKS) throw new Error('watermark count over bound');
  const out = [];
  for (let i = 0; i < count; i++) {
    const siteLen = r.u8();
    if (siteLen === 0 || siteLen > CRR_SITE_ID_MAX) throw new Error('bad site id length');
    out.push([r.take(siteLen), r.i64()]);
  }
  return out;
}

function readBatches(r) {
  const count = r.u32();
  if (count > CRR_MAX_BATCHES) throw new Error('batch count over bound');
  const out = [];
  for (let i = 0; i < count; i++) {
    const len = r.u32();
    if (len > BROWSER_MAX_CRR_FRAME) throw new Error('batch over frame bound');
    out.push(r.take(len));
  }
  return out;
}

export const toHex = (bytes) => Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');
export const fromHex = (s) => {
  const out = new Uint8Array(s.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(s.slice(2 * i, 2 * i + 2), 16);
  return out;
};
