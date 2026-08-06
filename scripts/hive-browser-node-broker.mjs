#!/usr/bin/env node
// Node-local session broker for the HEADLESS browser node (ansible role
// `hive_browser_node`). Single canonical source lives here in scripts/ and is
// COPIED to the node by the role -- never hand-edit a second copy (same
// convention as scripts/hive-lockdown.sh, see roles/prerequisites).
//
// WHY THIS EXISTS AT ALL
// ---------------------
// The /run-node page authenticates its browser node with the httpOnly
// `hive_jwt` cookie the dashboard's /api/token route mints from a verified
// CLERK session. A server has no human to sign in, and the backend leaves no
// second door open:
//
//   crates/hive-cloud/src/browser_admission.rs::fresh_user_claims rejects
//     * `sub` starting with "key:"  -> a dashboard API KEY can never admit
//     * `role == "service"`         -> no service identity can admit
//     * `now - iat > HIVE_BROWSER_SESSION_MAX_AGE_SECS` (default 300s)
//                                   -> no long-lived credential can admit
//
// So the ONLY admissible credential is a freshly-minted platform JWT, and the
// only mint is POST /v1/token, which crates/hive-cloud/src/admin.rs::
// mint_allowed gates on `x-hive-internal == HIVE_INTERNAL_TOKEN`.
//
// This broker is therefore the smallest possible thing that can exist: it
// holds the internal token (delivered by systemd LoadCredential= from a
// root-owned 0600 file, NEVER an Environment= line in a unit), and it hands
// the browser nothing but a short-lived, single-tenant, non-admin cookie.
// Chromium never sees the internal token, never sees a user password, and
// never sees an OAuth/Clerk credential -- there is none to leak.
//
// LEAST PRIVILEGE OF THE MINTED TOKEN
//   tenant : exactly HIVE_BROWSER_NODE_TEAM, hardcoded here; the request body
//            is IGNORED, so a compromised page cannot mint for another tenant
//   role   : "member" by default -- `may_serve_public` needs owner/admin, so a
//            member can never be talked into a PUBLIC-scope node
//   email  : "" -- mint_token derives `platform_admin` from ITS OWN admin_emails
//            set against this field, so an empty email is structurally
//            incapable of producing a platform-operator token
//   sub    : "fleet-browser-node:<node>" -- auditable, never a human's id, and
//            stable so browser_presence::require_owned_admission's subject
//            check keeps matching across re-mints
//   ttl    : whatever the backend issues (1h), re-minted every ~2min so the
//            300s freshness bar is never the thing that breaks a renewal
//
// SURFACES (loopback only; Chromium treats http://127.0.0.1 as a secure
// context, which is what makes WebCrypto/IndexedDB/OPFS available to the
// worker without TLS):
//   GET  /                  the headless host page
//   GET  /config            relay list / team / scope / serveMode / abi
//   POST /api/token         mint -> Set-Cookie: hive_jwt   (same path the
//                           worker's own remintSession() already calls)
//   *    /cloud/*           reverse proxy to the node's admin API, exactly
//                           what ui/next.config.mjs's rewrite does
//   POST /heartbeat         page liveness + last RunNodeStatus
//   GET  /healthz           supervisor probe (scripts/hive-browser-node-run.sh)
//   GET  /run-node-worker.js, /browser-node/**   static, from WWW_DIR

import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { readFileSync } from "node:fs";
import { join, normalize, extname } from "node:path";

const env = (k, d = "") => (process.env[k] ?? d).toString().trim();
const num = (k, d) => {
  const v = Number.parseInt(env(k, ""), 10);
  return Number.isFinite(v) ? v : d;
};

