# node-worker

Node.js-compatible runtime running transformed code in a Worker with node globals.

## Starting a worker

`NodeWorker.create(workerURL, puterToken, cwd, options)` is the entry point. The second
argument decides which of the two starts you get: a puter token makes the worker that user's,
and an empty one makes it **anonymous** — nothing in the runtime calls api.puter.com, and the
filesystem and network are whatever you supply instead.

`swURL` is needed either way: the module resolver is synchronous end to end, so without the
service worker backing synchronous `fs` there is no working `require` and nothing runs.
Pass `requireSyncFs: false` only if you genuinely intend to run on `fs.promises` alone.

### Authenticated

```js
import { NodeWorker } from "node-worker";
import workerURL from "node-worker/worker?url";
import swURL from "node-worker/sw?url";

const worker = await NodeWorker.create(workerURL, puterToken, "/project", {
	swURL,
});
await worker.import("/project/index.js", {
	argv: ["node", "/project/index.js"],
});
```

### Anonymous

```js
import { NodeWorker } from "node-worker";
import workerURL from "node-worker/worker?url";
import swURL from "node-worker/sw?url";

const worker = await NodeWorker.create(workerURL, "", "/project", {
	swURL,
	// Optional. Without it the worker has no network at all.
	net: {
		wispUrl: "wss://anura.pro/",
		peerToken: localStorage.getItem("peer") ?? crypto.randomUUID(),
	},
});

// Nothing is mounted under "/" but memory, so put the code there yourself.
const project = worker.vfs.mountMemory("/project");
project.write([
	{ path: "package.json", data: pkgJson },
	{ path: "index.js", data: src },
]);

await worker.import("/project/index.js", {
	argv: ["node", "/project/index.js"],
});
```

What differs from the authenticated start:

