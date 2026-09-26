// The node-worker execution substrate for browser-executed functions
// (bn-node-worker-substrate / execution-path-swap-off-quickjs).
//
// WHAT THIS REPLACES, AND WHY. Browser artifacts used to run inside
// QuickJS-emscripten: `pkg/function-worker.js` embedded the interpreter wasm,
// evaluated the verified artifact in a bare-ECMAScript guest, and
// node-runtime.js was prepended INSIDE that guest to supply `require`,
// `process`, `Buffer`, `console`, timers and the Node builtin set — a
// hand-written shim standing in for Node. The operator's decision is to
// REPLACE QuickJS ENTIRELY: the substrate is now vendored node-worker
// (vendor/node-worker, Node's own lib transpiled for a Worker, built to
// crates/hive-browser/www/node-worker/{index,worker,sw,sw-handler}.js), so the
// guest gets the REAL Node module set and node-runtime.js's shim is gone.
//
// HOW AN ARTIFACT RUNS HERE. The host (this file) is page-side: it creates one
// node-worker Worker per artifact, mounts a filesystem at /hive, writes two
// files into it and runs the second one as CommonJS:
//
//   /hive/artifact.cjs — `module.exports = (<verified artifact source>);`
//   /hive/entry.cjs    — a platform driver: requires the artifact, calls it
//                        with (request, ops) and hands the result back.
//
// The artifact's own bytes are never rewritten: they were size-checked,
// BLAKE3-matched and policy-digest-verified by pin() before this line, and the
// `module.exports = (...)` wrapper is AROUND them, exactly the discipline
// node-runtime.js's `wrapArtifactSource` carried.
//
// THE GUEST ROOT IS PERSISTENT, NOT THE MEMORY OVERLAY (bn-node-worker-vfs-
// opfs-fsa). /hive is mounted by ./node-worker-vfs.js: OPFS preferred, a
// directory the donor picked with File System Access as the alternative, and a
// memory overlay in FRONT of that for the two files this host writes, so a
// guest's own state survives the run and the reload while the platform's
// per-run injector files never become litter in the donor's storage. A browser
// with neither backend is told so (`stats().fs`) and runs on memory knowingly
// — never a silent fallback that looks persistent and is not. Synchronous
// `require`/`readFileSync` reach the same provider through node-worker's
// service-worker bridge, which relays an opaque frame to this page and does not
// know what a mount is. See ./node-worker-vfs.js for what survives, what is
// wiped on revocation, and the concurrency guarantee.
//
// THE REQUEST GOES IN AS argv, THE RESULT COMES OUT AS A CHANNEL CALL. The
// driver reads the invocation request from `process.argv[2]` and returns its
// answer through `chan.callSync("hive.function.result", …)` — node-worker's
// one primitive that a program parked inside a synchronous call can still
// use. `chan.callSync` is also how `ops.call` reaches the host, which matters
// for artifacts that call a host op from inside a synchronous segment (a
// parked worker never reads a MessagePort).
//
// HONESTY RULE (preserved from node-runtime.js). A module the substrate cannot
// implement is NOT a silent no-op. Under node-worker the honest behaviour is
// the substrate's own: `node:dns` has no transport here and fails at the point
// of use with the runtime's own named error instead of resolving to nothing,
// and `node:net` / `node:tls` get a transport ONLY when a Wisp relay actually
// resolves — the platform relay the admission capability published, else a
// third-party public fallback the fleet opted into and the substrate proved
// reachable (bn-node-worker-wisp-net). With none of those, no relay is dialed
// and the first socket throws a named `WispRelayUnavailable` — never a silent
// nothing. `node:child_process` throws ENOSYS naming
// the missing ProcessProvider, which this host deliberately does not register
// (a second Node runtime per command is the embedder's call, and one per
// browser artifact is not it). Every failure THIS FILE produces is named
// `node_worker_<reason>: <fix>` — see substrateError below — so a donor's
// console and the worker's status both say which prerequisite is missing,
// never "undefined" or a hang.
//
// TRUST RULE (preserved). This runtime adds NO capability. It runs in a
// Worker realm of its own — not in the trusted worker that owns the iroh
// endpoint, the seed and the platform session — over a memory filesystem that
// only ever holds the artifact's own two files, with `process.env` replaced by
// an EMPTY object (`RunOptions.env` replaces it wholesale, so no project env
// and no secret can leak into a donor's browser) and no network transport of
// its own: a Wisp relay is dialed only if the platform published one in the
// admission capability's `net` block, which is DISCLOSED rather than silent
// (bn-node-worker-wisp-net) — and that relay is a third party unless the fleet
// operator runs it. The artifact reaches the host ONLY through `ops.call`, which the
// caller bounds by the artifact's `allowed_ops` before it ever dispatches.
//
// WHAT IS NOT MEDIATED, STATED PLAINLY. A node-worker guest is a real Worker
// with real Worker globals: `fetch`, `IndexedDB`, `CacheStorage` and
// same-origin credentialed requests are reachable from tenant code WITHOUT
// going through `ops.call`. That is a genuine change from the QuickJS guest,
// which had no such globals at all, and it is not closed here — see PRD row
// bn-node-worker-drop-opt-in, whose trust re-specification owns it.
//
// HOST CONTEXT, THE ONE HARD PREREQUISITE. node-worker's module resolver is
// synchronous end to end, so a guest `require` parks the Worker thread on a
// BLOCKING XMLHttpRequest that only a service worker can answer; the service
// worker relays it to the page that registered it. `navigator.serviceWorker` is
// `[Exposed=Window]`, so the host must be a PAGE — neither the run-node
// SharedWorker nor a nested dedicated Worker can be it (measured upstream: a
// SharedWorker global scope has no `Worker` constructor either). This file
// therefore refuses to boot anywhere else with a named reason, and the
// run-node worker brokers it through a connected page
// (node-worker-agent.js + the nodeWorkerRequest broker).
//
// ENGINE LIMITS, HONESTLY: node-worker enforces NO memory or stack quota. The
// descriptor's `memoryBytes`/`stackBytes` still ride the policy digest (they
// are part of the canonical encoding the server signs) but nothing meters
// them; the only bound this substrate enforces is the WALL-CLOCK deadline
// handed to invoke(), and a guest that outlives it is TERMINATED — a worker
// stuck in an uninterruptible native call cannot be interrupted, only
// abandoned. That is weaker than QuickJS's interrupt handler and is the price
// of the substrate swap, stated rather than hidden.