const PORT = num("HIVE_BROWSER_NODE_PORT", 3009);
const HOST = "127.0.0.1";
const ADMIN = env("HIVE_ADMIN", "http://127.0.0.1:8786").replace(/\/+$/, "");
const WWW_DIR = env("HIVE_BROWSER_NODE_WWW", "/opt/hive-browser-node/www");
const TEAM = env("HIVE_BROWSER_NODE_TEAM", "personal");
const ROLE = env("HIVE_BROWSER_NODE_ROLE", "member");
const SCOPE = env("HIVE_BROWSER_NODE_SCOPE", "team") === "public" ? "public" : "team";
const SERVE_MODE = env("HIVE_BROWSER_NODE_SERVE_MODE", "auto") === "none" ? "none" : "auto";
const NODE_NAME = env("HIVE_BROWSER_NODE_NAME", "unknown");
const SUBJECT = `fleet-browser-node:${NODE_NAME}`;
const RELAYS = env(
  "HIVE_BROWSER_NODE_RELAYS",
  "https://fc-sanjose.relay.shadw.app:3343,https://fc-bangkok.relay.shadw.app:3343,https://fc-virginia.relay.shadw.app:3343",
);
// Pinned deployment/function (optional). Empty = the automatic shape: the
// fleet derives this tenant's whole browser-eligible set on every renewal.
const PIN_DEPLOYMENT = env("HIVE_BROWSER_NODE_DEPLOYMENT", "");
const PIN_FUNCTION = env("HIVE_BROWSER_NODE_FUNCTION", "");
// Geo published with presence. Operator-declared coordinates (inventory
// `hive_geo`, the same value hive-node itself is pinned to) win outright; see
// resolveGeoFromRegistry() below for the fallback and why there is no third
// option.
const GEO_LAT = env("HIVE_BROWSER_NODE_LAT", "");
const GEO_LON = env("HIVE_BROWSER_NODE_LON", "");

const HEARTBEAT_STALE_MS = num("HIVE_BROWSER_NODE_HEARTBEAT_STALE_MS", 75_000);
const BOOT_GRACE_MS = num("HIVE_BROWSER_NODE_BOOT_GRACE_MS", 300_000);
const MINT_TIMEOUT_MS = 10_000;
const PROXY_TIMEOUT_MS = 30_000;
// A single artifact GET can carry a real bundle; everything else is small JSON.
const MAX_BODY_BYTES = 8 * 1024 * 1024;

/** The internal token: systemd LoadCredential= puts it in a 0400 file inside a
 *  private tmpfs, so it is never in this process's env, never in the unit text,
 *  and never visible via `systemctl show` or /proc/<pid>/environ. Read ONCE at
 *  boot and held in memory only. */
function loadInternalToken() {
  const explicit = env("HIVE_BROWSER_NODE_TOKEN_FILE", "");
  const credDir = env("CREDENTIALS_DIRECTORY", "");
  const path = explicit || (credDir ? join(credDir, "internal-token") : "");
  if (!path) {
    throw new Error(
      "no internal token source: expected systemd LoadCredential=internal-token or HIVE_BROWSER_NODE_TOKEN_FILE",
    );
  }
  const token = readFileSync(path, "utf8").trim();
  if (!token) throw new Error(`internal token file ${path} is empty`);
  return token;
}
const INTERNAL_TOKEN = loadInternalToken();

/** The host-ABI version is READ OUT OF the deployed worker, never hardcoded
 *  here: it keys the worker URL (ui/lib/use-run-node.ts does the same), so a
 *  copy in this file would silently drift from the artifact it is booting the
 *  moment ui/public/run-node-worker.js bumps it. */
function readAbiVersion() {
  try {
    const src = readFileSync(join(WWW_DIR, "run-node-worker.js"), "utf8");
    const m = src.match(/const\s+HOST_ABI_VERSION\s*=\s*(\d+)/);
    if (m) return Number.parseInt(m[1], 10);
  } catch {
    /* fall through to the loud failure below */
  }
  throw new Error(`cannot read HOST_ABI_VERSION from ${WWW_DIR}/run-node-worker.js`);
}
const ABI_VERSION = readAbiVersion();

const state = {
  startedMs: Date.now(),
  lastHeartbeatMs: 0,
  lifecycle: "stopped",
  admission: "none",
  endpointId: null,
  relay: null,
  serving: false,
  presenceOkMs: 0,
  lastError: null,
  mints: 0,
  mintFailures: 0,
};

