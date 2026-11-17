import { randomBytes } from "node:crypto";

/**
 * One cohesive binding from the Lakebed capsule API to shadw storage.
 *
 * This module is the ONLY place that talks to the platform. It exports two
 * surfaces that share one transport, one credential and one validator:
 *
 *   - `database()` implements Lakebed's `ctx.db` read/write chain
 *   - `storage()`  implements Lakebed's `client.storage` object surface
 *
 * Both are backed by shadw's tenant-scoped Blob store
 * (`/v1/storage/blob/:bucket[/:key]`). Nothing here runs in a browser: the
 * server-held platform credential is read from the environment on the server
 * and is never returned to a client, never logged and never interpolated into
 * a URL from a caller-controlled value.
 *
 * The honest boundary is in README.md. This is NOT Lakebed's hosted source
 * runtime: `app.json`/`clientBundle` execution, server-side index pushdown,
 * multi-row transactions and public/unauthenticated objects are not provided.
 */

const KEY_RE = /^[A-Za-z0-9._-]{1,180}$/;
const BUCKET_RE = /^[a-z0-9][a-z0-9_-]{0,62}$/;
const TABLE_RE = /^[a-z][a-z0-9_]{0,47}$/;
const ID_RE = /^[a-f0-9]{64}$/;

/** A refusal that names the missing platform capability, never a silent no-op. */
export class ShadowBindingError extends Error {
  constructor(code, message) {
    super(message);
    this.name = "ShadowBindingError";
    this.code = code;
  }
}

function fail(code, message) {
  throw new ShadowBindingError(code, message);
}

/** Validated path segment. A tenant-influenced string never becomes a path. */
function segment(value, label, re) {
  if (typeof value !== "string" || !re.test(value) || value.includes("..")) {
    fail("INVALID_SEGMENT", `${label} is not a valid storage segment`);
  }
  return encodeURIComponent(value);
}

/** Parse a trusted base, then set the path. No string interpolation of tenant data. */
function urlFor(config, parts) {
  let base;
  try {
    base = new URL(config.apiBase);
  } catch {
    fail("BAD_BASE", "SHADOW_API_BASE is not an absolute URL");
  }
  const path = ["/v1/storage/blob", ...parts.map((p) => p.encoded)].join("/");
  base.pathname = path;
  base.search = "";
  base.hash = "";
  return base.toString();
}

function bucketPart(config) {
  return { encoded: segment(config.bucket, "bucket", BUCKET_RE) };
}

function keyPart(key) {
  return { encoded: segment(key, "key", KEY_RE) };
}

/** Server-derived configuration. Names only; values are never printed.
 *  `BLOB_ENDPOINT` is the platform-injected absolute URL; derive the API base
 *  from it so a deployment never names a loopback or a hand-built host. */
export function configFromEnv(env = process.env) {
  const endpoint = env.BLOB_ENDPOINT;
  let apiBase = env.SHADOW_API_BASE || "";
  if (!apiBase && endpoint) {
    try {
      const parsed = new URL(endpoint);
      parsed.pathname = "/";
      parsed.search = "";
      parsed.hash = "";
      apiBase = parsed.toString();
    } catch {
      fail("BAD_ENDPOINT", "BLOB_ENDPOINT is not an absolute URL");
    }
  }
  const bucket = env.BLOB_BUCKET;
  // A `blob_rw_` token is a per-database object token, NOT a platform principal:
  // admin::api_key_team only accepts a verified `hive_` API key / JWT claim, so
  // the server-held credential must be one of those. The browser never sees it.
  const token = env.HIVE_API_TOKEN;
  if (!apiBase) fail("MISSING_API_BASE", "SHADOW_API_BASE or BLOB_ENDPOINT is not set");
  if (!bucket) fail("MISSING_BUCKET", "BLOB_BUCKET is not set");
  if (!token) fail("MISSING_TOKEN", "HIVE_API_TOKEN (a platform API key) is not set");
  if (!BUCKET_RE.test(bucket) || bucket.includes("..")) {
    fail("INVALID_SEGMENT", "BLOB_BUCKET is not a valid storage segment");
  }
  return { apiBase, bucket, token, databaseId: env.BLOB_DATABASE_ID || "" };
}

