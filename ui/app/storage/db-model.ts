/**
 * The Storage page's ONE database model.
 *
 * Two backends feed it and neither is a special case in the UI:
 *
 *  * **Managed engines** — `GET /v1/databases` (`crates/hive-cloud/src/databases.rs`).
 *    Postgres/Redis get a real external wire endpoint at `<slug>.{HIVE_DB_DOMAIN}`
 *    (TLS-SNI proxied by `db_gateway.rs`); blob/queue/vector/pubsub/realtime are
 *    HTTP kinds served by the platform API.
 *  * **Browser-replicated SQLite** — the `browser_db` contract
 *    (`docs/browser-db-contract.md`). Database identity IS the project, the spec
 *    rides the deployment manifest, and the fleet keeps one cr-sqlite replica per
 *    project on every node.
 *
 * A SQLite database is a database. It gets the same row, the same detail page and
 * the same affordances as the rest. The browser-replicated lane's difference is
 * HOW clients connect: browsers sync a local replica (no DSN), while servers use
 * its libsql/Hrana + Upstash-style REST endpoint (`browser_db_rest`) with a
 * minted per-project token — see `sqliteConnectivity()`.
 */

import type { Database, DbKind, Deployment, BrowserDbPolicy } from "@/lib/api";

/**
 * Two genuinely different SQLite objects, never collapsed into one kind:
 *  * `sqlite` — the MANAGED engine (`DbKind::Sqlite`). One file per database on
 *    its owning node, spoken over libsql/Hrana, with a real DSN.
 *  * `browser_sqlite` — the `browser_db` contract. One cr-sqlite CRDT replica
 *    per PROJECT, replicated into admitted browsers over `Op::CrrSync`, with no
 *    query endpoint at all.
 * They share the word SQLite and nothing else, so a row must state which it is.
 */
export type UnifiedKind = DbKind | "browser_sqlite";

export const KIND_LABEL: Record<UnifiedKind, string> = {
  postgres: "Postgres",
  redis: "Redis",
  blob: "Blob",
  queue: "Queue",
  vector: "Vector",
  pubsub: "Pub/Sub",
  realtime: "Realtime",
  sqlite: "SQLite",
  browser_sqlite: "SQLite (browser)",
};

// ---------------------------------------------------------------------------
// Endpoint reachability — an honest label on every published address.
// ---------------------------------------------------------------------------

/** Where a published address can actually be reached FROM. */
export type Reach =
  /** Routable from anywhere (the `<slug>.{db_domain}` DB gateway is enabled). */
  | "external"
  /** Loopback/RFC1918 only — reachable from the host node, not from a laptop. */
  | "internal"
  | "unknown";