/** Presence coordinates, resolved in strict priority order:
 *
 *   1. HIVE_BROWSER_NODE_LAT/LON -- the OPERATOR's declared position, straight
 *      out of the inventory's `hive_geo` for this host, i.e. the same pin
 *      hive-node itself runs with.
 *   2. This host's OWN entry in the platform's node registry (`GET /v1/nodes`,
 *      the record flagged `is_self`) -- exactly where the platform already
 *      believes this machine is. The browser node then lands on top of its
 *      host node on the constellation instead of nowhere.
 *
 * There is deliberately NO third source: no geolocation HTTP service, and no
 * browser Geolocation permission -- which does not exist headlessly and would
 * be an invention if it did. Measured against the real roster
 * (`ansible-inventory --list`), only 2 of the 5 rostered hosts declare
 * `hive_geo`, so without (2) three fleet browser nodes would publish presence
 * with no fix at all -- silently, since an absent coordinate is indis-
 * tinguishable from a node that simply has not published yet.
 *
 * Resolution NEVER blocks: /config answers immediately with whatever is known,
 * and a late resolution is picked up because the page re-reads /config while
 * it is still unlocated. */
const geo = {
  lat: Number.isFinite(Number(GEO_LAT)) && GEO_LAT !== "" ? Number(GEO_LAT) : null,
  lon: Number.isFinite(Number(GEO_LON)) && GEO_LON !== "" ? Number(GEO_LON) : null,
  source: "none",
  lastAttemptMs: 0,
  error: null,
};
if (geo.lat !== null && geo.lon !== null) geo.source = "declared";
// A failed lookup costs one mint against the backend's per-IP mint limiter
// (20/60s, shared with hive-ui on this same loopback address), so the retry
// floor is deliberately coarse: this is a cosmetic map fix, never worth
// crowding out the session mints the node actually needs to stay admitted.
const GEO_RETRY_MS = 120_000;
let geoInFlight = null;

const MIME = {
  ".js": "text/javascript; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".wasm": "application/wasm",
  ".html": "text/html; charset=utf-8",
  ".json": "application/json; charset=utf-8",
  ".map": "application/json; charset=utf-8",
  ".ts": "text/plain; charset=utf-8",
};

function json(res, status, body, headers = {}) {
  const payload = Buffer.from(JSON.stringify(body));
  res.writeHead(status, {
    "content-type": "application/json; charset=utf-8",
    "content-length": payload.length,
    "cache-control": "no-store",
    ...headers,
  });
  res.end(payload);
}

async function readBody(req) {
  const chunks = [];
  let total = 0;
  for await (const chunk of req) {
    total += chunk.length;
    if (total > MAX_BODY_BYTES) throw new Error("request body too large");
    chunks.push(chunk);
  }
  return Buffer.concat(chunks);
}

/** The ONE mint. Every claim in the request is this process's configuration,
 *  never a caller's: the tenant, the role, the auditable subject, and the
 *  deliberately EMPTY email that makes `mint_token`'s platform_admin
 *  derivation structurally impossible. `countFailures` is false for the
 *  internal geo lookup so a cosmetic background failure cannot pollute the
 *  session-mint counters an operator reads out of /healthz. */
async function mintRaw({ countFailures = true } = {}) {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), MINT_TIMEOUT_MS);
  try {
    const upstream = await fetch(`${ADMIN}/v1/token`, {
      method: "POST",
      headers: { "content-type": "application/json", "x-hive-internal": INTERNAL_TOKEN },
      body: JSON.stringify({ sub: SUBJECT, tenant: TEAM, role: ROLE, email: "" }),
      signal: controller.signal,
    });
    if (!upstream.ok) {
      if (countFailures) {
        state.mintFailures += 1;
        state.lastError = `mint ${upstream.status}`;
      }
      return { ok: false, status: upstream.status };
    }
    const data = await upstream.json();
    const token = String(data.token || "");
    const expiresIn = Number(data.expires_in) || 3600;
    if (token && countFailures) state.mints += 1;
    return { ok: true, status: upstream.status, token, expiresIn };
  } catch (err) {
    const reason = err && err.message ? err.message : String(err);
    if (countFailures) {
      state.mintFailures += 1;
      state.lastError = `mint: ${reason}`;
    }
    return { ok: false, status: 0, reason };
  } finally {
    clearTimeout(timer);
  }
}

