# Guardian Sentinel — Terminal UI for GuardianDB

> A terminal user interface (TUI) for **inspecting, managing, and monitoring**
> GuardianDB. GuardianDB is a library — historically every interaction happened
> through Rust code. Sentinel turns the database into something an operator or
> developer can **drive visually**, and everything created through it **survives a
> restart**. The explicit goal: *"GuardianDB used by everyone"* — including people
> who do not write Rust.

> **Naming note.** Earlier planning docs call the panel `guardian-admin` and the
> Cargo feature `admin`. In the shipped code these are named **`guardian-sentinel`**
> / feature **`sentinel`**, with the RPC server binary **`guardian-sentinel-server`**
> and the module under [`src/sentinel/`](../src/sentinel/). This document uses the
> shipped names throughout. The RPC wire protocol still uses `admin`-prefixed
> internals (`AdminSource`, `AdminContext`, `AdminClient`, `GUARDIAN_ADMIN_TOKEN`).

---

## Table of contents

1. [Quick start](#1-quick-start)
2. [Architecture](#2-architecture)
3. [The Admin RPC seam](#3-the-admin-rpc-seam)
4. [Wire protocol](#4-wire-protocol)
5. [Operation catalog](#5-operation-catalog)
6. [Screens and features](#6-screens-and-features)
7. [Store lifecycle & management](#7-store-lifecycle--management)
8. [P2P sharing and replication](#8-p2p-sharing-and-replication)
9. [Usability for non-Rust operators](#9-usability-for-non-rust-operators)
10. [Security](#10-security)
11. [Keyboard reference](#11-keyboard-reference)
12. [Persistence & relaunch](#12-persistence--relaunch)
13. [Known limitations](#13-known-limitations)
14. [Testing](#14-testing)

---

## 1. Quick start

Everything lives behind the `sentinel` feature; default builds are
unaffected.

### Embedded mode (own an offline data-dir)

The panel opens the storage directly. This only works on a `data-dir` that no
other process is using, because redb (and Iroh's internal stores) hold an
**exclusive per-process file lock**.

```bash
cargo run --features sentinel --bin guardian-sentinel -- --data-dir ./my_db
```

On an empty data-dir you get an onboarding quickstart; press `n` to create your
first store.

### Attached mode (inspect a live instance)

Run the RPC server in the owner process, then point any number of panels at it
over a socket — **without contending for the redb lock**.

```bash
# Owner process — the only one that touches storage
cargo run --features sentinel --bin guardian-sentinel-server -- \
  --addr 127.0.0.1:15433 \
  --data-dir ./guardian_data \
  --token "$GUARDIAN_ADMIN_TOKEN"     # optional; gates all action ops

# Panel — connects over the socket, holds no lock
cargo run --features sentinel --bin guardian-sentinel -- \
  --connect 127.0.0.1:15433 \
  --token "$GUARDIAN_ADMIN_TOKEN"
```

The default RPC address is `127.0.0.1:15433` (`sentinel::DEFAULT_ADDR`), sitting
right next to the pgwire gateway's `15432`. The server also reads the token from
the `GUARDIAN_ADMIN_TOKEN` environment variable if `--token` is omitted.

| Flag | Binary | Meaning |
|---|---|---|
| `--data-dir <path>` | both | Storage directory (owner). Embedded mode for the panel. |
| `--connect <addr>` | `guardian-sentinel` | Attach to a live server over RPC. |
| `--addr <addr>` | `guardian-sentinel-server` | Bind address (default `127.0.0.1:15433`). |
| `--token <t>` | both | Shared auth token. On the server it gates all ops; on the panel it authenticates. |

---

## 2. Architecture

```
┌─────────────────────────────────────────────────┐
│                 guardian-sentinel                │
│  ┌───────────┐ ┌───────────┐ ┌───────────────┐   │
│  │ Dashboard │ │ Inspectors│ │   Monitors    │   │
│  │  (home)   │ │ (stores)  │ │ (net/sync)    │   │
│  └─────┬─────┘ └─────┬─────┘ └──────┬────────┘   │
│        └──────────────┼──────────────┘           │
│               ┌───────▼────────┐                 │
│               │  State Machine │                 │
│               │  (enum Screen) │                 │
│               └───────┬────────┘                 │
│        ┌──────────────┼──────────────┐           │
│   ┌────▼────┐   ┌──────▼──────┐ ┌─────▼─────┐    │
│   │ Terminal│   │  Event      │ │  Tokio    │    │
│   │  Input  │   │  stream     │ │  refresh  │    │
│   └─────────┘   └─────────────┘ └───────────┘    │
└───────────────────────┬─────────────────────────┘
                        │  AdminSource (trait)
          ┌─────────────┴──────────────┐
   ┌──────▼───────┐             ┌───────▼───────┐
   │ EmbeddedSource│            │  AdminClient  │
   │ (owns data-dir)│           │  (socket)     │
   └──────┬───────┘             └───────┬───────┘
          │ direct                      │ RPC
   ┌──────▼───────┐             ┌───────▼─────────────────┐
   │  GuardianDB  │             │ guardian-sentinel-server │
   │  IrohClient  │             │ (owns GuardianDB)        │
   └──────────────┘             └──────────────────────────┘
```

**The central decoupling.** Inspection and management no longer talk to storage
directly. An **Admin RPC** (see [§3](#3-the-admin-rpc-seam)) exposes every
operation through an `AdminSource` seam with two interchangeable backends:

- **`EmbeddedSource`** — owns the `data-dir`, opens `GuardianDB` directly.
- **`AdminClient`** — a socket client that speaks to `guardian-sentinel-server`.

The panel's entire render/state layer is identical for both; only *where the data
comes from* changes.

### Technology stack

| Component | Technology | Rationale |
|---|---|---|
| Rendering | `ratatui` 0.30 | Already a project dependency |
| Async runtime | `tokio` (full) | Already a project dependency |
| Reactive events | GuardianDB `EventBus` | Real-time updates |
| Serialization | `serde_json` | JSON-lines RPC framing |
| Persistence | `redb` | Store registry + admin keystore |
| Image preview | `image` + `ratatui-image` | Blob preview |

The `sentinel` feature enables `dep:image` and `dep:ratatui-image`.

### State machine

The panel is a state machine over an `enum Screen`. Rendering (`render()`) is
kept separate from state (`AppState`) so state transitions are unit-testable:

```rust
enum Screen {
    Dashboard,
    StoreDetail { store_name: String },
    EventLogInspector { log_name: String },
    KeyValueInspector { kv_name: String },
    DocumentInspector { store_name: String },
    AccessControlManager,
    AccessControlDetail { controller_id: String },
    ReplicationMonitor,
    PeerDetail { node_id: String },
    NetworkTopology,
    EventBusExplorer,
    KeystoreManager,
    KeyDetail { key_id: String },
    BlobBrowser,
    BlobDetail { hash: String },
}
```

The main loop multiplexes three sources with `tokio::select!`: terminal input
(crossterm), the event stream (`events.subscribe`), and a 1s refresh timer.

---

## 3. The Admin RPC seam

### Why it exists: dissolving the redb lock

Originally the panel opened the storage directly (`IrohClient::new` +
`GuardianDB::new` over a `data-dir`). Because redb — and Iroh's internal stores —
hold an **exclusive per-process file lock**, the panel only worked on an
empty/own data-dir; pointing it at a live instance failed to open.

The pgwire gateway already solved this exact problem for SQL: the process running
`serve(...)` is the only one touching storage; any number of `psql`/TypeORM
clients speak to it over TCP. The Admin RPC generalizes that pattern to
administration operations (stores, peers, ACL, keystore, blobs, events).

```
        BEFORE (lock conflict)                  AFTER (RPC)
  ┌──────────┐   ┌──────────┐          ┌──────────┐   ┌──────────┐
  │ app/prod │   │ sentinel │          │ app/prod │   │ sentinel │
  │ (owns    │   │ (opens   │          │ = owns   │◀─▶│ (client, │
  │ data-dir)│   │ SAME dir)│          │ data-dir │RPC│ no redb) │
  └────┬─────┘   └────┬─────┘          └────┬─────┘   └──────────┘
       │ redb lock ✗──┘                     │ redb lock (owner only)
       ▼ CONFLICT                           ▼ OK
```

### Mirroring the pgwire model

The RPC reuses the pgwire three-layer pattern as a *pattern*, not as code:

| pgwire (`src/pgwire/mod.rs`) | Admin RPC (`src/sentinel/`) |
|---|---|
| `serve(addr, db, user)` | `sentinel::serve(addr, ctx)` |
| `GuardianFactory` (per connection) | connection handler (per connection) |
| `GuardianHandler` (owns `Session`) | handler owns `AdminContext` |
| `process_socket` (Postgres protocol) | `process_admin_conn` (protocol below) |

### `AdminContext`

`AdminContext` is the shared, clonable (`Arc`) handle to the owner process's live
resources. Construction has two paths:

- **`AdminContext::with_keystore(...)`** — in-memory keystore, for dev/tests; no
  stores are reopened.
- **`AdminContext::with_data_dir(db, iroh, data_dir)`** — the full owner setup.
  Opens a **persistent** `RedbKeystore` under `<data-dir>/admin_keystore` (managed
  keys, survives restart — *not* bound to the node identity, which lives in
  `identity.json`) and the `StoreRegistry` under `<data-dir>/store_registry`. Call
  **`AdminContext::reopen_stores()`** once after this to reopen every registered
  store on boot.

### The two backends

```rust
#[async_trait]
pub trait AdminSource: Send + Sync {
    async fn stores_list(&self) -> Result<Vec<StoreInfo>>;
    async fn acl_grant(&self, /* … */) -> Result<()>;
    // … one method per op
}

struct EmbeddedSource(/* Arc<GuardianDB>, IrohClient, StoreRegistry, … */);
struct AdminClient(/* socket */);
```

- `guardian-sentinel --data-dir ./x` → `EmbeddedSource` (owns the storage; useful
  for offline / self-owned DBs). Still shows the lock-error screen if the dir is
  in use.
- `guardian-sentinel --connect 127.0.0.1:15433` → `AdminClient` (inspects a live
  super-peer **without** contending for the lock).

---

## 4. Wire protocol

- **Transport:** loopback TCP (`127.0.0.1:15433`) by default. External exposure
  requires an explicit flag plus auth.
- **Framing:** newline-delimited JSON (`\n`) — trivial to debug with `nc`.
- **Correlation:** every request has an `id`; replies echo it. This allows
  pipelining and out-of-order replies, which streams require.

```jsonc
// → request
{ "id": 7, "op": "stores.list" }
{ "id": 8, "op": "acl.grant", "args": { "controller": "…", "role": "write", "key_id": "…" } }
{ "id": 9, "op": "events.subscribe", "args": { "kinds": ["sync", "peer"] } }

// ← single reply
{ "id": 7, "ok": true, "data": { "stores": [ … ] } }
{ "id": 8, "ok": false, "error": { "code": "acl_denied", "message": "…" } }

// ← stream (multiple messages with the same id, until "end")
{ "id": 9, "event": { "kind": "sync", "peer": "abcd…", "ts": "…" } }
{ "id": 9, "end": true }
```

```rust
#[derive(Deserialize)]
struct AdminRequest { id: u64, op: String, #[serde(default)] args: serde_json::Value }

#[derive(Serialize)]
#[serde(untagged)]
enum AdminReply {
    Ok    { id: u64, ok: bool, data: serde_json::Value },
    Err   { id: u64, ok: bool, error: AdminError },
    Event { id: u64, event: serde_json::Value },
    End   { id: u64, end: bool },
}

#[derive(Serialize)]
struct AdminError { code: &'static str, message: String }
```

Each connection accepts requests, spawns a task per request (so long-lived
streams do not block subsequent requests), and writes replies/events through a
shared, mutex-guarded writer.

### `AdminEvent` enrichment

The streaming events carry structured fields, not just a flat summary. The core
emits rich typed events (`EventSyncCompleted`, `EventStoreUpdated`,
`EventSyncError`, …), and the RPC surface propagates the structured fields
(`#[serde(default)]`):

```rust
AdminEvent { kind, detail, store: Option<String>, peer: Option<String>,
             duration_ms: Option<u64>, heads_synced: Option<u64>, ts: Option<String> }
```

This enrichment is the key lever behind several "not exposed" features that were
in fact **derivable from the event stream**: peers-per-store, stores-per-peer,
sync durations, in-flight syncs, and top-peers-by-volume.

---

## 5. Operation catalog

All ops are implemented end-to-end (embedded + client + server + mock) with e2e
tests. Action/write ops are gated by the connection token (see [§10](#10-security)).

| Group | Ops |
|---|---|
| **Stores** | `stores.list`, `stores.create`, `stores.close`, `stores.drop`, `stores.share`, `stores.import` |
| **Node** | `node.info`, `node.identity`, `node.latency`, `node.throughput` |
| **KeyValue** | `kv.entries`, `kv.put`, `kv.delete` |
| **EventLog** | `eventlog.entries` (cursor-paginated by `before`), `eventlog.heads`, `eventlog.append` |
| **Document** | `docs.list`, `docs.get`, `docs.put`, `docs.delete` |
| **Peers** | `peers.list`, `peers.force_sync` |
| **Blobs** | `blobs.list` (real size + partial/complete), `blob.get`, `blob.add`, `blob.export`, `blob.delete` |
| **Events** | `events.subscribe` (stream, structured fields) |
| **Keystore** | `keystore.list`, `keystore.detail` (metadata + public key only), `keystore.generate` |
| **Access Control** | `acl.list`, `acl.grant`, `acl.revoke`, `acl.create` |
| **Network** | `net.topology` (real conn-type + per-peer p95/p99), `net.relay`, `net.discovered` |
| **Auth** | `auth` (handshake) |

### Management op contracts

| Op | Args | Returns | Gate |
|---|---|---|---|
| `stores.create` | `kind, name, {replicate, local_only, read_only, overwrite, acl_address}` | `address` | token |
| `stores.close` | `name` | `ok` | token |
| `stores.drop` | `name` | `ok` | token |
| `stores.share` | `name` | read + write `ticket`s | token |
| `stores.import` | `kind, name, ticket, read_only` | `address` | token |
| `node.identity` | — | `{ node_id, addresses }` | — |

---

## 6. Screens and features

Navigation is `F1`–`F7` between top-level screens; `Enter` opens a detail;
`Esc` goes back. Status legend below uses ✅ done · 🟡 partial.

### F1 · Store Dashboard ✅

Overview of every open store (EventLog / KeyValue / Document) with metrics.

- Store table: **Name, Type, Entry count, Connected peers**, with a type filter
  (`Tab`) and status indicators (🟢 synced, 🟡 syncing, 🔴 error).
- Store detail: metadata (name, type, address, entries, status); **peers observed
  syncing this store** (derived from the event stream); **document inspector** for
  `document` stores (`docs.list`/`docs.get`); action `c` to connect a new peer
  (NodeId → `peers.force_sync`).
- Management keys (see [§7](#7-store-lifecycle--management)): `n` new store wizard,
  `x` close, `d` drop, `s` share, `i` import, `y` node identity, `l` audit log.

### F2 · Network Topology ✅

ASCII star graph from this node (each node only knows its own connections).

- Link type: **direct** (solid `───`) vs **relay** (dashed `╌╌╌`); abbreviated
  NodeId label; address + connection time + op count per edge.
- **Real latency per edge** (`avg_latency_ms`), colored by quality (green <50ms,
  yellow <200ms, red above); **global** p95/p99 (`node.latency`) and **per-peer**
  p95/p99 (`peer_latency_history` → `TopoLink.p95/p99`).
- Real connection type via `remote_info` (`[type]` without `~` = real, with `~` =
  inferred from the address); **relay status** (connected + last error) via
  `net.relay`; aggregate **throughput** line (`node.throughput`); **known-but-not-
  connected** peers via `net.discovered`.

### F3 · Replication Monitor (Peers) ✅

Real-time view of sync state across peers.

- Peer list: abbreviated NodeId, status (connected/offline), address; reactive
  refresh via `events.subscribe` with a periodic safety-net refresh.
- Peer detail: full NodeId, addresses, connection type/latency; **stores shared
  with this peer** (derived from events); recent sync history (enriched with
  heads/duration); action `s` force-sync (`c` for an arbitrary NodeId).
- Real-time sync dashboard: peers online/offline, syncs/min, recent sync errors,
  **average/last sync duration** (`EventSyncCompleted.duration_ms`), **in-flight
  syncs** (heads exchanged without a completion in-window), **aggregate
  throughput** (bytes/s, ops/s via `node.throughput`), and a **60s sync
  sparkline**.
- Alerts/diagnostics: highlights peers connected without a sync for >5 min
  (◐ yellow), and a diagnostic line (sync errors / stale peers / isolated).

### F4 · Access Control Manager ✅

- Controller list: type + keys per role (`acl.list`); type indicator with
  color+icon (🔵 Simple, 🟢 Guardian, 🟣 Iroh); authorized keys per role inline.
- Controller detail: all permissions grouped by role (admin/write), full key ID
  lists, and a summary of totals.
- Grant/revoke: `g` grant, `x` revoke — key-ID input with a role selector (`Tab`
  toggles `write ↔ admin`), immediate feedback (`acl.grant`/`acl.revoke`).
- Creation wizard (`n`, `acl.create`): choose type → name → initial permissions
  (admin + write keys) → confirm; displays the created **manifest hash** in a
  persistent modal for manual copy.

### F5 · Keystore Manager ✅

Manages keys held in the admin keystore (persistent redb under
`<data-dir>/admin_keystore`; not bound to the node identity).

- Key list: key IDs with **active vs. rotated** status and key type, from a
  parallel `keystore_meta` table (`{ created_at, updated_at, kind, rotated_count }`).
- Key detail: metadata + **exportable public key** (`x`) — **never** private
  material.
- Operations: `n` generate (Ed25519), `r` rotate (generates a new pair, marks the
  old as rotated). The secret is never displayed.

### F6 · Blob Browser ✅

- Blob list: BLAKE3 hash + **real size** (`blobs().status(hash)`), download
  indicator (● complete / ◐ partial), sort by hash ↔ size (`s`), total disk usage
  in the title.
- Blob detail: full hash, real size (`blob.get`), content preview (first 512
  bytes if text; text/binary classification).
- Operations: `a` add from file (`blob.add`), `x` export to file (`blob.export`),
  `d` delete local (`blob.delete`, confirmed). File paths resolve in the **owner**
  process (relevant in `--connect` mode).

### F7 · EventBus Explorer ✅

Real-time monitor of GuardianDB's internal events.

- Stream: consumes `events.subscribe` (1000-entry ring buffer) as a scrollable
  list; each event shows timestamp, colored type, and a one-line summary.
  Follow-mode auto-scroll (`f`); pause with Space (background buffer keeps
  filling, capped 2000).
- Filters: by type (`t` cycles), by content (`/` with highlight); filters combine
  (AND).
- Statistics: per-type frequency table, events/s (5s window), **60s activity
  sparkline**, and **top peers by volume** (via the `peer` field of `AdminEvent`).

### EventLog Inspector (Enter on an EventLog) ✅

- Entry list (`eventlog.entries`, 500-entry blocks): `#`, Op, Key, payload
  preview, size; **cursor pagination** (scrolling to the top loads the previous
  block via `StreamOptions.lt`, re-numbers, keeps selection).
- Entry detail: hash, clock (`id@time`), identity/author, `next` (CRDT pointers),
  log id, and payload as pretty-printed JSON when applicable.
- Search (`/`): live substring filter over payload, author, op, key, hash; **range
  by logical clock** (Lamport, `t` → `min-max`); highlighted matches; a "N of M"
  result counter.
- CRDT heads (`h`, `eventlog.heads`): current heads with divergence indicator
  (multiple heads = ⚠ pending merge; one = ✓ converged), a simplified branch diff
  (entries exclusive to each head via DAG traversal), and a merge timeline
  (entries with >1 `next`).

### KeyValue Inspector (Enter on a KV store) ✅

- Key list (`kv.entries`): Key, truncated value preview, size; alphabetical order.
- Value detail/edit: full value (indented JSON) in a modal (`Enter`); edit mode
  (`e`) with inline JSON-validated input → `kv.put` (replicates automatically);
  `Esc` cancels.
- CRUD: `n` create (`key=value`, JSON-validated → `kv.put`), `d` delete (confirmed,
  `kv.delete`), `/` search (substring over key + value, highlighted), `x` export
  all loaded keys as JSON to a local file.

### Document Inspector (Enter on a Document store) ✅

- List/get via `docs.list` / `docs.get`, analogous to the KV inspector.
- CRUD: `n` new document (`id={json}`, `docs.put`), `d` delete (`docs.delete`).

---

## 7. Store lifecycle & management

Before this work the panel could only *inspect*. Now it is the full manageable
base — an operator with no Rust creates, writes, shares, and removes stores, and
it all survives a restart.

### Create & persist (the base)

- **`StoreRegistry`** ([`src/sentinel/store_registry.rs`](../src/sentinel/store_registry.rs))
  — a redb table `store_registry` under `<data-dir>/store_registry` mapping
  `name → StoreSpec { kind, replicate, local_only, read_only, acl_address, doc_ticket }`
  (JSON), with `put/get/list/remove/contains`. This is the **catalog of what to
  reopen** — not the store storage itself.
- **Boot reopen** — `AdminContext::with_data_dir` opens the registry;
  `AdminContext::reopen_stores()` reopens each store with its reconstructed
  `CreateDBOptions`. Called by both the embedded panel and the server at startup.
  Idempotent and tolerant of per-store failure (a corrupt/missing store is skipped
  and flagged, the rest reopen).
- **`stores.create(kind, name, opts)`** — validates name/type, refuses an
  already-open name, writes to the registry, returns the address.
- **New-store wizard (`n`)** — Type (list with descriptions) → Name → Options
  (toggles `replicate` / `local_only` / `read_only` with explanations, Space
  toggles) → optional ACL address → confirm.

### Full lifecycle

- **Close (`x`)** — `stores.close(name)` → `Store::close` + `delete_store(addr)`;
  **keeps data and registry** (reopens on restart).
- **Drop (`d`)** — `stores.drop(name)` → `Store::drop` + `delete_store` +
  `registry.remove` (deletes data + registry entry), confirmed with a
  "⚠ DELETES the data" warning.
- **Write parity** — all three writable store types are usable without code: KV
  (put/delete/edit), **EventLog append** (`a` → `eventlog.append`), **Document
  put/delete** (`n`/`d` → `docs.put`/`docs.delete`).

---

## 8. P2P sharing and replication

This is the biggest value lever for "used by everyone" — making the P2P model
usable without writing Rust.

- **Node identity in focus** — `node.identity` (via `client.id()` → NodeId +
  bound addresses). Panel: `y` on the Dashboard opens a copyable *"My identity
  (share this)"* modal.
- **Generate a `DocTicket`** — `stores.share(name)` → `share_tickets` (both
  **read** and **write** tickets; KV/Document only; EventLog returns
  `not_shareable`). Panel: `s` shows both tickets.
- **Import a store from a ticket** — `stores.import(kind, name, ticket, read_only)`
  opens via `doc_ticket` (joins the peer's namespace) and **writes to the
  registry** with the ticket, so it reopens/syncs on restart. Panel: `i` opens the
  wizard Type (kv/document) → Name → paste Ticket (`Tab` toggles read-only) →
  Enter.
- **Connect to a peer (guided)** — covered by identity (`y`, publish the NodeId)
  plus the existing force-sync (`c`) on the Network screen. The flow: *friend A
  publishes, friend B connects and imports the ticket.*

`stores.create` and `stores.import` share `EmbeddedSource::open_and_register`
(open + resolve address + persist to the registry).

---

## 9. Usability for non-Rust operators

The design principles: **lib↔TUI parity** (everything doable in code has a guided
path), **non-programmers first** (wizards over raw flags, sensible defaults, clear
language, inline validation, destructive-action confirmation, contextual help),
**consistency** (every mutation follows the same op → token-gate → confirm →
uniform feedback → e2e-test pattern), **persistence** (what you create survives
relaunch), and **security by default**.

- **Contextual help (`?`)** — on any screen, a large overlay (`help_for_screen`)
  in plain language: what the screen is, what you can do (with the shortcuts), and
  the concepts without jargon (EventLog vs KeyValue vs Document, replicate, ACL,
  direct/relay, p95/p99, logical clock). Any key closes it.
- **Action-oriented empty states** — every empty list suggests the next key:
  stores (`n`), KV (`n`), Document (`n`), EventLog (`a`), ACL (`n`), Keystore
  (`n`), Blobs (`a`).
- **Onboarding / quickstart** — on an empty data-dir (0 stores), a 3-step guide
  appears at startup (create → write → share/connect), plus hints.
- **Labels & descriptions** — wizards carry one line per option; the concept
  glossary lives in each screen's `?`.
- **Validation & confirmation** — inline validation (JSON / name / ticket) and
  destructive-action confirmation. A literal "undo" is out of scope (actions are
  reversible through existing paths: re-grant, recreate, etc.).

---

## 10. Security

- **Loopback by default** — the RPC binds `127.0.0.1`; external exposure requires
  an explicit flag plus auth.
- **Token gating, per connection** — with `--token` (or `GUARDIAN_ADMIN_TOKEN`),
  the server blocks every op until the client authenticates via the `auth`
  handshake. Because the gate is per-connection, **all** create/drop/share/import
  and other writes are covered in `--connect` mode. (Covered by
  `token_auth_gates_ops`, which asserts `stores.create`/`stores.drop` return
  `unauthorized` without auth.)
- **Never serialize private key material** — `keystore.detail` returns only
  metadata + the derived public key. The panel never displays a secret.
- **Destructive confirmation on the client** — the server merely executes; the
  panel confirms "Are you sure? [y/N]" for destructive ops.
- **Audit trail** — `App.audit_log: VecDeque<(time, description)>` (client-side,
  capped at 200). `App::audit(...)` fires on every sensitive mutation
  (create/close/drop/import store, grant/revoke, create controller,
  generate/rotate key, delete KV/document/blob, generate tickets). Viewable via
  `l` on the Dashboard.
- **Input sanitation** — NodeIds and addresses are sanitized before use; sensitive
  payloads are not logged to the status bar.

---

## 11. Keyboard reference

### Global

| Key | Action |
|---|---|
| `F1` Dashboard · `F2` Topology · `F3` Network · `F4` Access · `F5` Keystore · `F6` Blobs · `F7` Events | Switch top-level screen |
| `Esc` | Back (or clear an active search filter) |
| `q` | Quit |
| `/` | Open search |
| `?` | Contextual help |
| `Tab` | Toggle panel/filter |
| `Enter` | Open detail of the selected item |
| `r` | Manual data refresh |

### Dashboard management

| Key | Action |
|---|---|
| `n` | New-store wizard |
| `x` | Close store (keeps data) |
| `d` | Drop store (⚠ deletes data) |
| `s` | Share store (read + write tickets) |
| `i` | Import store from a ticket |
| `y` | Show node identity (copyable) |
| `l` | Audit log overlay |

Screen-specific keys (e.g. `e` edit, `g` grant, `h` heads, `t` type/range,
`f` follow, `a` append/add) are listed in each screen's section and its `?` help.

---

## 12. Persistence & relaunch

Everything created through Sentinel survives a restart:

- **Store registry** (`<data-dir>/store_registry`, redb) records every created
  store and its options. On boot, `reopen_stores()` reopens each with its
  `CreateDBOptions`, repopulating `db.list_stores()`. Reopening is safe because
  `db.log/key_value/docs` are **idempotent** (they reopen the existing store via
  `DatabaseAlreadyExists`).
- **Admin keystore** (`<data-dir>/admin_keystore`, redb) persists managed keys and
  their `keystore_meta` (active vs. rotated, type, timestamps).
- **Imported stores** persist their `doc_ticket` in the registry, so they
  reopen and re-sync on restart.

A partial reopen failure (corrupt/missing store data) reopens the rest and flags
the problematic one in the UI.

---

## 13. Known limitations

Documented as known limitations rather than bugs — most are architectural or
deep-core with low operational return:

| Area | Limitation | Root cause |
|---|---|---|
| Network partition detection | Not available | Each node only knows its own connections (star topology); needs a topology-gossip protocol. |
| "Which peers have this blob" | Not available | The Iroh model has no DHT/provider records; needs a content-announce mechanism. |
| Blob mime / original name | Not tracked | Blobs are raw bytes; would need a metadata sidecar. |
| EventLog wall-clock range filter | Only logical (Lamport) clock | The CRDT `Entry` carries only a Lamport clock; wall-clock needs a log-format (schema) change. Lamport-range filter is provided instead. |
| Runtime `replicate` toggle | Not editable at runtime | `replicate` is fixed at store open; change by closing (`x`) and recreating. |
| ACL controller types | All behave as `SimpleAccessController` | The core maps every controller type to Simple; the type is stored in the manifest but behavior is uniform. Documented in the ACL screen's `?`. |
| Discovery-only peer enumeration | Only `known_peers` − active | iroh 1.0 exposes no getter to list discovered-but-not-connected peers; `net.discovered` is the honest subset. |
| Per-key usage (stores/controllers using a key) | Not tracked | No reverse index; low value / high cost, deferred. |
| Clipboard | Persistent copyable modals instead | A TUI has no portable clipboard; modals allow terminal mouse selection. |

---

## 14. Testing

- **State-machine unit tests** for screen transitions (state separated from
  rendering).
- **Registry tests** — the registry survives a reopen
  (`store_registry::tests`); `stores_create_persists_in_registry` asserts a created
  store appears in `stores.list`, that a duplicate errors, and that the spec
  persists.
- **Admin RPC e2e tests** — each op is exercised over the wire against a
  `MockSource` roundtrip (10 admin tests), including the management/write ops
  (`stores.create/close/drop/share/import`, `node.identity`, `eventlog.append`,
  `docs.put/delete`) and `token_auth_gates_ops` (auth gating).
- **Robustness note** — `iroh_blobs` 0.103 `Hash::from_str` **panics** for hashes
  whose length ≠ 64/52; `parse_blob_hash` validates length before parsing (the
  pagination cursor and `blob.get/export/delete` depend on this).

---

## Appendix — feature history (waves)

The work landed in two parallel axes. All waves are complete.

**Observability (Roadmap, Waves 1–4):**

- **Wave 1 — UI quick wins:** ACL role selector, type color/icon, EventBus
  sparkline, EventLog cursor pagination, blob sort-by-hash.
- **Wave 2 — Enrich the admin layer (no core change):** structured `AdminEvent`
  fields → peers-per-store / stores-per-peer, sync duration + in-flight,
  top-peers; `docs.*` ops + document inspector; Lamport-clock range filter.
- **Wave 3 — Wrap iroh/iroh-blobs:** real blob size/state, relay status, real
  conn-type, global p95/p99.
- **Wave 4 — Core instrumentation (on demand):** aggregate throughput
  (`node.throughput`), per-peer p95/p99 (`peer_latency_history`),
  discovered-not-connected (`net.discovered`), keystore lifecycle metadata
  (`keystore_meta`). Deferred: wall-clock timestamps, per-key usage index.

**Management (Management Plan, Waves G1–G5):**

- **G1 — Create + persist + reopen stores** (the base): `StoreRegistry`, boot
  reopen, `stores.create`, new-store wizard.
- **G2 — Full lifecycle:** close/drop, EventLog append, Document CRUD, ACL at
  creation. (Deferred: runtime `replicate` edit.)
- **G3 — Sharing & replication:** node identity, generate/import tickets, guided
  peer connect.
- **G4 — Usability:** contextual help, empty states with actions, onboarding,
  labels/descriptions. (Partial: literal undo.)
- **G5 — Consistency, security, audit:** uniform token gate, client-side audit
  trail. (Documented: real ACL types are a core limitation.)
