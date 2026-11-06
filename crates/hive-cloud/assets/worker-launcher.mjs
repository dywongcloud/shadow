// Platform adapter for Cloudflare Worker modules (the `export default {
// async fetch(request, env, ctx) { ... } }` shape). Staged into the build dir by
// the platform; never part of the repository.
//
// Usage: node [--experimental-transform-types] .hive-worker-launcher.mjs <entry>
//
// WHY THIS EXISTS. A Worker is not a Node server and cannot be started the way
// its own template starts it: every Cloudflare template's `scripts.start` runs
// the `wrangler` CLI, which needs the Workers runtime (workerd) and a Cloudflare
// account. Our substrate runs Node, so launching `wrangler` fails with
// "command wrangler is not a dir". This adapter instead hosts the SAME handler
// the Worker exports, on Node: it binds $PORT, converts each inbound Node
// request into a Fetch API `Request`, calls `fetch(request, env, ctx)`, and
// writes the returned `Response` back. No workerd, no account, no deploy.
//
// WHAT IS AND IS NOT EMULATED — stated honestly, because a silent partial
// emulation is worse than a named gap:
//   * `env` is populated from the deployment's real process environment. There
//     is no bindings layer: KV / D1 / R2 / Queues / Durable Objects / service
//     bindings are NOT provided. A Worker that dereferences one at request time
//     gets `undefined` (or its own error) rather than a fabricated stub.
//   * `ctx.waitUntil` runs the promise in the background but, unlike Workers,
//     does not extend the platform's request lifetime — the response is flushed
//     when `fetch` resolves, exactly as with any Node handler.
//   * `ctx.passThroughOnException` is accepted and ignored: on a thrown handler
//     this returns 500, it does not proxy to origin.
//   * `request.cf` is absent (no Cloudflare edge metadata).
//
const entryArg = process.argv[2];
if (!entryArg) {
  console.error('[hive-worker-launcher] missing entry argument');
  process.exit(1);
}
const { pathToFileURL } = await import('node:url');
const path = await import('node:path');
const http = await import('node:http');
const port = Number(process.env.PORT || 3000);

const mod = await import(pathToFileURL(path.resolve(entryArg)).href);
// Worker module shape: `export default { fetch }` (object) or, in the newer
// "plain" format, `export default fetch` (function). ESM/CJS interop can
// double-wrap the default export, so unwrap once before inspecting it.
let exported = mod.default;
if (exported && typeof exported === 'object' && typeof exported.default !== 'undefined') {
  exported = exported.default;
}
const handler =
  typeof exported === 'function' ? exported : exported && exported.fetch;

if (typeof handler !== 'function') {
  console.error(
    '[hive-worker-launcher] entry exported no Worker fetch handler (expected ' +
      '`export default { fetch }` or `export default fetch`)'
  );
  process.exit(1);
}

function toWebRequest(req, url) {
  const headers = new Headers();
  for (const [name, value] of Object.entries(req.headers)) {
    if (Array.isArray(value)) {
      for (const entry of value) headers.append(name, entry);
    } else if (value !== undefined) headers.append(name, String(value));
  }
  const hasBody = req.method !== 'GET' && req.method !== 'HEAD';
  return new Request(url, {
    method: req.method,
    headers,
    body: hasBody ? req : undefined,
    // Node's IncomingMessage is a readable stream, which is a valid BodyInit.
    duplex: hasBody ? 'half' : undefined,
  });
}

async function writeResponse(res, response) {
  res.statusCode = response.status;
  response.headers.forEach((value, name) => {
    // `content-length` / `transfer-encoding` are owned by Node once we stream.
    if (name === 'content-length' || name === 'transfer-encoding') return;
    res.setHeader(name, value);
  });
  if (!response.body) {
    res.end();
    return;
  }
  const reader = response.body.getReader();
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      res.write(value);
    }
  } finally {
    res.end();
  }
}

const server = http.createServer(async (req, res) => {
  const host = req.headers.host || `127.0.0.1:${port}`;
  const url = new URL(req.url || '/', `http://${host}`);
  const background = [];
  const ctx = {
    waitUntil(promise) {
      // Fire-and-forget with an explicit rejection log: an unhandled rejection
      // here would otherwise take the whole server down.
      const tracked = Promise.resolve(promise).catch((error) => {
        console.error('[hive-worker-launcher] ctx.waitUntil rejected:', error);
      });
      background.push(tracked);
    },
    passThroughOnException() {
      // Accepted for API compatibility; not emulated (see the header comment).
    },
  };
  try {
    const response = await handler(toWebRequest(req, url), process.env, ctx);
    if (!response || typeof response.status !== 'number') {
      res.statusCode = 500;
      res.end('[hive-worker-launcher] handler returned no Response');
      return;
    }
    await writeResponse(res, response);
  } catch (error) {
    console.error('[hive-worker-launcher] handler threw:', error);
    if (!res.headersSent) res.statusCode = 500;
    res.end('[hive-worker-launcher] handler threw');
  }
});

server.listen(port, '0.0.0.0', () => {
  console.log(`[hive-worker-launcher] worker handler listening on :${port}`);
});