- **The filesystem is yours to provide.** The root is the memory overlay with nothing under it, so a fresh anonymous worker has an empty `/` and a memory `/tmp`. Populate it with `worker.vfs.mountMemory(...)`, or mount a real backend like `createDirectoryHandleProvider` over a File System Access handle, or your own `VfsProvider` for OPFS/IndexedDB/a fetch.
- **The network comes from `options.net`.** Any [Wisp](https://github.com/MercuryWorkshop/wisp-protocol)-compliant relay works.
- **`fs.watch` only sees local mutations.** Mutations made through this worker's own providers still reach watchers.

## Running programs

`node:child_process` throws `ENOSYS` until the page registers a `ProcessProvider`
(`src/process/provider.ts`), because where a program *runs* is the embedder's decision and
a worker cannot host one for another worker — only a page can create a `NodeWorker`.

For the common case of "each child is a `node` of its own", the provider ships:

```js
import { NodeWorker, createWorkerProcessProvider, nodeCommandLine } from "node-worker";

worker.registerProcessProvider(
	createWorkerProcessProvider({
		async start(request) {
			if (request.file !== "node") return null;          // null ⇒ ENOENT
			const cmd = nodeCommandLine(request.args);
			if (!cmd) return null;
			const child = await NodeWorker.create(workerURL, "", request.cwd ?? "/", {
				swURL,
				vfs,               // shared, so the child sees the same filesystem
				keepalive: true,   // so a pending timer keeps the child alive, as in node
				isTTY: false,      // captured output, so nothing colourises it
			});
			return cmd.kind === "script"
				? { worker: child, target: cmd.path, module: cmd.module }
				: { worker: child, target: "/tmp/.eval.cjs", source: cmd.source, module: "cjs" };
		},
	})
);
```

`worker.spawnSibling(overrides)` is the short way to make the child: it inherits the worker
script, service worker, filesystem, network and TTY setting of the worker it is called on, so
only what differs has to be stated. It is the same call `worker_threads` and `fork` use.

Creating the worker is yours — which filesystem it shares, what answers *its*
`child_process`, whether it keeps a keepalive. Everything after that is the provider's:
readers attached before the run starts, a `poll` that waits rather than spins, stdin closed
when the caller says so *and* immediately when the caller never will, both exit shapes
normalised, and `terminate()` on every path. A pool would be a change to `start` alone.

`spawnSync` works through the same code path, which is the point of putting the host here:
the calling worker parks in a blocking `XMLHttpRequest` while this side runs the program on
its own event loop, so a synchronous spawn can be a whole pipeline rather than only a
program that never waits.

`child_process.fork` is still `ENOSYS`. Worker creation was never what stopped it — an IPC
channel is, and the process SPI carries stdout, stderr and exit and nothing else.

## Threads and forks

`worker_threads.Worker` and `child_process.fork` work with **no setup at all** — unlike
`spawn`, which needs a `ProcessProvider`, because "run an arbitrary program" is a question only
the embedder can answer. "Run another copy of this runtime" is not: the page already knows the
worker script, the service worker, the filesystem and the network it built this worker from, so
it can build another the same way.

```js
// in the worker
const { Worker } = require("worker_threads");
const w = new Worker("/app/job.js", { workerData: new Map([["size", 4]]) });
w.on("message", (v) => console.log(v));
w.postMessage({ buf }, [buf]);        // transferables work
```

A thread is a second `NodeWorker` the page creates, and the two workers are joined by a real
`MessageChannel` — one end delivered to each. So `postMessage` is the platform's own: structured
clone, transferables, ordering, and **the page is not in the data path**. There is no
serialisation code anywhere in this, which is the point; putting messages on the wire instead
would have meant inventing the binary format `node:v8`'s `serialize` deliberately refuses to fake.

`child_process.fork` is the same primitive with different manners. Node's `fork` is not a POSIX
fork — it starts a new process at the top of a named module, copying nothing, with an IPC channel
— so it needs a realm and a port and nothing more:

```js
const kid = require("child_process").fork("/app/kid.js", ["a", "b"]);
kid.on("message", (m) => …);          // kid uses process.send / process.on("message")
kid.send({ ping: 1 });
```

Worth knowing:

- **A worker start is not free** — around 300 ms, since it is a real `Worker` fetching a
  multi-megabyte bundle. `new Worker()` is not the cheap thing it is in node; a thread has to be
  worth a third of a second before it pays.
- A live `Worker`, a live `parentPort` and a live `fork` each keep their side's event loop open,
  as they do in node. `unref()`, `parentPort.close()` and `disconnect()` give that up.
- `isMainThread` is `true` at top level and `false` in a thread. It was hardcoded `false` before
  this existed, which was backwards.
- `receiveMessageOnPort` and `moveMessagePortToContext` still throw: the first needs a
  *synchronous* drain of a port, which browsers do not offer without `SharedArrayBuffer` — and
  not needing that is what lets this run without cross-origin isolation.

## Asking the host a question

A program can call its host by name, and — unlike a `MessagePort` — it can do so from inside
a synchronous call, where a parked worker will never read a port:

```js
// page
worker.registerChannelHandler("build.status", async (args) => ({ ok: true, id: args.id }));

// worker
const chan = require("node-worker/channel");
const answer = await chan.call("build.status", { id: 7 });
const same   = chan.callSync("build.status", { id: 7 });   // works while parked
```

`chan.open` — `worker.openChannel(name)` — remains the right shape when the two sides have a
protocol of their own to run and want structured clone and transferables.

## The filesystem cache

An authenticated worker puts a read cache in front of puterfs — stats, directory listings,
negative lookups and file contents — so a repeated read costs nothing and a directory listed
in full answers every miss under it locally. It is on by default, and it is only sound
because the same token buys a change feed to invalidate it with: puterfs's socket names every
path that moves, and when the socket is unavailable (an app launched with an app token cannot
authenticate one) the runtime falls back to polling the account's change counter, which says
_that_ something moved without saying what.

```js
new NodeVfs({
	puter: {
		token,
		cache: {
			maxBytes: 64 * 1024 * 1024, // file contents held; 0 caches metadata only
			maxFileBytes: 8 * 1024 * 1024, // larger files stream through uncached
			maxStaleMs: 3000, // tolerated window of unreported change
			prefetch: true, // turn a tree walk into subtree listings; see below
			enabled: true,
		},
	},
});
```

### Walks

A tree walk — a search tool, a build, anything that lists a directory and then lists each of
its subdirectories — otherwise costs one request per directory, and a `node_modules` with two
thousand of them costs two thousand requests. A ripgrep over one workspace was ~1000
`GET /fs/readdir`, the last 44 of them answered with 429.

So the cache watches for a **descent**: a listing that misses inside a directory whose own
listing it just answered. Nothing about a first listing says a walk is happening — a lone `ls`
must not drag a subtree over the wire — but the second one does, and the reply to a bounded
recursive listing rooted at the _parent_ answers the whole neighbourhood the walk is about to
ask for. Measured over five random trees of each shape, walked to the bottom:

| tree                          | requests without | with |
| ----------------------------- | ---------------- | ---- |
| ≤4 levels, ≤4 wide (232 dirs) | 232              | 59   |
| ≤6 levels, ≤4 wide (618 dirs) | 618              | 38   |
| ≤9 levels, ≤3 wide (445 dirs) | 445              | 52   |

On by default for puterfs, where a listing is a network round trip, and off elsewhere — over a
mount that is already local it trades bytes for round trips that were never being paid.
`prefetch: { depth, maxEntries }` tunes it.

Requests that come back 429 are retried with a jittered backoff, honouring `Retry-After` when
CORS lets it be read, and one 429 holds the account's other in-flight requests back rather than
letting each rediscover the same limit. `apiStats()` reports `(429 retried)` and
`(429 gave up)`. Only 429 — a 5xx says nothing about whether a mutation landed, and nothing in
this api is idempotent.

`vfs.cacheStats()` reports hits, misses and bytes held, alongside `apiStats()` (what left the
browser) and `opStats()` (what the worker asked for) — the three together are where a run's
filesystem traffic actually went. `vfs.flushCache()` drops it all.

Other mounts can have one too, via `mount(root, provider, { cache })`. It is on by default for
a **read-only** mount, since nothing can write through one, and off otherwise — there is no
change feed for a `FileSystemDirectoryHandle`, so "nobody else touches this" is a claim only
the consumer can make.
