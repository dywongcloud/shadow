// Platform launcher for Node server apps without a `scripts.start`: the entry
// either exports its app (`export default app`, the Vercel zero-config Express
// shape) or listens on $PORT itself. Staged into the build dir by the platform;
// never part of the repository.
//
// Usage: node [--experimental-transform-types] .hive-express-launcher.mjs <entry>
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
const listenDeadlineMs = Number(process.env.HIVE_LAUNCHER_LISTEN_DEADLINE_MS || 60_000);
const probeIntervalMs = 250;

const portBound = () =>
  new Promise((resolve) => {
    const probe = net.connect(port, '127.0.0.1');
    probe.on('connect', () => { probe.end(); resolve(true); });
    probe.on('error', () => resolve(false));
  });
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

const mod = await import(pathToFileURL(path.resolve(entryArg)).href);
let app = mod.default ?? mod.app ?? mod.server ?? mod.handler;
if (app && typeof app === 'object' && typeof app.default === 'function') {
  app = app.default;
}

// A `listen()` issued during module evaluation binds on a later tick.
await sleep(50);
const selfListening = (await portBound()) || app?.listening === true || app?.server?.listening === true;

if (selfListening) {
  console.log(`[hive-launcher] entry already listening on :${port}`);
} else if (typeof app === 'function') {
  http.createServer(app).listen(port, () => {
    console.log(`[hive-launcher] exported app listening on :${port}`);
  });
} else if (app && typeof app.listen === 'function') {
  await app.listen({ port, host: '0.0.0.0' });
  console.log(`[hive-launcher] exported server listening on :${port}`);
} else {
  const deadline = Date.now() + listenDeadlineMs;
  let bound = false;
  while (!bound && Date.now() < deadline) {
    await sleep(probeIntervalMs);
    bound = await portBound();
  }
  if (!bound) {
    console.error(
      `[hive-launcher] entry exported no callable app and nothing listened on $PORT within ${listenDeadlineMs} ms`
    );
    process.exit(1);
  }
  console.log(`[hive-launcher] entry bound :${port} on its own`);
}
