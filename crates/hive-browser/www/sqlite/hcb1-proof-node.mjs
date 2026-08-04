// HCB1 canonical-frame proof, JS half (bn-browser-fleet-crr-exchange) —
// drives hcb1.js (the browser glue's HCB1 implementation) against the Rust
// `hive_crsql::ChangeBatch::encode`/`decode` on REAL exports, orchestrated by
// prove-wire.sh:
//
//   node hcb1-proof-node.mjs roundtrip <in.hex> <out.hex>
//     decode each Rust-encoded hex line and re-encode it; the output must
//     compare byte-identical to the input (JS decode+encode fidelity).
//   node hcb1-proof-node.mjs emit <b.json> <out.hex>
//     build batches from the wasm peer's JSON dump (same grouping as
//     wire_proof.rs's apply-final: one batch per origin site chained from 0)
//     and encode them; the Rust apply-hcb1 step then decodes, re-encodes
//     (byte-compare), and applies them to a real db.

import { readFileSync, writeFileSync } from 'node:fs';
import { encodeBatch, decodeBatch, toHex, fromHex } from './hcb1.js';

function fail(msg) {
  console.error(`HCB1_PROOF_FAIL: ${msg}`);
  process.exit(1);
}

function hcbVal(v) {
  if ('Null' in v) return { tag: 0 };
  if ('Integer' in v) return { tag: 1, int: BigInt(v.Integer) };
  if ('Real' in v) return { tag: 2, real: v.Real };
  if ('Text' in v) return { tag: 3, text: v.Text };
  if ('Blob' in v) return { tag: 4, blob: fromHex(v.Blob) };
  fail(`bad wire val ${JSON.stringify(v)}`);
}

function roundtrip(inPath, outPath) {
  const lines = readFileSync(inPath, 'utf8').split('\n').filter((l) => l.trim());
  const out = [];
  for (const [i, line] of lines.entries()) {
    let batch;
    try {
      batch = decodeBatch(fromHex(line));
    } catch (err) {
      fail(`line ${i + 1}: JS decode rejected a Rust-encoded frame: ${err.message}`);
    }
    out.push(toHex(encodeBatch(batch)));
  }
  writeFileSync(outPath, out.join('\n') + '\n');
  console.log(`roundtrip: ${out.length} batch(es) decoded+re-encoded by hcb1.js`);
}

function emit(bPath, outPath) {
  const dump = JSON.parse(readFileSync(bPath, 'utf8'));
  const bySite = new Map();
  for (const c of dump.changes) {
    const change = {
      table: c.table,
      pk: fromHex(c.pk),
      cid: c.cid,
      val: hcbVal(c.val),
      col_version: BigInt(c.col_version),
      db_version: BigInt(c.db_version),
      site_id: fromHex(c.site_id),
      cl: BigInt(c.cl),
      seq: BigInt(c.seq),
      ts: c.ts,
    };
    const key = c.site_id;
    if (!bySite.has(key)) bySite.set(key, []);
    bySite.get(key).push(change);
  }
  const out = [];
  for (const [siteHex, changes] of [...bySite.entries()].sort()) {
    changes.sort((a, b) => (a.db_version < b.db_version ? -1 : a.db_version > b.db_version ? 1 : a.seq < b.seq ? -1 : a.seq > b.seq ? 1 : 0));
    out.push(toHex(encodeBatch({ site_id: fromHex(siteHex), since_db_version: 0n, changes })));
  }
  writeFileSync(outPath, out.join('\n') + '\n');
  console.log(`emit: ${out.length} batch(es) encoded by hcb1.js from the wasm peer's export`);
}

const [cmd, ...rest] = process.argv.slice(2);
if (cmd === 'roundtrip' && rest.length === 2) roundtrip(...rest);
else if (cmd === 'emit' && rest.length === 2) emit(...rest);
else {
  console.error('usage: hcb1-proof-node.mjs roundtrip <in.hex> <out.hex>');
  console.error('       hcb1-proof-node.mjs emit <b.json> <out.hex>');
  process.exit(2);
}
