// Platform-owned Node HTTP bridge for Build Output API v3 `.func` bundles.
//
// Staged verbatim (see `stage_build_output_node_launchers` in git.rs) into the
// SAME immutable, validated `.func` directory as the tenant handler itself,
// only AFTER every repository-controlled install/build command has finished,
// the artifact has been parsed and re-validated, and collision/symlink checks
// on the exact target path have passed. It is then chmod'd read-only. Because
// staging already happens after validation and the file is written via
// create-new + atomic rename + read-only, no separate content-hash/data-URL
// bootstrap is needed to prove the bytes running here are platform-owned
// rather than tenant-supplied — the staging discipline itself is the proof.
//
// Invoked with direct Node argv, no shell, no package manager:
//   node <this-file> <handler-relative-path>
// CWD is the function's OWN `.func` directory (never the deployment root),
// so `import()` below resolves the handler exactly the way `.vc-config.json`
// declared it.
//
// Implements the official @vercel/node request-handling contract (pinned at
// commit 6331571e2fe14de31a01d00deead9e7a349e53a6):
//   packages/node/src/serverless-functions/serverless-handler.mts
//   packages/node/src/serverless-functions/helpers.ts
//   packages/node/src/serverless-functions/helpers-web.ts
// Handler precedence, in order: middleware special case is not applicable
// here (middleware ships as an Edge function, which this platform refuses
// before it ever reaches a launcher) — so the live precedence is:
//   1. `fetch` export                    (Web Request -> Web Response)
//   2. named HTTP method exports         (GET/HEAD/OPTIONS/POST/PUT/DELETE/PATCH)
//   3. callable classic handler          ((req, res) with Vercel helpers)
//   4. a captured `http.Server`          (module calls `.listen()` itself)
//   5. none of the above -> typed refusal, surfaced as a loud 500 at startup.
//
// Limits enforced here (Build-Output-only; never touches the legacy gateway
// path): request/response body 4_500_000 bytes, header count 2_000, aggregate
// header bytes 16_384, startup/import budget 30_000ms, standalone-server
// capture budget 1_000ms, per-request execution budget `HIVE_MAX_DURATION_MS`
// (defaults to 300_000ms, the platform's default `maxDuration`).

import http from 'node:http';
import { Buffer } from 'node:buffer';

const HANDLER_REL = process.argv[2];
if (!HANDLER_REL) {
  console.error('hive-build-output-launcher: missing handler argument');
  process.exit(1);
}

const PORT = Number(process.env.PORT);
if (!Number.isInteger(PORT) || PORT <= 0) {
  console.error('hive-build-output-launcher: PORT is not a valid port number');
  process.exit(1);
}

const MAX_BODY_BYTES = 4_500_000;
const MAX_HEADER_COUNT = 2_000;
const MAX_HEADER_BYTES = 16_384;
const IMPORT_TIMEOUT_MS = 30_000;
const SERVER_CAPTURE_TIMEOUT_MS = 1_000;
const REQUEST_TIMEOUT_MS = (() => {
  const raw = Number(process.env.HIVE_MAX_DURATION_MS);
  return Number.isFinite(raw) && raw > 0 ? raw : 300_000;
})();

function withTimeout(promise, ms, message) {
  let timer;
  const timeout = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error(message)), ms);
  });
  return Promise.race([promise, timeout]).finally(() => clearTimeout(timer));
}

// --- Step 1: temporarily capture any http.Server the handler module might
// construct and `.listen()` on itself (the "standalone server" shape some
// frameworks emit into a Build Output function). Restored immediately after
// import finishes so a later, unrelated `.listen()` call from application
// code is never silently swallowed.
let capturedServer = null;
const realListen = http.Server.prototype.listen;
http.Server.prototype.listen = function patchedListen(...args) {
  capturedServer = this;
  // Never actually bind the tenant's own port — this process owns the one
  // real listener on $PORT. Acknowledge asynchronously so callback-based
  // callers (`server.listen(port, cb)`) still observe a "listening" signal.
  const cb = args.find((a) => typeof a === 'function');
  if (cb) queueMicrotask(cb);
  return this;
};

let handlerModule;
try {
  handlerModule = await withTimeout(
    import(`./${HANDLER_REL}`),
    IMPORT_TIMEOUT_MS,
    `handler module did not finish importing within ${IMPORT_TIMEOUT_MS}ms`,
  );
} catch (error) {
  console.error('hive-build-output-launcher: handler import failed:', error?.stack || error);
  process.exit(1);
} finally {
  http.Server.prototype.listen = realListen;
}

// A module that calls `.listen()` asynchronously (after import resolves) gets
// one short grace window before we decide there is no captured server.
if (!capturedServer) {
  await new Promise((resolve) => setTimeout(resolve, SERVER_CAPTURE_TIMEOUT_MS));
}

