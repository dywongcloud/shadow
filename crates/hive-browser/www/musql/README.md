# musql browser lane

musql compiled to WebAssembly, running **in memory** in a Web Worker.

This is a **third** browser database surface, and it is deliberately not a
substitute for either of the other two:

| lane | where | persistence | replication |
| --- | --- | --- | --- |
| `../sqlite/` (wa-sqlite + cr-sqlite v0.17) | DedicatedWorker, OPFS | durable | CRR via `Op::CrrSync` |
| managed SQLite (`hrana.rs`) | a node, `.db`/`.musq` file | durable | owner-routed, single writer |
| **this** | Web Worker, memory | **none** | **none** |

So this lane is for analysis and sandbox work over a snapshot the caller already
holds — an exported `.db`/`.musq` — not for tenant state. Nothing points it at
`$HIVE_DATA/browser-dbs/*`, and it must never be: a musql database has no clock
tables, so anything written here could never merge.

## Build

```sh
./build-musql.sh            # or MUSQL_SRC=/path/to/musql ./build-musql.sh
```

Builds from source (`GOOS=js GOARCH=wasm go build ./npm/wasm`) rather than
`npm install`, because the package is on GitHub Packages, which requires a token
even for public packages. `musql.wasm` is ~23 MB raw (~5.1 MB gzip, ~3.7 MB
brotli) and is gitignored — serve it compressed.

## Use

```js
import { openMusql } from "./musql-client.js";

const db = await openMusql({ name: "snap.db", data: bytes }); // musql or SQLite bytes
db.run("INSERT INTO t(a) VALUES (?)", 1);
const rows = await db.all("SELECT * FROM t");
const out = await db.export();        // Uint8Array: the whole database
const sqlite = await db.exportSQLite(); // the same database as a C SQLite file
await db.close();
```

The worker's protocol tag is `hive-musql-worker/1` — never `hive-sqlite-worker/1`
— so a caller cannot mistake one lane for the other.

## The JIT

musql compiles query kernels to wasm at run time, which a browser permits only
off the main thread: in a Worker the JIT is on, on a main thread
`open(name, { jit: false })` is the fallback (same answers, slower). Node always
has it.

## Verified live

`open` → `exec` → `run` → `all` → `get` → `query` → `export` → `open({data})`
→ `exportSQLite` (a real `SQLite format 3` file that reopens) → `close`.
