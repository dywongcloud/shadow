// Platform launcher for Node server apps that EXPORT their app (the Vercel
// zero-config Express shape: `export default app`, no `scripts.start`, the
// module never binds $PORT itself). Staged into the build dir by the platform
// next to `.hive-after-shim.cjs`; never part of the repository.
//
// Usage: node [--experimental-strip-types] .hive-express-launcher.mjs <entry>
//
// TypeScript entries run under Node's own type stripping (erasable syntax
// only) — full parity with a compiled build is deliberately not claimed; a
// non-erasable entry fails HERE with Node's own diagnostic instead of being
// served wrong.
const entryArg = process.argv[2];
if (!entryArg) {
  console.error('[hive-launcher] missing entry argument');
  process.exit(1);
}
const { pathToFileURL } = await import('node:url');
const path = await import('node:path');
const http = await import('node:http');
const net = await import('node:net');
const port = Number(process.env.PORT || 3000);

const mod = await import(pathToFileURL(path.resolve(entryArg)).href);
let app = mod.default ?? mod.app ?? mod.server ?? mod.handler;
// ESM/CJS interop can double-wrap the default export.
if (app && typeof app === 'object' && typeof app.default === 'function') {
  app = app.default;
}

if (typeof app === 'function') {
  // An Express app (any version) IS a Node request handler, and so is a bare
  // (req, res) function — wrap it in a plain HTTP server.
  http.createServer(app).listen(port, () => {
    console.log(`[hive-launcher] exported app listening on :${port}`);
  });
} else if (app && typeof app.listen === 'function') {
  // Framework object owning its own listen (fastify-style, or a bare
  // http.Server export). `await` tolerates both promise and non-promise
  // returns.
  await app.listen({ port, host: '0.0.0.0' });
  console.log(`[hive-launcher] exported server listening on :${port}`);
} else {
  // The module may have bound $PORT itself as an import side effect. Verify
  // instead of exiting silently with nothing listening — an unbound port here
  // must be a loud cold-start failure, not a hung deployment.
  setTimeout(() => {
    const probe = net.connect(port, '127.0.0.1');
    probe.on('connect', () => probe.end());
    probe.on('error', () => {
      console.error(
        '[hive-launcher] entry exported no callable app and nothing is listening on $PORT'
      );
      process.exit(1);
    });
  }, 1500);
}