async function request(config, parts, init = {}) {
  const headers = {
    authorization: `Bearer ${config.token}`,
    ...(init.headers || {}),
  };
  let response;
  const method = init.method || "GET";
  try {
    response = await fetch(urlFor(config, parts), {
      method,
      headers,
      body: init.body,
      signal: AbortSignal.timeout(10_000),
    });
  } catch (error) {
    fail("UPSTREAM_UNREACHABLE", `shadw storage did not answer: ${error.message}`);
  }
  if (method === "GET" && response.status === 404) return null;
  if (!response.ok) {
    fail("UPSTREAM_STATUS", `shadw storage answered ${response.status}`);
  }
  return response;
}

/** Read a blob as JSON, or null when absent. */
async function readJson(config, key) {
  const response = await request(config, [bucketPart(config), keyPart(key)]);
  if (response === null) return null;
  const text = await response.text();
  try {
    return JSON.parse(text);
  } catch {
    fail("CORRUPT_ROW", "stored row is not JSON");
  }
}

async function writeJson(config, key, value) {
  const body = JSON.stringify(value);
  await request(config, [bucketPart(config), keyPart(key)], {
    method: "PUT",
    headers: { "content-type": "application/json" },
    body,
  });
  return { key, size: Buffer.byteLength(body) };
}

async function listKeys(config) {
  const response = await request(config, [bucketPart(config)]);
  const payload = await response.json();
  return Array.isArray(payload?.keys) ? payload.keys.filter((k) => typeof k === "string") : [];
}

/**
 * shadw's blob REST surface has GET/PUT but no DELETE route. The sanctioned
 * deletion path is the database console's `DELETE <key>` command, which is a
 * NATIVE_WRITE: it needs `write: true` and an owning-tenant caller, not the
 * destructive tier. A typed refusal replaces a fabricated success when the
 * console database id is not configured.
 */