/** Asset names of the vendored build (scripts/build-node-worker.sh). */
export const NODE_WORKER_ASSETS = Object.freeze({
  index: "index.js",
  worker: "worker.js",
  sw: "sw.js",
});

/** Guest filesystem root every artifact runs under. */
export const GUEST_ROOT = "/hive";
export const GUEST_ARTIFACT_PATH = `${GUEST_ROOT}/artifact.cjs`;
export const GUEST_ENTRY_PATH = `${GUEST_ROOT}/entry.cjs`;

/** Channel names the guest driver calls the host on. */
export const OP_CHANNEL = "hive.function.op";
export const RESULT_CHANNEL = "hive.function.result";

/**
 * Why this context cannot host the substrate, if it cannot.
 *
 * Checked BEFORE anything is created, because the failure mode of getting it
 * wrong is a hang inside a guest program rather than an error.
 */
export function substrateUnavailableReason() {
  if (typeof navigator === "undefined" || !navigator.serviceWorker) {
    return {
      reason: "no-service-worker",
      detail:
        "the node-worker substrate must be hosted by a page (navigator.serviceWorker is [Exposed=Window]); " +
        "a worker global scope can neither register dist/sw.js nor, in a SharedWorker, construct a Worker at all",
    };
  }
  if (typeof isSecureContext === "boolean" && !isSecureContext) {
    return { reason: "insecure-context", detail: "service workers require a secure context (https, or localhost)" };
  }
  if (typeof Worker !== "function") {
    return { reason: "no-worker", detail: "this global scope has no Worker constructor, so it cannot start a node worker" };
  }
  return undefined;
}

/** Every failure this file raises: `node_worker_<reason>: <detail>`. */
export class NodeWorkerSubstrateError extends Error {
  constructor(reason, detail) {
    super(`node_worker_${reason}: ${detail}`);
    this.name = "NodeWorkerSubstrateError";
    this.reason = reason;
  }
}

export function substrateError(reason, detail) {
  return new NodeWorkerSubstrateError(reason, detail);
}

/** Absolute URLs of the three assets, resolved against `base`. */
export function assetUrls(base) {
  const root = new URL(typeof base === "string" ? base : "./node-worker/", import.meta.url);
  const at = name => new URL(name, root).href;
  return {
    indexUrl: at(NODE_WORKER_ASSETS.index),
    workerUrl: at(NODE_WORKER_ASSETS.worker),
    swUrl: at(NODE_WORKER_ASSETS.sw),
  };
}

/**
 * The artifact as a CommonJS module: the verified source wrapped, never
 * edited. `source` is the one `async function (request, ops)` expression the
 * build contract emits and pin() BLAKE3-verified.
 */
export function artifactModuleSource(source) {
  return `module.exports = (${source});\n`;
}

