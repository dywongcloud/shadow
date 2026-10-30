// Host side of the musql browser lane: spawn ./musql-worker.js and talk to it
// with promises.
//
// Separate from ../sqlite/sync-client.js on purpose. That client owns an OPFS
// CRR replica and its whole job is the clock-table-safe merge; this one owns an
// in-memory musql database and has no merge at all. They share no code, no
// protocol tag and no storage, so neither can be mistaken for the other.
//
//   const db = await openMusql({ name: "snap.db", data: bytes });
//   db.run("INSERT INTO t(a) VALUES (?)", 1);
//   const rows = db.all("SELECT * FROM t");
//   const out = await db.export();      // Uint8Array — the whole database
//   await db.close();
//
// `data` may be either format (musql's own, or a C SQLite file — `open` tells
// them apart by header and converts), which is what makes this lane useful for
// inspecting an exported managed-SQLite snapshot without any server round trip.

const PROTO = "hive-musql-worker/1";

export function openMusql({ name, data, jit = true, workerUrl } = {}) {
  const url = workerUrl || new URL("./musql-worker.js", import.meta.url);
  const w = new Worker(url, { type: "module" });
  let next = 1;
  const pending = new Map();

  w.onmessage = (ev) => {
    const m = ev.data;
    if (!m || m.proto !== PROTO || !pending.has(m.id)) return;
    const { resolve, reject } = pending.get(m.id);
    pending.delete(m.id);
    if (m.op === "error") {
      const e = new Error(m.message);
      e.name = m.name || "Error";
      reject(e);
      return;
    }
    resolve(m);
  };
  w.onerror = (e) => {
    for (const [, p] of pending) p.reject(e.error || new Error(String(e.message)));
    pending.clear();
  };

  const call = (payload) =>
    new Promise((resolve, reject) => {
      const id = next++;
      pending.set(id, { resolve, reject });
      w.postMessage(Object.assign({ proto: PROTO, id }, payload));
    });

  const api = {
    worker: w,
    raw: call,
    exec: (sql) => call({ op: "exec", sql }).then(() => undefined),
    run: (sql, ...params) =>
      call({ op: "run", sql, params }).then((r) => ({
        changes: r.changes,
        lastInsertRowid: r.lastInsertRowid,
      })),
    all: (sql, ...params) => call({ op: "all", sql, params }).then((r) => r.rows),
    get: (sql, ...params) => call({ op: "get", sql, params }).then((r) => r.row),
    query: (sql, ...params) =>
      call({ op: "query", sql, params }).then((r) => ({ columns: r.columns, rows: r.rows })),
    export: () => call({ op: "export" }).then((r) => r.bytes),
    exportSQLite: () => call({ op: "export-sqlite" }).then((r) => r.bytes),
    close: async () => {
      await call({ op: "close" });
      w.terminate();
    },
  };

  return call({ op: "open", name, data, jit }).then((r) => {
    api.opened = { name: r.name, jit: r.jit, persistent: r.persistent, lane: r.lane };
    return api;
  });
}