async function removeKey(config, key) {
  if (!config.databaseId) {
    fail(
      "DELETE_UNAVAILABLE",
      "shadw exposes no blob DELETE REST route; set BLOB_DATABASE_ID to use the database console",
    );
  }
  const id = segment(config.databaseId, "database id", KEY_RE);
  let base;
  try {
    base = new URL(config.apiBase);
  } catch {
    fail("BAD_BASE", "SHADOW_API_BASE is not an absolute URL");
  }
  base.pathname = `/v1/databases/${id}/console`;
  base.search = "";
  base.hash = "";
  let response;
  try {
    response = await fetch(base.toString(), {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${config.token}` },
      body: JSON.stringify({ input: `DELETE ${key}`, write: true }),
      signal: AbortSignal.timeout(10_000),
    });
  } catch (error) {
    fail("UPSTREAM_UNREACHABLE", `shadw console did not answer: ${error.message}`);
  }
  if (!response.ok) fail("UPSTREAM_STATUS", `shadw console answered ${response.status}`);
  const payload = await response.json().catch(() => ({}));
  if (payload?.error || payload?.ok === false) {
    fail("DELETE_REFUSED", "shadw console refused the delete");
  }
  return true;
}

function rowKey(table, id) {
  return `lb_${table}_${id}.json`;
}

function prefixFor(table) {
  return `lb_${table}_`;
}

function validateTable(table) {
  if (!TABLE_RE.test(table)) fail("INVALID_TABLE", "unknown table");
  return table;
}

/** Lakebed's index range builder, evaluated against one row at a time. */
function rangeBuilder() {
  const clauses = [];
  const api = {
    eq(field, value) { clauses.push(["eq", field, value]); return api; },
    gt(field, value) { clauses.push(["gt", field, value]); return api; },
    gte(field, value) { clauses.push(["gte", field, value]); return api; },
    lt(field, value) { clauses.push(["lt", field, value]); return api; },
    lte(field, value) { clauses.push(["lte", field, value]); return api; },
  };
  return { api, clauses };
}

function matches(row, clauses) {
  return clauses.every(([op, field, expected]) => {
    const actual = row[field];
    if (actual === undefined || actual === null) return false;
    switch (op) {
      case "eq": return actual === expected;
      case "gt": return actual > expected;
      case "gte": return actual >= expected;
      case "lt": return actual < expected;
      case "lte": return actual <= expected;
      default: return false;
    }
  });
}

/** Lakebed-shaped query chain: identity is the row set, not a color or a hue.
 *  `getRows` is lazy, so `withIndex(...).order(...).take(n)` stays synchronous
 *  exactly as Lakebed's v1 data API, while every terminal needs `await`. */
function chain(getRows) {
  const state = { clauses: [], order: "asc", limit: null, cursor: null };
  async function selectedRows() {
    const rows = await getRows();
    let selected = rows.filter((row) => matches(row, state.clauses));
    selected.sort((a, b) => {
      const byTime = (a.createdAt ?? 0) - (b.createdAt ?? 0);
      if (byTime !== 0) return state.order === "desc" ? -byTime : byTime;
      const byId = String(a.id).localeCompare(String(b.id));
      return state.order === "desc" ? -byId : byId;
    });
    return selected;
  }
  const api = {
    withIndex(_name, build) {
      const { api: builder, clauses } = rangeBuilder();
      build(builder);
      state.clauses = clauses;
      return api;
    },
    order(direction) {
      if (direction !== "asc" && direction !== "desc") fail("BAD_ORDER", "order must be asc or desc");
      state.order = direction;
      return api;
    },
    take(n) {
      if (!Number.isSafeInteger(n) || n < 1 || n > 1000) fail("BAD_TAKE", "take must be 1..1000");
      state.limit = n;
      return api;
    },
    paginate(opts) {
      state.cursor = opts?.cursor ?? null;
      if (!Number.isSafeInteger(opts?.numItems) || opts.numItems < 1 || opts.numItems > 1000) {
        fail("BAD_PAGE", "numItems must be 1..1000");
      }
      state.limit = opts.numItems;
      return api;
    },
    async collect() {
      let selected = await selectedRows();
      if (state.cursor !== null) {
        const at = selected.findIndex((row) => row.id === state.cursor);
        selected = at >= 0 ? selected.slice(at + 1) : [];
      }
      const capped = state.limit !== null ? selected.slice(0, state.limit) : selected;
      return state.cursor !== null
        ? { page: capped, continueCursor: capped.at(-1)?.id ?? null, isDone: capped.length < state.limit }
        : capped;
    },
    async first() {
      const priorLimit = state.limit;
      state.limit = 1;
      const rowsOut = await api.collect();
      state.limit = priorLimit;
      return (Array.isArray(rowsOut) ? rowsOut : rowsOut.page)[0] ?? null;
    },
    async count() {
      const priorLimit = state.limit;
      state.limit = null;
      const rowsOut = await api.collect();
      state.limit = priorLimit;
      return rowsOut.length;
    },
  };
  return api;
}

export function database(config) {
  const cache = new Map();
  async function rows(table) {
    validateTable(table);
    if (!cache.has(table)) {
      const keys = (await listKeys(config)).filter((k) => k.startsWith(prefixFor(table)));
      const loaded = [];
      for (const key of keys.slice(0, 1000)) {
        const row = await readJson(config, key);
        if (row && typeof row === "object") loaded.push(row);
      }
      cache.set(table, loaded);
    }
    return cache.get(table);
  }
  function invalidate(table) { cache.delete(table); }

  /** Build `ctx.db` from a declared capsule schema, so `ctx.db.todos` is a
   *  Lakebed-shaped table handle whose query chain stays synchronous. */
  function forSchema(schema) {
    const db = {};
    for (const name of Object.keys(schema)) {
      validateTable(name);
      const spec = schema[name];
      const getRows = () => rows(name);
      db[name] = {
        get: (id) => tableHandle(name, spec).get(id),
        insert: (document) => tableHandle(name, spec).insert(document),
        update: (id, patch) => tableHandle(name, spec).update(id, patch),
        delete: (id) => tableHandle(name, spec).delete(id),
        withIndex: (index, build) => chain(getRows).withIndex(index, build),
        order: (direction) => chain(getRows).order(direction),
        take: (n) => chain(getRows).take(n),
        collect: () => chain(getRows).collect(),
        first: () => chain(getRows).first(),
        count: () => chain(getRows).count(),
        paginate: (opts) => chain(getRows).paginate(opts),
      };
    }
    return db;
  }

  function tableHandle(name, spec) {
    return {
      async get(id) {
        if (!ID_RE.test(id)) fail("INVALID_ID", "row id is not 64 lowercase hex");
        const cells = await rows(name);
        return cells.find((row) => row.id === id) ?? null;
      },
      async insert(document) {
        if (!document || typeof document !== "object" || Array.isArray(document)) {
          fail("BAD_DOCUMENT", "insert expects a document");
        }
        const row = validateDocument(spec, document);
        const id = randomBytes(32).toString("hex");
        const stored = { ...row, id, createdAt: Date.now() };
        await writeJson(config, rowKey(name, id), stored);
        invalidate(name);
        return id;
      },
      async update(id, patch) {
        if (!ID_RE.test(id)) fail("INVALID_ID", "row id is not 64 lowercase hex");
        if (!patch || typeof patch !== "object" || Array.isArray(patch)) fail("BAD_PATCH", "update expects a patch");
        const cells = await rows(name);
        const at = cells.findIndex((row) => row.id === id);
        if (at < 0) return null;
        const next = { ...cells[at], ...patch, id, createdAt: cells[at].createdAt };
        await writeJson(config, rowKey(name, id), next);
        invalidate(name);
        return next;
      },
      async delete(id) {
        if (!ID_RE.test(id)) fail("INVALID_ID", "row id is not 64 lowercase hex");
        const cells = await rows(name);
        if (!cells.some((row) => row.id === id)) return false;
        await removeKey(config, rowKey(name, id));
        invalidate(name);
        return true;
      },
    };
  }
  return { forSchema };
}

/** A Lakebed schema validation, kept here so the binding never trusts a caller
 *  document. Unknown fields are refused rather than silently persisted. */
function validateDocument(spec, document) {
  const out = {};
  for (const [field, rule] of Object.entries(spec.fields || {})) {
    let value = document[field];
    if (value === undefined || value === null) {
      if (rule.defaultValue !== undefined) value = rule.defaultValue;
      else if (rule.isOptional) { out[field] = null; continue; }
      else fail("MISSING_VALUE", `missing value for ${field}`);
    }
    switch (rule.kind) {
      case "string":
        if (typeof value !== "string" || value.includes("\0")) fail("BAD_VALUE", `${field} must be a string`);
        break;
      case "boolean":
        if (typeof value !== "boolean") fail("BAD_VALUE", `${field} must be a boolean`);
        break;
      case "number":
        if (typeof value !== "number" || !Number.isFinite(value)) fail("BAD_VALUE", `${field} must be a finite number`);
        break;
      default:
        if (typeof value !== "string" || value.length === 0) fail("BAD_VALUE", `${field} must be a non-empty string`);
    }
    out[field] = value;
  }
  return out;
}

export function storage(config) {
  return {
    /** Lakebed-shaped upload. Returns the same UploadedObject field names.
     *  `public` is an organizational key prefix ONLY: shadw has no
     *  unauthenticated object read, so every URL needs the same tenant bearer. */
    async upload(bytes, options = {}) {
      if (!(bytes instanceof Uint8Array) && typeof bytes !== "string" && !(bytes instanceof Blob)) {
        fail("BAD_UPLOAD", "upload expects bytes, a string or a Blob");
      }
      const payload = bytes instanceof Uint8Array
        ? bytes
        : typeof bytes === "string"
          ? new TextEncoder().encode(bytes)
          : new Uint8Array(await bytes.arrayBuffer());
      const key = `${options.public ? "public" : "private"}-${randomBytes(32).toString("hex")}`;
      const contentType = options.contentType || "application/octet-stream";
      const response = await request(config, [bucketPart(config), keyPart(key)], {
        method: "PUT",
        headers: { "content-type": contentType },
        body: payload,
      });
      const answer = await response.json().catch(() => ({}));
      // A PUT 200 does not prove the bytes landed: shadw's BlobStore::put
      // swallows I/O errors. Read the object back and report the measured size.
      const readBack = await request(config, [bucketPart(config), keyPart(key)]);
      if (readBack === null) fail("WRITE_NOT_DURABLE", "blob write did not read back");
      const stored = new Uint8Array(await readBack.arrayBuffer());
      if (stored.byteLength !== payload.byteLength) {
        fail("WRITE_SIZE_MISMATCH", "blob read back at a different size");
      }
      return {
        key,
        url: `${config.apiBase.replace(/\/$/, "")}/v1/storage/blob/${config.bucket}/${key}`,
        size: stored.byteLength,
        contentType,
        reportedKey: answer?.key ?? null,
      };
    },
    async get(key) {
      const response = await request(config, [bucketPart(config), keyPart(key)]);
      if (response === null) return null;
      return new Uint8Array(await response.arrayBuffer());
    },
    async list() {
      return (await listKeys(config)).filter((k) => k.startsWith("public-") || k.startsWith("private-"));
    },
    /** Typed refusal unless the console database id is configured. */
    async delete(key) {
      if (!KEY_RE.test(key) || key.includes("..")) fail("INVALID_SEGMENT", "key is not a valid storage segment");
      return removeKey(config, key);
    },
    url(key) {
      if (!KEY_RE.test(key) || key.includes("..")) fail("INVALID_SEGMENT", "key is not a valid storage segment");
      return urlFor(config, [bucketPart(config), keyPart(key)]);
    },
  };
}