/** Mint a fresh platform JWT and hand it back as an httpOnly cookie.
 *  Deliberately ignores the request body: the tenant/role/subject are this
 *  process's configuration, never the caller's claim. */
async function mintToken(res) {
  const minted = await mintRaw();
  if (!minted.ok) {
    return json(res, 502, {
      ok: false,
      reason: minted.status ? `mint-failed-${minted.status}` : "mint-unreachable",
    });
  }
  if (!minted.token) {
    // Unenforced backend (no HIVE_JWT_SECRET) mints nothing meaningful.
    return json(res, 200, { ok: true, tenant: TEAM, enforced: false });
  }
  // No `Secure`: the origin is http://127.0.0.1, which browsers already
  // classify as a trustworthy context. HttpOnly keeps it out of page JS.
  const cookie = [
    `hive_jwt=${minted.token}`,
    "Path=/",
    "HttpOnly",
    "SameSite=Lax",
    `Max-Age=${Math.max(60, minted.expiresIn - 30)}`,
  ].join("; ");
  return json(res, 200, { ok: true, tenant: TEAM, role: ROLE }, { "set-cookie": cookie });
}

/** Fallback (2) above: ask the platform where IT thinks this host is. Uses the
 *  same least-privilege member token every other call uses -- `/v1/nodes` is a
 *  session-gated read, and a member token is enough for it. Best-effort by
 *  construction: any failure leaves the node unlocated, which is the honest
 *  outcome, never a guessed coordinate. */
async function resolveGeoFromRegistry() {
  if (geo.source !== "none") return;
  if (geoInFlight) return geoInFlight;
  const now = Date.now();
  if (now - geo.lastAttemptMs < GEO_RETRY_MS) return;
  geo.lastAttemptMs = now;
  geoInFlight = (async () => {
    try {
      const minted = await mintRaw({ countFailures: false });
      if (!minted.ok) {
        geo.error = minted.status ? `nodes-mint-${minted.status}` : "nodes-mint-unreachable";
        return;
      }
      const controller = new AbortController();
      const timer = setTimeout(() => controller.abort(), MINT_TIMEOUT_MS);
      let nodes;
      try {
        const headers = { accept: "application/json" };
        // An unenforced backend mints no token; the read still works there.
        if (minted.token) headers.authorization = `Bearer ${minted.token}`;
        const r = await fetch(`${ADMIN}/v1/nodes`, { headers, signal: controller.signal });
        if (!r.ok) {
          geo.error = `nodes-${r.status}`;
          return;
        }
        nodes = await r.json();
      } finally {
        clearTimeout(timer);
      }
      const list = Array.isArray(nodes) ? nodes : Array.isArray(nodes?.nodes) ? nodes.nodes : [];
      // `is_self` is the authoritative marker; the name match is only a
      // fallback for a registry view that does not carry it.
      const self =
        list.find((n) => n && n.is_self === true) ||
        list.find((n) => n && (n.name === NODE_NAME || n.id === NODE_NAME));
      const lat = Number(self?.lat);
      const lon = Number(self?.lon);
      // 0,0 is Null Island, which every "no fix" path in a geo pipeline
      // eventually produces -- it is a missing value, not the Gulf of Guinea.
      if (
        !Number.isFinite(lat) ||
        !Number.isFinite(lon) ||
        (lat === 0 && lon === 0) ||
        lat < -90 ||
        lat > 90 ||
        lon < -180 ||
        lon > 180
      ) {
        geo.error = self ? "nodes-self-unlocated" : "nodes-self-missing";
        return;
      }
      geo.lat = lat;
      geo.lon = lon;
      geo.source = "registry";
      geo.error = null;
      process.stdout.write(
        `hive-browser-node-broker: geo from node registry (is_self) lat=${lat} lon=${lon}\n`,
      );
    } catch (err) {
      geo.error = `nodes: ${err && err.message ? err.message : err}`;
    } finally {
      geoInFlight = null;
    }
  })();
  return geoInFlight;
}