/** Host of a URL-ish string, userinfo and port stripped. "" when unparseable. */
export function endpointHost(value: string): string {
  const m = /^[a-z0-9+.-]+:\/\/([^/?#]+)/i.exec(value.trim());
  if (!m) return "";
  const authority = m[1];
  const at = authority.lastIndexOf("@");
  const hostport = at >= 0 ? authority.slice(at + 1) : authority;
  if (hostport.startsWith("[")) {
    const end = hostport.indexOf("]");
    return end > 0 ? hostport.slice(1, end) : hostport;
  }
  const colon = hostport.lastIndexOf(":");
  return colon > 0 ? hostport.slice(0, colon) : hostport;
}

export function reachOfHost(host: string): Reach {
  const h = host.trim().toLowerCase();
  if (!h) return "unknown";
  if (h === "localhost" || h === "::1" || h.startsWith("127.")) return "internal";
  if (/^10\./.test(h)) return "internal";
  if (/^192\.168\./.test(h)) return "internal";
  if (/^172\.(1[6-9]|2\d|3[01])\./.test(h)) return "internal";
  return "external";
}

export const REACH_NOTE: Record<Reach, string> = {
  external: "Reachable from anywhere over TLS.",
  internal:
    "Node-local address — the DB gateway (HIVE_DB_DOMAIN) is not enabled for this database, so this connection string only resolves on the node running the engine, not from your machine.",
  unknown: "Address published by the backend; reachability could not be classified here.",
};

// ---------------------------------------------------------------------------
// Managed-engine endpoints
// ---------------------------------------------------------------------------

export interface EndpointInfo {
  /** The env key the platform publishes this under (DATABASE_URL / REDIS_URL / endpoint). */
  key: string;
  /** host:port as published. NEVER a secret — the copy action fetches the real DSN. */
  address: string;
  reach: Reach;
  /** A protocol hint for the UI ("postgres wire", "redis wire", "https"). */
  protocol: string;
}

/**
 * The primary connection endpoint of a managed database, derived from the
 * ALWAYS-UNMASKED `host`/`port`/`endpoint` keys (`Database::masked()` only masks
 * keys containing url/password/token/secret, so the address is safe to read from
 * the list poll while the DSN itself is fetched on demand from `/credentials`).
 */
export function managedEndpoint(db: Database): EndpointInfo | null {
  const c = db.connection ?? {};
  const host = c.host ?? "";
  const port = c.port ?? "";
  if (db.kind === "postgres" && host) {
    return {
      key: "DATABASE_URL",
      address: port ? `${host}:${port}` : host,
      reach: reachOfHost(host),
      protocol: "postgres wire",
    };
  }
  if (db.kind === "redis" && host) {
    return {
      key: "REDIS_URL",
      address: port ? `${host}:${port}` : host,
      reach: reachOfHost(host),
      protocol: "redis wire (TLS)",
    };
  }
  // Managed SQLite publishes `libsql://<host>` — the DSN a libsql/Turso client
  // takes verbatim. The token rides separately (`LIBSQL_AUTH_TOKEN`), so the
  // address shown here stays non-secret like every other kind's.
  if (db.kind === "sqlite") {
    const url = c.endpoint ?? "";
    const shown = host || endpointHost(url);
    if (shown) {
      return {
        key: "LIBSQL_URL",
        address: shown,
        reach: reachOfHost(shown),
        protocol: "libsql (Hrana over HTTPS)",
      };
    }
  }
  const endpoint = c.endpoint ?? "";
  if (endpoint) {
    return {
      key: "endpoint",
      address: endpointHost(endpoint) || endpoint,
      reach: reachOfHost(endpointHost(endpoint)),
      protocol: "https",
    };
  }
  return null;
}

/** Why a managed database shows no endpoint — always specific, never blank. */
export function managedNoEndpointReason(db: Database): string {
  if (db.status === "provisioning") return "Still provisioning — the endpoint appears once the engine is up.";
  if (db.kind === "postgres" || db.kind === "redis") {
    return "No address published yet. Wire endpoints come from the DB gateway (HIVE_DB_DOMAIN); until it is configured this database has only its node-local port.";
  }
  if (db.kind === "sqlite") {
    return "No libsql URL published yet — the backend publishes one on the DB gateway hostname, or on the platform API when that domain is unset.";
  }
  return `${KIND_LABEL[db.kind]} is served by the platform API under this database's token — see the connection details.`;
}

// ---------------------------------------------------------------------------
// Browser-replicated SQLite (browser_db)
// ---------------------------------------------------------------------------

/**
 * Mirrors `crate::git::sanitize_tag` (hive-cloud) — the platform-controlled
 * template that turns a project name into the replica file stem. Kept here so
 * the page can SHOW the real file name (`hive-browserdb-<tag>.db`) without ever
 * letting a tenant string become a path: this is display-only, the fleet derives
 * its own name server-side and refuses a mismatch.
 */
export function sanitizeTag(s: string): string {
  let out = s
    .toLowerCase()
    .split("")
    .map((c) => (/[a-z0-9._-]/.test(c) ? c : "-"))
    .join("");
  while (out.includes("--")) out = out.replace(/--/g, "-");
  out = out.replace(/^[-._]+/, "").replace(/[-._]+$/, "");
  return out || "app";
}

/** `hive-browserdb-{sanitize_tag(project)}.db` — `browser_db::replica_file_name`. */
export function replicaFileName(project: string): string {
  return `hive-browserdb-${sanitizeTag(project)}.db`;
}

/** Route id for a SQLite database. Identity is the project (contract §6). */
export const SQLITE_ID_PREFIX = "sqlite_";
export function sqliteRouteId(project: string): string {
  return `${SQLITE_ID_PREFIX}${project}`;
}
/** null when `id` is not a SQLite route id. */
export function projectFromRouteId(id: string): string | null {
  if (!id.startsWith(SQLITE_ID_PREFIX)) return null;
  const raw = id.slice(SQLITE_ID_PREFIX.length);
  if (!raw) return null;
  try {
    return decodeURIComponent(raw);
  } catch {
    return raw;
  }
}

export interface SqliteDb {
  project: string;
  policy: BrowserDbPolicy;
  /** A Ready deployment carries the block — the manifest is what the fleet acts on. */
  live: boolean;
  /** The dashboard-managed `ProjectSettings.browser_db` mirror exists. */
  configured: boolean;
  /** Newest Ready deployment carrying the block (null when none yet). */
  liveDeployment: Deployment | null;
  /** Newest deployment of the project — the redeploy target. */
  redeployTarget: Deployment | null;
  replicaFile: string;
  /** Server-derived REST/Hrana URLs (`browser_db_rest`), when the lane's poll
   *  carried them. Display-only — the token gates actual use. */
  restUrls?: { libsql_url: string; sql_url: string };
}

export const SQLITE_NO_DSN =
  "Browsers never use a connection string — each admitted browser holds a full " +
  "local replica and queries it in-process. Servers, CI and curl use this " +
  "database's libsql/REST endpoint with a minted access token (below).";

/**
 * The honest capability statement for the SQLite lane. Every clause is a fact
 * from `docs/browser-db-contract.md` / `crates/hive-cloud/src/browser_db.rs` /
 * `crates/hive-cloud/src/browser_db_rest.rs`, not a roadmap promise.
 */
export function sqliteConnectivity(): { how: string[]; missing: string[] } {
  return {
    how: [
      "Each admitted browser holds a full read-write replica in OPFS and your app queries it locally through the run-node worker — no network round trip, no DSN.",
      "The fleet keeps one cr-sqlite replica per project on every node; it converges with browsers over the authenticated sync exchange (Op::CrrSync on the hive/browser/0 ALPN), which is a change-batch merge, not a query protocol.",
      "Servers, CI jobs and curl reach the same replica over plain HTTP: a libsql/Hrana endpoint (v2/v3 pipeline — any libsql/Turso client) and an Upstash-style POST /sql JSON endpoint, both gated by a per-project access token you mint here (team = read+write, public = read-only).",
      "Access is granted per scope. Team scope is read-write; public read (when enabled) is read-only, re-checked live on every request.",
    ],
    missing: [
      "Interactive Hrana streams (a baton spanning several HTTP requests) and the v3 cursor are not supported in v1 — every pipeline request is self-contained; use the batch request type for atomic multi-statement groups.",
      "There is no server-side connection pool for it, because there are no persistent server-side connections to pool — each REST request is one short-lived connection behind a per-project concurrency cap.",
      "Server-side (Node/Python function) access with an injected DSN / host op is deliberately deferred — contract §9 lists it as not yet designed; the REST endpoint is the server path today.",
    ],
  };
}

/**
 * Server-side pooling, stated exactly as it is for the managed engines. The
 * SQL-over-HTTP surface (`db_rest.rs`) opens a FRESH connection per request and
 * bounds concurrency with a per-database semaphore; it is an admission cap, not
 * a pooler, and the code says so.
 */
export const POOLING_NOTE =
  "Postgres/Redis: no server-side pooler — the wire endpoints proxy straight to the engine, and the SQL-over-HTTP surface opens a fresh connection per request behind a per-database concurrency cap (16), so pool in your client. Managed SQLite is different: it has a real per-database pool (bounded live connections, a bounded idle set and a wait queue), so a burst queues instead of being refused.";

// ---------------------------------------------------------------------------
// The unified row
// ---------------------------------------------------------------------------

export interface UnifiedDb {
  id: string;
  href: string;
  name: string;
  kind: UnifiedKind;
  provider: string;
  project: string;
  region: string;
  regionTitle: string;
  replicas: string[];
  status: "ready" | "provisioning" | "error";
  /** Short qualifier rendered next to the status ("sim", "pending redeploy", …). */
  statusNote: string;
  createdMs: number;
  /** What `createdMs` MEANS for this row (managed rows: provisioned). */
  createdTitle: string;
  endpoint: EndpointInfo | null;
  noEndpointReason: string;
  source: "managed" | "sqlite";
  /** Present iff `source === "sqlite"`. */
  sqlite?: SqliteDb;
  /** Present iff `source === "managed"`. */
  managed?: Database;
}

function newestReady(deployments: Deployment[]): Deployment | null {
  let best: Deployment | null = null;
  for (const d of deployments) {
    if (d.state !== "ready") continue;
    if (!best || (d.production && !best.production) || d.created_at_ms > best.created_at_ms) best = d;
  }
  return best;
}

function newestAny(deployments: Deployment[]): Deployment | null {
  let best: Deployment | null = null;
  for (const d of deployments) {
    if (!best || (d.production && !best.production) || d.created_at_ms > best.created_at_ms) best = d;
  }
  return best;
}

/**
 * Every SQLite database the tenant has, from BOTH truthful sources:
 *   * a Ready deployment whose gossiped manifest carries `browser_db` (live), and
 *   * the dashboard-managed `ProjectSettings.browser_db` mirror (configured).
 *
 * The old panel listed only the second, so a project whose fluid.json declares
 * the block by hand — the documented primary opt-in — was invisible in the
 * dashboard while being fully live on the fleet. The manifest wins on a tie: it
 * is what the fleet actually reconciles against.
 */
export function sqliteDatabases(
  deployments: Deployment[],
  settings: Record<string, BrowserDbPolicy | null | undefined>,
  restUrls?: Record<string, { libsql_url: string; sql_url: string }>,
): SqliteDb[] {
  const byProject = new Map<string, Deployment[]>();
  for (const d of deployments) {
    const list = byProject.get(d.project);
    if (list) list.push(d);
    else byProject.set(d.project, [d]);
  }

  const projects = new Set<string>();
  for (const [project, list] of byProject) {
    if (list.some((d) => d.state === "ready" && d.browser_db)) projects.add(project);
  }
  for (const [project, policy] of Object.entries(settings)) {
    if (policy) projects.add(project);
  }

  return Array.from(projects)
    .sort()
    .map((project) => {
      const list = byProject.get(project) ?? [];
      const liveDeployment = newestReady(list.filter((d) => !!d.browser_db));
      const policy = liveDeployment?.browser_db ?? settings[project] ?? null;
      return {
        project,
        // A project can only reach this list with a policy from one source or the
        // other; the fallback keeps the type total without inventing caps.
        policy: policy ?? { max_bytes: 0, max_value_bytes: 0, public_read: false, schema: [] },
        live: !!liveDeployment,
        configured: !!settings[project],
        liveDeployment,
        redeployTarget: newestAny(list),
        replicaFile: replicaFileName(project),
        restUrls: restUrls?.[project],
      };
    });
}

export function sqliteRow(db: SqliteDb): UnifiedDb {
  // The libsql base URL is shown host-only, exactly like the managed kinds
  // show host:port — the address is not the secret, the minted token is.
  const restHost = db.restUrls ? endpointHost(db.restUrls.libsql_url) : "";
  return {
    id: sqliteRouteId(db.project),
    href: `/storage/${encodeURIComponent(sqliteRouteId(db.project))}`,
    name: db.project,
    kind: "browser_sqlite",
    provider: "OpenEdge SQLite (browser-replicated)",
    project: db.project,
    region: "global",
    regionTitle:
      "Replicated on every fleet node and in every admitted browser — no primary region to choose.",
    replicas: [],
    status: db.live ? "ready" : "provisioning",
    statusNote: db.live ? "replicated" : db.configured ? "pending redeploy" : "not deployed",
    createdMs: db.liveDeployment?.created_at_ms ?? 0,
    createdTitle: db.live
      ? "Live since the deployment that carries this project's browser_db block."
      : "Not carried by a Ready deployment yet — redeploy to apply.",
    endpoint: restHost
      ? {
          key: "LIBSQL_URL",
          address: restHost,
          reach: reachOfHost(restHost),
          protocol: "libsql + REST (token-gated)",
        }
      : null,
    noEndpointReason: restHost ? "" : SQLITE_NO_DSN,
    source: "sqlite",
    sqlite: db,
  };
}

export function managedRow(db: Database): UnifiedDb {
  const endpoint = managedEndpoint(db);
  return {
    id: db.id,
    href: `/storage/${encodeURIComponent(db.id)}`,
    name: db.name,
    kind: db.kind,
    provider: db.provider || KIND_LABEL[db.kind],
    project: db.project,
    region: db.region,
    regionTitle: "Primary region of the backing engine.",
    replicas: db.replicas ?? [],
    status: db.status,
    statusNote: db.mode === "simulated" ? "sim" : "",
    createdMs: db.created_ms,
    createdTitle: "When this database was provisioned.",
    endpoint,
    noEndpointReason: endpoint ? "" : managedNoEndpointReason(db),
    source: "managed",
    managed: db,
  };
}

/** One list, ordered newest-first with undated rows last, name-tiebroken. */
export function unifiedDatabases(managed: Database[], sqlite: SqliteDb[]): UnifiedDb[] {
  const rows = [...managed.map(managedRow), ...sqlite.map(sqliteRow)];
  rows.sort((a, b) => (b.createdMs || 0) - (a.createdMs || 0) || a.name.localeCompare(b.name));
  return rows;
}