// --- Step 2: five-level truthy `.default` unwrapping (CommonJS/ESM
// namespace interop — `export default function handler(){}` compiled to CJS
// nests one extra `.default`, and some bundlers nest further).
function unwrapDefault(mod) {
  let current = mod;
  for (let i = 0; i < 5; i += 1) {
    if (current && typeof current === 'object' && 'default' in current && current.default) {
      current = current.default;
    } else {
      break;
    }
  }
  return current;
}

const resolved = unwrapDefault(handlerModule);

const METHODS = ['GET', 'HEAD', 'OPTIONS', 'POST', 'PUT', 'DELETE', 'PATCH'];
const namedMethodHandlers = {};
for (const method of METHODS) {
  const fromDefault = typeof resolved === 'object' && resolved ? resolved[method] : undefined;
  const fromModule = handlerModule[method];
  const candidate = typeof fromDefault === 'function' ? fromDefault : fromModule;
  if (typeof candidate === 'function') namedMethodHandlers[method] = candidate;
}

const fetchExport =
  typeof handlerModule.fetch === 'function'
    ? handlerModule.fetch
    : typeof resolved === 'object' && resolved && typeof resolved.fetch === 'function'
      ? resolved.fetch
      : null;

const classicHandler = typeof resolved === 'function' ? resolved : null;

const dispatchMode = fetchExport
  ? 'fetch'
  : Object.keys(namedMethodHandlers).length > 0
    ? 'methods'
    : classicHandler
      ? 'classic'
      : capturedServer
        ? 'server'
        : null;

if (!dispatchMode) {
  console.error(
    'hive-build-output-launcher: handler exposes none of fetch/named-method exports/a callable ' +
      'default export/a captured http.Server — refusing to serve',
  );
  process.exit(1);
}

// --- Body/header reading, shared by every dispatch mode.
function readBoundedBody(req) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    let total = 0;
    req.on('data', (chunk) => {
      total += chunk.length;
      if (total > MAX_BODY_BYTES) {
        req.destroy();
        reject(Object.assign(new Error('request body exceeds 4500000 bytes'), { statusCode: 413 }));
        return;
      }
      chunks.push(chunk);
    });
    req.on('end', () => resolve(Buffer.concat(chunks)));
    req.on('error', reject);
  });
}

function validateIncomingHeaders(req, res) {
  const names = Object.keys(req.headers);
  if (names.length > MAX_HEADER_COUNT) {
    res.writeHead(431).end('Too Many Headers');
    return false;
  }
  let aggregate = 0;
  for (const name of names) {
    const value = req.headers[name];
    aggregate += name.length + (Array.isArray(value) ? value.join(',').length : String(value ?? '').length);
  }
  if (aggregate > MAX_HEADER_BYTES) {
    res.writeHead(431).end('Request Header Fields Too Large');
    return false;
  }
  return true;
}

// Classic Vercel helpers (req.cookies/req.query/req.body, res.status/redirect/
// send/json) — the minimal, faithful subset actually reachable from a
// `(req, res)` classic handler per helpers.ts.
function attachClassicHelpers(req, res, bodyBuffer, url) {
  req.query = Object.fromEntries(url.searchParams);
  const cookieHeader = req.headers.cookie || '';
  req.cookies = Object.fromEntries(
    cookieHeader
      .split(';')
      .map((pair) => pair.trim())
      .filter(Boolean)
      .map((pair) => {
        const eq = pair.indexOf('=');
        return eq === -1 ? [pair, ''] : [pair.slice(0, eq), decodeURIComponent(pair.slice(eq + 1))];
      }),
  );
  const contentType = req.headers['content-type'] || '';
  if (contentType.includes('application/json')) {
    try {
      req.body = bodyBuffer.length ? JSON.parse(bodyBuffer.toString('utf8')) : undefined;
    } catch {
      req.body = undefined;
    }
  } else if (contentType.includes('application/x-www-form-urlencoded')) {
    req.body = Object.fromEntries(new URLSearchParams(bodyBuffer.toString('utf8')));
  } else if (bodyBuffer.length) {
    req.body = bodyBuffer.toString('utf8');
  }
  res.status = (code) => {
    res.statusCode = code;
    return res;
  };
  res.send = (payload) => {
    if (payload === undefined) return res.end();
    if (Buffer.isBuffer(payload) || typeof payload === 'string') return res.end(payload);
    return res.end(JSON.stringify(payload));
  };
  res.json = (payload) => {
    if (!res.headersSent) res.setHeader('content-type', 'application/json; charset=utf-8');
    return res.end(JSON.stringify(payload));
  };
  res.redirect = (statusOrUrl, maybeUrl) => {
    const [status, location] = typeof statusOrUrl === 'number' ? [statusOrUrl, maybeUrl] : [307, statusOrUrl];
    res.writeHead(status, { location });
    return res.end();
  };
}