/** Reverse-proxy /cloud/* to the node's own admin API -- byte-for-byte what
 *  ui/next.config.mjs's `{ source: "/cloud/:path*", destination: ADMIN }`
 *  rewrite does, so the worker's existing `fetch("/cloud" + path)` calls work
 *  unchanged against this origin. The `hive_jwt` cookie rides through as the
 *  Authorization the backend's auth::extract_token reads. */
async function proxyCloud(req, res, url) {
  const target = `${ADMIN}${url.pathname.slice("/cloud".length)}${url.search}`;
  const headers = {};
  for (const [k, v] of Object.entries(req.headers)) {
    // Hop-by-hop + host headers must not be forwarded; `x-hive-internal` from
    // a page would be an escalation attempt and is dropped unconditionally.
    if (["host", "connection", "content-length", "x-hive-internal", "transfer-encoding"].includes(k)) continue;
    if (typeof v === "string") headers[k] = v;
  }
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), PROXY_TIMEOUT_MS);
  try {
    const body = req.method === "GET" || req.method === "HEAD" ? undefined : await readBody(req);
    const upstream = await fetch(target, {
      method: req.method,
      headers,
      body: body && body.length ? body : undefined,
      redirect: "manual",
      signal: controller.signal,
    });
    const buf = Buffer.from(await upstream.arrayBuffer());
    const out = { "content-length": buf.length };
    for (const pass of ["content-type", "etag", "cache-control", "x-hive-policy-digest", "x-hive-source-digest"]) {
      const v = upstream.headers.get(pass);
      if (v) out[pass] = v;
    }
    res.writeHead(upstream.status, out);
    res.end(buf);
  } catch (err) {
    json(res, 502, { error: { code: "proxy_failed", message: String(err && err.message ? err.message : err) } });
  } finally {
    clearTimeout(timer);
  }
}

/** Static files under WWW_DIR. The path is normalized and re-checked against
 *  the root AFTER normalization, so no `..` segment can escape. */
async function serveStatic(res, pathname) {
  const rel = normalize(decodeURIComponent(pathname)).replace(/^(\.\.[/\\])+/, "");
  const abs = join(WWW_DIR, rel);
  if (!abs.startsWith(WWW_DIR + "/") && abs !== WWW_DIR) {
    return json(res, 400, { error: "bad path" });
  }
  try {
    const buf = await readFile(abs);
    res.writeHead(200, {
      "content-type": MIME[extname(abs)] || "application/octet-stream",
      "content-length": buf.length,
      // Never cached: a role re-run replaces these files in place and the
      // browser must pick the new bundle up on its next restart, not serve a
      // stale wasm against a rolled-forward fleet.
      "cache-control": "no-store",
    });
    res.end(buf);
  } catch {
    json(res, 404, { error: "not found" });
  }
}

/** Instantaneous health verdict. The supervisor
 *  (scripts/hive-browser-node-run.sh) requires several consecutive failures
 *  before it acts, so a momentary relay migration never recycles Chromium. */
function health() {
  const now = Date.now();
  const age = now - state.lastHeartbeatMs;
  const booting = now - state.startedMs < BOOT_GRACE_MS;
  const heartbeatOk = state.lastHeartbeatMs > 0 && age < HEARTBEAT_STALE_MS;
  const lifecycleOk = state.lifecycle === "online" || state.lifecycle === "suspended";
  const ok = booting ? state.lastHeartbeatMs === 0 || heartbeatOk : heartbeatOk && lifecycleOk;
  return {
    ok,
    booting,
    heartbeat_age_ms: state.lastHeartbeatMs ? age : null,
    lifecycle: state.lifecycle,
    admission: state.admission,
    endpoint_id: state.endpointId,
    relay: state.relay,
    serving: state.serving,
    presence_age_ms: state.presenceOkMs ? now - state.presenceOkMs : null,
    last_error: state.lastError,
    mints: state.mints,
    mint_failures: state.mintFailures,
    team: TEAM,
    node: NODE_NAME,
    abi: ABI_VERSION,
    geo_source: geo.source,
    geo_error: geo.error,
  };
}

