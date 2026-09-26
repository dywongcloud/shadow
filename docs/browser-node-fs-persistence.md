# Browser node guest filesystem: persistence contract

Code of record: `crates/hive-browser/www/node-worker-vfs.js` (published to
`ui/public/browser-node/node-worker-vfs.js` by `ui/scripts/sync-browser-node.mjs`).
Mounted by `crates/hive-browser/www/node-worker-host.js`; wiped on revocation from
`ui/public/run-node-worker.js`.

This is the browser node's *guest* filesystem — the tree the executed program
sees — not the CRR database replica (`docs/browser-db-contract.md`), which has its
own naming, its own lane and its own retention.

## Backend selection

`node-worker`'s filesystem is provider-based, and without a puter token its root
is a memory overlay — so a guest that installs dependencies or writes state loses
all of it when the tab goes away. Selection happens in `selectPersistentRoot()`,
in this order:

1. **OPFS** — `navigator.storage.getDirectory()`. Private to the origin, no
   permission prompt, survives reload, restart and profile reopen. Preferred, and
   the only backend mounted without asking anyone anything.
2. **File System Access** — `showDirectoryPicker()`. A real directory the donor
   chose. Reachable only from a user gesture, and its grant does **not** survive a
   reload, so it is the alternative, never the default.
3. **Neither** — `GuestFsUnavailable` naming why. The mount is declined; the host
   runs on memory *and says so* in `stats().fs`. A silent memory fallback would be
   a filesystem that looks persistent and is not.

Both 1 and 2 hand back the same object — a `FileSystemDirectoryHandle` — so both
run on the vendor's `createDirectoryHandleProvider`. Nothing reimplements a
backend. The tree is mounted with a memory overlay in front of it
(`{ overlay: true }`), so injected per-run files (the artifact and its driver)
never become litter in the donor's storage.

## Synchronous `fs`

`readFileSync` and friends are not a second implementation and do not bypass any
of this. A synchronous call in the guest parks the worker thread on a blocking
XHR; `sw.js` relays the frame — opaque bytes, no filesystem knowledge — to the
page, which runs `handleFrame`; the mount table resolves the path to whatever
provider is mounted there. Mounting a persistent provider is therefore
*sufficient* for the synchronous methods to hit persistent storage.

Two integration facts, both load-bearing:

- **The fs host must be a window.** `navigator.serviceWorker` is
  `[Exposed=Window]`, and a cold service worker re-attaches by asking
  `clients.matchAll({ type: "window" })`. The donor node runs in a SharedWorker,
  which has neither, so the filesystem is hosted by a connected page and brokered
  — the same shape as the sqlite DedicatedWorker broker. `syncFsHost()` reports
  this rather than letting a sync call hang.
- **No `SharedArrayBuffer` is required.** The bridge is a blocking XHR plus a
  `MessagePort`; there is no shared memory in `src/lib/sw.ts`, `src/sw/handler.ts`
  or `src/wire/sw.ts`. (The vendored tree's only SAB mention is
  `worker_threads`' `receiveMessageOnPort`.) PRD row
  `bn-node-worker-coop-coep` claims otherwise for this bridge and should be
  re-scoped to whatever actually needs cross-origin isolation.

## Concurrency guarantee

Two layers, both narrow on purpose:

1. **Per-path mutual exclusion inside one owner** — `serializeProvider()` puts
   every operation, reads included, on a promise chain keyed by its path;
   `rename`/`copyFile` take both paths' chains in sorted order so two of them
   cannot deadlock.

   Measured in headless Chrome against real OPFS (256 KiB payloads, 30 rounds of
   "write and read the same path with no await between them"):

   | provider                    | writes | reads | read errors | torn reads |
   | --------------------------- | ------ | ----- | ----------- | ---------- |
   | OPFS handle, unserialized   |     30 |     0 |          30 |          0 |
   | same, via `serializeProvider` |   30 |    30 |           0 |          0 |

   So "a concurrent read sees a torn file" is too kind: unserialized, every read
   *failed outright* against the in-flight write (the file is locked by
   `createWritable`), which in a guest program is an unexplained ENOENT on a file
   that is demonstrably there.
2. **One owner per browser profile** — `mountGuestFs()` takes an exclusive Web
   Lock (`hive-nodefs:<dir>`) and holds it until `dispose()`/`wipe()`, the
   primitive `asset-store.js` already uses for single-writer safety. A second
   context gets `guest-fs-busy`, never a silent second writer. Where Web Locks are
   missing the mount still happens and reports `guarantee: "per-path"` instead of
   `"single-owner+per-path"`.

**Not guaranteed, and not papered over:** cross-path atomicity (a build's writes
are not a transaction); ordering between a path and its parent directory (chains
are per path, not per subtree); another browser profile, or a second context that
ignores the lock; and `FileSystemSyncAccessHandle`, which has a real exclusive
lock but is DedicatedWorker-only and so is deliberately not used from a page.
Foreign modification is *reported* (EBUSY) and never waited on.

## Retention

**Survives** a reload, a tab close or a browser restart (same profile, same
origin): everything under the persistent mount — default `/persist`, the whole
tree beneath it, including dependencies a guest build installed.

**Lost on every reload, by design:** `/tmp` and `/dev`; everything outside the
persistent mount, since `/` remains the memory overlay; injected modules and the
run's entry point (they live in the overlay); open descriptors and unflushed
buffers.

**Wiped, and only wiped, on revocation:** the whole `hive-nodefs/<project>`
subtree, by `wipeGuestFs()`. Called from one place — the terminal-admission-denial
arm of `renewOnce()` in `ui/public/run-node-worker.js`, the same event that wipes
the CRR replica.

An ordinary `stop()` deliberately does **not** wipe — it only unmounts. Unlike the
database replica, whose system of record is the converged fleet set, this tree
exists only in the donor's browser; a donor turning the node off and on again gets
their filesystem back. A refused sync round is not revocation either.

## Naming and isolation

A tenant-controlled project name never becomes a path: it is sanitized to
`[a-z0-9._-]` and used as a single entry name under the platform-owned
`hive-nodefs` directory — the same invariant as `hive-vol-{sanitize_tag(project)}`.
`"../../etc"` becomes `etc`; a name with no usable characters (`".."`, `"/"`, `""`)
is rejected rather than degraded onto a shared tree. Wipe validates the name
before it reaches `removeEntry`, and only ever removes `hive-nodefs` or a
sanitized child of it. The guest tree is separate from `hive-crsql/`, so the two
lanes' retention never collide.

## Known gaps

- **No quota.** The guest tree is bounded only by the origin's storage quota; a
  runaway guest can fill it, and the only backstop is the browser's own ENOSPC.
  A per-project `max_bytes` (measured with the provider's `statfs` and persisted
  usage) is the follow-up.
- The vendored `createDirectoryHandleProvider` is the provider this module is
  written against, but the node-worker artifact is not built in this environment,
  so the end-to-end mount — and the synchronous bridge over it — is reasoned from
  the vendored source, not yet witnessed live.
- A File System Access grant does not survive a reload, so a donor on that
  backend re-picks (or re-grants) their directory on the next boot.
