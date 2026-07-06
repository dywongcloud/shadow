// A "half-migrated" repo: BOTH bun.lock and pnpm-lock.yaml are committed (e.g.
// someone tried Bun, didn't finish the migration, and pnpm-lock.yaml never got
// deleted). Package-manager detection picks "bun" per lockfile precedence and
// logs a conflict warning naming pnpm-lock.yaml — but since there is no
// explicit runtime override (no vercel.json `runtime`/`bunVersion`, no Project
// Settings override), the RUNTIME stays whatever `detect_start_cmd` infers from
// this package.json's own `scripts.start` (`node server.js` => Node), proving
// package-manager choice never silently forces the runtime.
const http = require("http");
const PORT = process.env.PORT || 8000;
http
  .createServer((req, res) => {
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ msg: "hello from a plain Node function", pid: process.pid, path: req.url }));
  })
  .listen(PORT, "127.0.0.1", () => console.log(`listening on ${PORT}`));