// The driver. NOTHING here is tenant code: it is the platform's fixed bridge
// between the invocation protocol and the artifact.
//
// Written as a plain string (no nested template literals) because it is
// evaluated inside the guest, where only `require` and `process` exist.
export const GUEST_ENTRY_SOURCE = [
  '"use strict";',
  'const chan = require("node-worker/channel");',
  'const handler = require("/hive/artifact.cjs");',
  'if (typeof handler !== "function") throw new TypeError("artifact must evaluate to a function");',
  "const ops = Object.freeze({",
  "  call(op, payload) {",
  '    const answer = chan.callSync("hive.function.op", { op: op, payload: payload });',
  '    if (!answer || answer.ok !== true) throw new Error((answer && answer.error) || "host operation failed");',
  "    return answer.value;",
  "  },",
  "});",
  'const done = result => chan.callSync("hive.function.result", result);',
  "Promise.resolve(handler(process.argv[2] || \"\", ops)).then(",
  '  value => { if (typeof value !== "string") done({ ok: false, error: "function result must be a string" });',
  "            else done({ ok: true, value: value }); },",
  '  error => done({ ok: false, error: String((error && error.message) || error) }),',
  ");",
  "",
].join("\n");

/**
 * The guest's Wisp relay config, or undefined when there is none to pass
 * (bn-node-worker-wisp-net).
 *
 * Re-validated here even though it came from the platform: this is the second
 * of two independent filters, and the value is about to become a socket
 * destination in a donor's browser. A malformed or hostile `wispUrl` is DROPPED
 * rather than forwarded — the substrate then falls back to its own resolution
 * instead of dialing something the fleet did not publish.
 */
export function normalizeHostNet(raw) {
  if (!raw || typeof raw !== "object") return undefined;
  const wispUrl = wispUrlOrNull(raw.wispUrl);
  const publicFallbackUrl = wispUrlOrNull(raw.publicFallbackUrl);
  const allowPublicFallback = raw.allowPublicFallback === true;
  if (!wispUrl && !publicFallbackUrl && !allowPublicFallback) return undefined;
  const net = {};
  if (wispUrl) net.wispUrl = wispUrl;
  if (publicFallbackUrl) net.publicFallbackUrl = publicFallbackUrl;
  if (allowPublicFallback) net.allowPublicFallback = true;
  return net;
}

function wispUrlOrNull(value) {
  return typeof value === "string" && /^wss?:\/\/[^\s]+$/.test(value.trim())
    ? value.trim()
    : null;
}

function withDeadline(promise, ms, reason, detail) {
  if (!Number.isFinite(ms) || ms <= 0) return promise;
  let timer;
  const deadline = new Promise((_, reject) => {
    timer = setTimeout(() => reject(substrateError(reason, detail)), ms);
  });
  return Promise.race([promise, deadline]).finally(() => clearTimeout(timer));
}

/**
 * One node-worker Worker hosting one artifact.
 *
 * One invocation at a time, by construction: the guest is a single-threaded
 * program and `require`ing the entry again while a run is parked would
 * interleave two artifacts' ops on one channel namespace.
 */
export class NodeWorkerHost {
  constructor(options) {
    this.indexUrl = options.indexUrl;
    this.workerUrl = options.workerUrl;
    this.swUrl = options.swUrl;
    this.artifactSource = options.artifactSource;
    this.timeoutMs = options.timeoutMs;
    this.bootTimeoutMs = options.bootTimeoutMs ?? 20_000;
    this.onOp = options.onOp;
    this.worker = undefined;
    this.pending = undefined;
    this.nextId = 0;
    this.closed = false;
    this.served = 0;
    /**
     * What scopes this guest's tree in storage — and therefore what a
     * revocation wipe deletes. Null means the platform's own directory, which
     * is the honest scope when the caller cannot name a project.
     */
    this.project =
      typeof options.project === "string" && options.project ? options.project : null;
    /** The mounted persistent tree, or undefined when this run is on memory. */
    this.guestFs = undefined;
    /** Why it is on memory, when it is. Reported by `stats()`, never silent. */
    this.guestFsReason = undefined;
    /**
     * The guest's `node:net` / `node:tls` config (bn-node-worker-wisp-net):
     * `{ wispUrl?, allowPublicFallback?, publicFallbackUrl? }`, or undefined
     * when the caller published none. `wispUrl` is a relay the caller pinned;
     * the rest is a PERMIT for the substrate to fall back to a third-party
     * public relay, proven reachable and logged before it carries anything.
     * With neither, the guest's first socket throws the substrate's named
     * `WispRelayUnavailable` — never a silent no-network.
     */
    this.net = normalizeHostNet(options.net);
  }

