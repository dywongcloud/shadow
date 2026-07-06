// Regression baseline: a plain Node function with NO Bun signal anywhere
// (no bun.lock, no packageManager field, no vercel.json runtime/bunVersion).
// This must build, detect, and serve EXACTLY as it did before any Bun-runtime
// work existed — proving the Bun additions are purely additive.
const http = require("http");
const PORT = process.env.PORT || 8000;
http
  .createServer((req, res) => {
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ msg: "hello from a plain Node function", pid: process.pid, path: req.url }));
  })
  .listen(PORT, "127.0.0.1", () => console.log(`listening on ${PORT}`));
