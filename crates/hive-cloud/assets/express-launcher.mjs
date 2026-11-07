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

// A TUN-only guest carries no host<->guest loopback. An unbounded connect here
// can never settle: the launcher would stall before it ever calls listen(), and
// the readiness gate would report "did not listen" with only Node's startup
// warning on stderr. Bound every probe, then always fall through to a bind.
const portBound = () =>
  new Promise((resolve) => {
    const probe = net.connect({ host: '127.0.0.1', port });
    let settled = false;
    let timer;
    const finish = (bound) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      probe.removeAllListeners();
      probe.destroy();
      resolve(bound);
    };
    // A wall-clock timer, not socket.setTimeout: on a SYN that never resolves
    // the socket is not yet connected, so its inactivity timeout never fires.
    timer = setTimeout(() => finish(false), 250);
    probe.on('connect', () => finish(true));
    probe.on('error', () => finish(false));
  });
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

const mod = await import(pathToFileURL(path.resolve(entryArg)).href);
let app = mod.default ?? mod.app ?? mod.server ?? mod.handler;
console.error(`[hive-launcher] imported ${entryArg}; default type ${typeof app}`);
if (app && typeof app === 'object' && typeof app.default === 'function') {
  app = app.default;
}

// A `listen()` issued during module evaluation binds on a later tick.
await sleep(50);
const selfListening = (await portBound()) || app?.listening === true || app?.server?.listening === true;

if (selfListening) {
  console.error(`[hive-launcher] entry already listening on :${port}`);
} else if (typeof app === 'function') {
  // Bind explicitly: the guest bind shim rewrites 0.0.0.0 to the real guest IP,
  // and the readiness probe is IPv4. A no-host bind resolves to :: on Node.
  http.createServer(app).listen(port, '0.0.0.0', () => {
    console.error(`[hive-launcher] exported app listening on :${port}`);
  });
} else if (app && typeof app.listen === 'function') {
  await app.listen({ port, host: '0.0.0.0' });
  console.error(`[hive-launcher] exported server listening on :${port}`);
} else {
  console.error('[hive-launcher] entry exported no callable app; waiting for a self-bind');
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
  console.error(`[hive-launcher] entry bound :${port} on its own`);
}