  static async boot(options) {
    const unavailable = substrateUnavailableReason();
    if (unavailable) throw substrateError(unavailable.reason, unavailable.detail);
    if (typeof options.artifactSource !== "string" || !options.artifactSource) {
      throw substrateError("no-artifact", "NodeWorkerHost.boot needs the verified artifact source");
    }
    if (typeof options.onOp !== "function") {
      throw substrateError("no-op-handler", "NodeWorkerHost.boot needs an onOp handler to reach the host");
    }
    const host = new NodeWorkerHost(options);
    await host.#start();
    return host;
  }

  async #start() {
    let module;
    try {
      module = await import(this.indexUrl);
    } catch (error) {
      throw substrateError(
        "unbuilt",
        `the node-worker lane is not built (${this.indexUrl} could not be imported) — run scripts/build-node-worker.sh: ${
          String(error?.message || error)
        }`,
      );
    }
    const NodeWorker = module?.NodeWorker;
    if (typeof NodeWorker?.create !== "function") {
      throw substrateError("unbuilt", `${this.indexUrl} exports no NodeWorker — the vendored build is stale or corrupted`);
    }
    // Anonymous (empty token): nothing in the runtime calls api.puter.com, the
    // filesystem is the memory mount below and the network is whatever the
    // caller configured — which is nothing. `requireSyncFs` stays at its
    // default (true): the module resolver is synchronous end to end, so a
    // substrate without the blocking transport is one where nothing runs, and
    // failing at boot with the reason named beats every artifact failing to
    // resolve its first require.
    //
    // `net` (bn-node-worker-wisp-net) is the guest's whole `node:net` /
    // `node:tls`: one Wisp relay URL, nothing else. Left undefined unless a
    // caller pinned one, so the substrate resolves it itself — the platform
    // relay the SharedWorker published from the admission capability
    // (`globalThis.HIVE_WISP_URL`, fleet `HIVE_BROWSER_WISP_URL`) first, then a
    // THIRD-PARTY public fallback ONLY when the fleet opted in and that relay
    // proves it answers, each use logged. With none of those the guest's first
    // socket throws a named `WispRelayUnavailable`; see
    // vendor/node-worker/src/wire/wisp.ts for the resolution and its reasoning.
    const created = NodeWorker.create(this.workerUrl, "", GUEST_ROOT, {
      swUrl: this.swUrl,
      isTTY: false,
      keepalive: false,
      requireSyncFs: true,
      net: this.net,
    });
    const worker = await withDeadline(created, this.bootTimeoutMs, "boot-timeout",
      `the node worker did not boot within ${this.bootTimeoutMs}ms`);
    if (this.closed) {
      worker.terminate();
      throw substrateError("closed", "the host was closed while the node worker was booting");
    }
    this.worker = worker;
    await this.#mountGuestRoot(worker);
    // Registered before the first run: a handler registered later would work
    // too (the table is read at call time) but an artifact that calls a host
    // op before then would get ENOSYS naming nothing.
    worker.registerChannelHandler(RESULT_CHANNEL, args => {
      this.#settle(args);
      return null;
    });
    worker.registerChannelHandler(OP_CHANNEL, async args => this.#runOp(args));
  }

