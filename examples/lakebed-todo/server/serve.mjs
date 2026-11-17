/**
 * Executes a Lakebed-shaped capsule on shadw's plain Node lane.
 *
 * `fluid.json` names this file as `start_cmd`; it listens on `$PORT`. There is
 * no Lakebed source runtime here: this adapter builds `ctx.db` and
 * `ctx.storage` from ONE cohesive binding (`./shadow.mjs`) to shadw's storage
 * REST API, then dispatches the capsule's declared queries/mutations/endpoints.
 *
 * Browser code never sees a platform credential. The token is server-side and
 * read from the environment; routes return measurements, never secrets.
 */
import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { dirname, join, normalize } from "node:path";
import capsule from "./index.mjs";
import { configFromEnv, database, storage, ShadowBindingError } from "./shadow.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));
const PUBLIC = normalize(join(HERE, "..", "public"));
const CLIENT = normalize(join(HERE, "..", "client"));
const PORT = Number(process.env.PORT || 3000);

let config;
try {
  config = configFromEnv();
} catch (error) {
  console.error(`lakebed capsule refused to start: ${error.code}: ${error.message}`);
  process.exit(1);
}

const bindings = database(config);
const ctx = {
  db: bindings.forSchema(capsule.schema),
  storage: storage(config),
  env: {},
  log: { info: () => {} },
};

function send(res, status, value, contentType = "application/json") {
  const body = Buffer.isBuffer(value)
    ? value
    : contentType === "application/json"
      ? JSON.stringify(value)
      : String(value);
  res.writeHead(status, {
    "content-type": contentType,
    "content-length": body.length,
    "cache-control": "no-store",
  });
  res.end(body);
}

function failure(res, error) {
  const known = error instanceof ShadowBindingError;
  const status = known ? 502 : 400;
  send(res, status, {
    ok: false,
    error: known ? error.code : "CAPSULE_REFUSED",
    message: known ? error.message : String(error?.message || error),
  });
}

async function readBody(req) {
  const chunks = [];
  let size = 0;
  for await (const chunk of req) {
    size += chunk.length;
    if (size > 6 * 1024 * 1024) throw new Error("request too large");
    chunks.push(chunk);
  }
  return Buffer.concat(chunks).toString("utf8");
}

function endpointReply(res, reply) {
  if (!reply || reply.kind !== "response") return send(res, 500, { ok: false, error: "BAD_REPLY" });
  const headers = {
    "content-type": reply.contentType || "text/plain",
    "content-length": Buffer.byteLength(reply.body ?? ""),
    "cache-control": "no-store",
  };
  if (reply.location) headers.location = reply.location;
  res.writeHead(reply.status || 200, headers);
  res.end(reply.body ?? "");
}

async function dispatch(req, res, pathname) {
  const url = new URL(req.url, `http://127.0.0.1:${PORT}`);
  if (pathname === "/api/health") {
    return send(res, 200, { ok: true, framework: "lakebed", binding: "shadw-storage" });
  }

  // Queries and mutations: POST /api/:kind/:name with a JSON argument object.
  const named = /^\/api\/(query|mutation)\/([a-z][a-z0-9_]{0,47})$/.exec(pathname);
  if (named) {
    if (req.method !== "POST") return send(res, 405, { ok: false, error: "METHOD" });
    const [, kind, name] = named;
    const bag = kind === "query" ? capsule.queries : capsule.mutations;
    const entry = bag[name];
    if (!entry || entry.kind !== kind) return send(res, 404, { ok: false, error: "NO_SUCH_ENTRY" });
    let args = {};
    try {
      const raw = await readBody(req);
      args = raw ? JSON.parse(raw) : {};
    } catch (error) {
      return failure(res, error);
    }
    try {
      const value = await entry.handler(ctx, args);
      return send(res, 200, { ok: true, value: value === undefined ? null : value });
    } catch (error) {
      return failure(res, error);
    }
  }

  // Declared endpoints, longest declared path first.
  const endpoints = Object.entries(capsule.endpoints)
    .sort((a, b) => b[1].path.length - a[1].path.length);
  for (const [, entry] of endpoints) {
    const pattern = entry.path.replace(/:[A-Za-z][A-Za-z0-9_]*/g, "([^/]+)");
    const match = new RegExp(`^${pattern}$`).exec(pathname);
    if (!match || entry.method !== req.method) continue;
    const keys = [...entry.path.matchAll(/:([A-Za-z][A-Za-z0-9_]*)/g)].map((m) => m[1]);
    const params = Object.fromEntries(keys.map((key, i) => [key, match[i + 1]]));
    let body;
    try {
      body = await readBody(req);
    } catch (error) {
      return failure(res, error);
    }
    try {
      const reply = await entry.handler(ctx, { body, params, query: url.searchParams, method: req.method });
      return endpointReply(res, reply);
    } catch (error) {
      return failure(res, error);
    }
  }

  // Static client assets, resolved inside PUBLIC or CLIENT only. The Lakebed
  // module boundary puts browser code in `client/`; both roots are allowlisted
  // by exact path, never by a caller-supplied prefix.
  if (req.method === "GET" && !pathname.startsWith("/api/")) {
    const inClient = pathname.startsWith("/client/");
    const rel = inClient
      ? pathname.slice("/client/".length)
      : pathname === "/" ? "index.html" : pathname.slice(1);
    const root = inClient ? CLIENT : PUBLIC;
    const resolved = normalize(join(root, rel));
    if (resolved !== root && !resolved.startsWith(root + "/")) {
      return send(res, 403, { ok: false, error: "PATH" });
    }
    try {
      const bytes = await readFile(resolved);
      const type = resolved.endsWith(".html") ? "text/html; charset=utf-8"
        : resolved.endsWith(".js") ? "text/javascript; charset=utf-8"
          : resolved.endsWith(".css") ? "text/css; charset=utf-8" : "application/octet-stream";
      return send(res, 200, bytes, type);
    } catch {
      return send(res, 404, { ok: false, error: "NOT_FOUND" }, "text/plain");
    }
  }

  return send(res, 404, { ok: false, error: "NOT_FOUND" });
}

createServer((req, res) => {
  const pathname = new URL(req.url, `http://127.0.0.1:${PORT}`).pathname;
  dispatch(req, res, pathname).catch((error) => failure(res, error));
}).listen(PORT, "127.0.0.1", () => {
  console.log(`lakebed capsule listening on ${PORT}`);
});