function boundedResponseWriter(res) {
  let written = 0;
  const originalWrite = res.write.bind(res);
  const originalEnd = res.end.bind(res);
  const check = (chunk) => {
    if (chunk == null) return true;
    written += Buffer.byteLength(chunk);
    if (written > MAX_BODY_BYTES) {
      res.destroy(new Error('response body exceeds 4500000 bytes'));
      return false;
    }
    return true;
  };
  res.write = (chunk, ...rest) => (check(chunk) ? originalWrite(chunk, ...rest) : false);
  res.end = (chunk, ...rest) => (check(chunk) ? originalEnd(chunk, ...rest) : res);
}

const server = http.createServer(async (req, res) => {
  if (!validateIncomingHeaders(req, res)) return;
  boundedResponseWriter(res);
  const url = new URL(req.url || '/', `http://127.0.0.1:${PORT}`);
  let waitUntilPromises = [];
  const waitUntil = (p) => {
    waitUntilPromises.push(Promise.resolve(p).catch((error) => {
      console.error('hive-build-output-launcher: waitUntil rejection:', error?.stack || error);
    }));
  };

  const run = async () => {
    if (dispatchMode === 'fetch' || dispatchMode === 'methods') {
      const bodyBuffer = req.method === 'GET' || req.method === 'HEAD' ? Buffer.alloc(0) : await readBoundedBody(req);
      const headers = new Headers();
      for (const [name, value] of Object.entries(req.headers)) {
        if (value === undefined) continue;
        headers.set(name, Array.isArray(value) ? value.join(', ') : String(value));
      }
      const request = new Request(url, {
        method: req.method,
        headers,
        body: bodyBuffer.length ? bodyBuffer : undefined,
        // FetchEvent-style waitUntil: exposed via a non-standard property so a
        // `fetch`/named-method handler can extend the response lifetime for
        // background work, mirroring Vercel's WAIT_UNTIL_TIMEOUT precedent.
      });
      request.waitUntil = waitUntil;

      const target =
        dispatchMode === 'fetch' ? fetchExport : namedMethodHandlers[req.method];
      if (dispatchMode === 'methods' && !target) {
        res.writeHead(405, { allow: Object.keys(namedMethodHandlers).join(', ') }).end('Method Not Allowed');
        return;
      }
      const response = await target(request);
      if (!(response instanceof Response)) {
        throw new Error('handler did not return a Response');
      }
      const responseHeaders = {};
      for (const [name, value] of response.headers) responseHeaders[name] = value;
      res.writeHead(response.status, responseHeaders);
      if (response.body) {
        for await (const chunk of response.body) {
          if (!res.write(Buffer.from(chunk))) break;
        }
      }
      res.end();
      return;
    }

    if (dispatchMode === 'classic') {
      const bodyBuffer = await readBoundedBody(req);
      attachClassicHelpers(req, res, bodyBuffer, url);
      await classicHandler(req, res);
      return;
    }

    // dispatchMode === 'server': hand the raw req/res straight to the
    // captured server's own request listeners, exactly as if it had bound
    // the port itself.
    capturedServer.emit('request', req, res);
  };

  try {
    await withTimeout(run(), REQUEST_TIMEOUT_MS, `request execution exceeded ${REQUEST_TIMEOUT_MS}ms`);
  } catch (error) {
    console.error('hive-build-output-launcher: request failed:', error?.stack || error);
    if (!res.headersSent) {
      res.writeHead(error?.statusCode || 500, { 'content-type': 'text/plain; charset=utf-8' });
    }
    if (!res.writableEnded) res.end(String(error?.message || error));
  } finally {
    await Promise.allSettled(waitUntilPromises);
  }
});

server.on('clientError', (error, socket) => {
  if (socket.writable) socket.end('HTTP/1.1 400 Bad Request\r\n\r\n');
});

server.listen(PORT, '0.0.0.0', () => {
  // A single stdout line signals readiness for the platform's preflight/cold
  // start capture — never written before the real listener is bound.
  console.log(`hive-build-output-launcher: listening on 0.0.0.0:${PORT}`);
});

for (const signal of ['SIGTERM', 'SIGINT']) {
  process.on(signal, () => {
    server.close(() => process.exit(0));
    // Do not wait forever for in-flight connections on a forced shutdown.
    setTimeout(() => process.exit(0), 5_000).unref();
  });
}

process.on('unhandledRejection', (reason) => {
  console.error('hive-build-output-launcher: unhandled rejection:', reason);
});
