// hive browser-node musql worker.
//
// A THIRD database surface, kept deliberately separate from the other two:
//
//   * ./../sqlite/sqlite-worker.js  — wa-sqlite + cr-sqlite v0.17, an OPFS
//     replica per PROJECT, replicating through `Op::CrrSync`. That is the
//     browser lane with a replication story.
//   * the managed SQLite lane        — a .db/.musq FILE on a node, reached over
//     Hrana.
//   * THIS                           — musql compiled to wasm, IN MEMORY, in
//     this worker. No clock tables, no CRR, no replication, no persistence:
//     `export()` / `open(name, { data })` move a whole database in and out as
//     the bytes of one file.
//
// So this lane is for analysis/sandbox work over a snapshot the caller already
// holds, never for tenant state. It is pointed at `$HIVE_DATA/browser-dbs/*`
// by nothing, and it must never be: a musql database has no clock tables, so
// anything written here could never merge.
//
// Run in a DedicatedWorker (or SharedWorker/Node with the same module graph)
// because musql's query JIT compiles WebAssembly at run time, which a browser
// permits only off the main thread. On a main thread `open(..., {jit:false})`
// is the fallback — same answers, slower.
//
// Message protocol (structured clone, versioned envelope). The proto tag is
// NOT the sqlite lane's, so a caller can never mistake one for the other:
//   -> { proto:'hive-musql-worker/1', id, op:'open', name?, data?, jit? }
//   <- { proto, id, op:'opened', name, jit, persistent:false, lane:'musql-memory' }
//   -> { proto, id, op:'exec', sql }                  (SQL returning no rows)
//   <- { proto, id, op:'ok' }
//   -> { proto, id, op:'run', sql, params? }
//   <- { proto, id, op:'ok', changes, lastInsertRowid }
//   -> { proto, id, op:'all', sql, params? }
//   <- { proto, id, op:'rows', rows: [ {} ] }
//   -> { proto, id, op:'get', sql, params? }
//   <- { proto, id, op:'row', row }
//   -> { proto, id, op:'query', sql, params? }
//   <- { proto, id, op:'rows', columns, rows: [ [] ] }
//   -> { proto, id, op:'export' }                     (musql's own format)
//   <- { proto, id, op:'exported', format:'musql', bytes: Uint8Array }
//   -> { proto, id, op:'export-sqlite' }              (a C SQLite file)
//   <- { proto, id, op:'exported', format:'sqlite', bytes: Uint8Array }
//   -> { proto, id, op:'close' }
//   <- { proto, id, op:'closed' }
//   <- { proto, id, op:'error', name, message }       (any op; never hangs)
//
// Values: an INTEGER outside JavaScript's safe range comes back as a BigInt and
// a BLOB as a Uint8Array — both structured-cloneable, so rows post as-is.

const PROTO = "hive-musql-worker/1";

let db = null;
let ready = null;

function reply(msg, extra) {
  self.postMessage(Object.assign({ proto: PROTO, id: msg.id }, extra));
}

function fail(msg, e) {
  reply(msg, {
    op: "error",
    name: (e && e.name) || "Error",
    message: (e && e.message) || String(e),
  });
}

async function load(jit) {
  ready ??= (async () => {
    // Imported lazily so an unbuilt lane (no musql.wasm — see build-musql.sh)
    // reports a typed error on `open` instead of throwing at import time.
    const mod = await import("./index.js");
    return mod;
  })();
  const mod = await ready;
  return mod;
}

async function handle(msg) {
  if (!msg || msg.proto !== PROTO) {
    // Not ours: ignore rather than answer, so two lanes on one port can coexist.
    return;
  }
  try {
    switch (msg.op) {
      case "open": {
        const mod = await load(msg.jit !== false);
        db = await mod.open(msg.name || "hive-browser.db", {
          data: msg.data,
          jit: msg.jit !== false,
        });
        reply(msg, {
          op: "opened",
          name: msg.name || "hive-browser.db",
          jit: msg.jit !== false,
          // Honest, and fixed: this lane has no persistence of any kind.
          persistent: false,
          lane: "musql-memory",
        });
        return;
      }
      case "exec":
        db.exec(msg.sql);
        reply(msg, { op: "ok" });
        return;
      case "run": {
        const r = db.run(msg.sql, ...(msg.params || []));
        reply(msg, { op: "ok", changes: r.changes, lastInsertRowid: r.lastInsertRowid });
        return;
      }
      case "all":
        reply(msg, { op: "rows", rows: db.all(msg.sql, ...(msg.params || [])) });
        return;
      case "get":
        reply(msg, { op: "row", row: db.get(msg.sql, ...(msg.params || [])) });
        return;
      case "query": {
        const q = db.query(msg.sql, ...(msg.params || []));
        reply(msg, { op: "rows", columns: q.columns, rows: q.rows });
        return;
      }
      case "export":
        reply(msg, { op: "exported", format: "musql", bytes: db.export() });
        return;
      case "export-sqlite":
        reply(msg, { op: "exported", format: "sqlite", bytes: db.exportSQLite() });
        return;
      case "close":
        if (db) db.close();
        db = null;
        reply(msg, { op: "closed" });
        return;
      default:
        throw new Error(`unknown op ${String(msg && msg.op)}`);
    }
  } catch (e) {
    // A caller is never left waiting: every op answers, including 'open' when
    // the lane was never built (the dynamic import above is what fails).
    if (!db && msg && msg.op === "open") {
      reply(msg, {
        op: "error",
        name: "musql-unbuilt",
        message: `musql browser lane is not built: ${e && e.message ? e.message : e}. Run crates/hive-browser/www/musql/build-musql.sh.`,
      });
      return;
    }
    fail(msg, e);
  }
}

self.onmessage = (ev) => {
  handle(ev.data).catch((e) => fail(ev.data, e));
};