  /**
   * Mount the guest root: persistent when the browser has a backend for it,
   * memory when it does not — and the reason recorded either way.
   *
   * `replace`, because this host re-mounts the same root for every artifact it
   * boots on one page.
   *
   * The two injector files go into the mount's memory overlay (`addVirtualFile`
   * resolves to it), so they are never persisted: they are this run's copies of
   * a verified artifact and a fixed driver, and writing them into the donor's
   * OPFS on every invocation would be pure litter.
   */
  async #mountGuestRoot(worker) {
    const files = [
      [GUEST_ARTIFACT_PATH, artifactModuleSource(this.artifactSource)],
      [GUEST_ENTRY_PATH, GUEST_ENTRY_SOURCE],
    ];
    let vfs = null;
    try {
      vfs = await import("./node-worker-vfs.js");
    } catch (error) {
      this.guestFsReason = `node-worker-vfs.js unavailable: ${String(error?.message || error)}`;
    }
    if (vfs) {
      try {
        this.guestFs = await vfs.mountGuestFs({
          vfs: worker.vfs,
          project: this.project ?? undefined,
          mount: GUEST_ROOT,
          replace: true,
        });
      } catch (error) {
        this.guestFsReason = String(error?.message || error);
      }
    }
    if (this.guestFs) {
      for (const [path, code] of files) worker.vfs.addVirtualFile(path, code);
      return;
    }
    // Memory, knowingly: no backend, or a mount that failed for a reason this
    // host cannot fix. The substrate still runs — a guest on a memory fs works,
    // it just does not persist — and `stats().fs` says which of the two it is.
    await worker.mountMemory(GUEST_ROOT, { replace: true });
    await worker.writeMemory(GUEST_ROOT, [
      { path: "artifact.cjs", data: artifactModuleSource(this.artifactSource) },
      { path: "entry.cjs", data: GUEST_ENTRY_SOURCE },
    ]);
  }

  async #runOp(args) {
    if (!args || typeof args !== "object") {
      return { ok: false, error: "host operation payload must be an object" };
    }
    try {
      const result = await this.onOp({ op: args.op, payload: args.payload });
      if (!result || typeof result !== "object") return { ok: false, error: "host operation returned no result" };
      return result;
    } catch (error) {
      return { ok: false, error: String(error?.message || error) };
    }
  }

  #settle(result) {
    const item = this.pending;
    if (!item || !result || typeof result !== "object") return;
    clearTimeout(item.timer);
    this.pending = undefined;
    this.served += 1;
    if (result.ok === true) item.resolve(result.value);
    else item.reject(new Error(String(result.error || "function invocation failed")));
  }

  /** Run the artifact once and resolve with its string result. */
  async invoke(request, deadlineMs) {
    if (this.closed) throw substrateError("closed", "the node-worker host is closed");
    if (this.pending) throw substrateError("busy", "the node-worker host is already running an invocation");
    const ms = Number.isFinite(deadlineMs) && deadlineMs > 0 ? deadlineMs : this.timeoutMs;
    const id = (this.nextId += 1);
    let timer;
    const settled = new Promise((resolve, reject) => {
      timer = setTimeout(() => {
        if (this.pending && this.pending.id !== id) return;
        this.pending = undefined;
        // A guest past its deadline is abandoned, not interrupted: the worker
        // is terminated, which is the only bound this substrate has.
        this.close();
        reject(substrateError("invoke-timeout", `the invocation exceeded its ${ms}ms budget`));
      }, ms);
      this.pending = { id, resolve, reject, timer };
    });
    // env: {} replaces process.env wholesale — the TRUST rule: no project env
    // and no secret reaches a donor's browser.
    const run = this.worker.require(GUEST_ENTRY_PATH, {
      argv: ["node", GUEST_ENTRY_PATH, request],
      env: {},
    });
    run.then(
      code => {
        if (!this.pending) return;
        this.pending.reject(
          substrateError("no-result", `the artifact returned no result (exit code ${code})`),
        );
        this.pending = undefined;
        clearTimeout(timer);
      },
      error => {
        if (!this.pending) return;
        this.pending.reject(error instanceof Error ? error : new Error(String(error)));
        this.pending = undefined;
        clearTimeout(timer);
      },
    );
    return settled;
  }

  close() {
    if (this.closed) return;
    this.closed = true;
    const item = this.pending;
    this.pending = undefined;
    if (item) {
      clearTimeout(item.timer);
      item.reject(substrateError("closed", "the node-worker host was closed"));
    }
    try {
      this.worker?.terminate();
    } catch {
      /* already gone */
    }
    this.worker = undefined;
    // Unmounted, NOT wiped: a donor closing a run keeps their filesystem
    // (./node-worker-vfs.js retention). Only revocation deletes it.
    const guestFs = this.guestFs;
    this.guestFs = undefined;
    if (guestFs) void guestFs.dispose().catch(() => {});
  }

  stats() {
    return {
      served: this.served,
      busy: this.pending !== undefined,
      closed: this.closed,
      fs: this.guestFs
        ? {
            kind: this.guestFs.kind,
            dir: this.guestFs.dir,
            mount: this.guestFs.mount,
            guarantee: this.guestFs.guarantee,
            lockHeld: this.guestFs.lock.held,
            // Whether `readFileSync`/`require` can reach this mount: the
            // service-worker bridge is what carries them.
            syncFs: this.guestFs.sync.ok ? true : this.guestFs.sync.reason,
          }
        : { kind: "memory", reason: this.guestFsReason ?? "not mounted" },
    };
  }

  /**
   * Delete the guest tree this donor holds for `project` — or all of them when
   * no project is named.
   *
   * Revocation, never teardown: a terminal admission denial means the grant
   * that authorized holding this data is gone. Every smaller event keeps it.
   */
  static async wipeGuestFs({ project } = {}) {
    const vfs = await import("./node-worker-vfs.js");
    return vfs.wipeGuestFs({ project });
  }
}