const server = createServer(async (req, res) => {
  // DNS-rebinding defense: this server is loopback-bound, but a browser can
  // still be pointed at it through a hostname that resolves to 127.0.0.1.
  const host = String(req.headers.host || "");
  if (host && !/^(127\.0\.0\.1|\[::1\]|localhost)(:\d+)?$/i.test(host)) {
    return json(res, 421, { error: "unexpected host" });
  }
  const url = new URL(req.url || "/", `http://${HOST}:${PORT}`);
  const p = url.pathname;

  if (p === "/healthz") {
    const h = health();
    return json(res, h.ok ? 200 : 503, h);
  }
  if (p === "/config") {
    // Never awaited: an unresolved fix must not delay the page's boot. The
    // page re-reads /config while it is still unlocated, so a resolution that
    // lands on the second or fiftieth attempt is still picked up.
    if (geo.source === "none") void resolveGeoFromRegistry();
    return json(res, 200, {
      relay: RELAYS,
      team: TEAM,
      scope: SCOPE,
      serveMode: SERVE_MODE,
      deployment: PIN_DEPLOYMENT,
      fn: PIN_FUNCTION,
      abi: ABI_VERSION,
      node: NODE_NAME,
      lat: geo.lat,
      lon: geo.lon,
      geoSource: geo.source,
    });
  }
  if (p === "/api/token" && req.method === "POST") {
    await readBody(req).catch(() => null); // drain; the body is deliberately unused
    return mintToken(res);
  }
  if (p === "/heartbeat" && req.method === "POST") {
    try {
      const body = JSON.parse((await readBody(req)).toString("utf8") || "{}");
      state.lastHeartbeatMs = Date.now();
      if (typeof body.lifecycle === "string") state.lifecycle = body.lifecycle;
      if (typeof body.admission === "string") state.admission = body.admission;
      state.endpointId = body.endpointId || null;
      state.relay = body.relay || null;
      state.serving = !!body.serving;
      state.lastError = body.lastError || null;
      if (body.presenceOk) state.presenceOkMs = Date.now();
      return json(res, 200, { ok: true });
    } catch (err) {
      return json(res, 400, { ok: false, reason: String(err && err.message ? err.message : err) });
    }
  }
  if (p === "/cloud" || p.startsWith("/cloud/")) return proxyCloud(req, res, url);
  if (p === "/" || p === "/index.html") return serveStatic(res, "/index.html");
  if (p === "/run-node-worker.js" || p.startsWith("/browser-node/")) return serveStatic(res, p);
  return json(res, 404, { error: "not found" });
});

server.listen(PORT, HOST, () => {
  process.stdout.write(
    `hive-browser-node-broker: http://${HOST}:${PORT} admin=${ADMIN} team=${TEAM} role=${ROLE} scope=${SCOPE} ` +
      `serve=${SERVE_MODE} abi=${ABI_VERSION} node=${NODE_NAME} geo=${geo.source}\n`,
  );
  // Kick the registry lookup at boot so the very first /config already carries
  // a fix on a host with no declared hive_geo. Fire-and-forget: hive-node may
  // not be listening yet (the unit is only Wants=), and the retry floor covers
  // that without holding up the listener the supervisor is waiting on.
  if (geo.source === "none") void resolveGeoFromRegistry();
});

for (const sig of ["SIGTERM", "SIGINT"]) {
  process.on(sig, () => {
    server.close(() => process.exit(0));
    setTimeout(() => process.exit(0), 2000).unref();
  });
}
