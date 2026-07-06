// Hono is a Bun-first web framework (idiomatic Bun serving convention: a
// default export with `{port, fetch}`, no explicit Bun.serve() call needed —
// Bun auto-detects and serves it). Manually verified live: `bun run --bun
// start` (the platform's real detect_start_cmd invocation for a
// package.json#scripts.start project) reports process.versions.bun set, and
// an unhandled error in one request never takes down subsequent requests.
import { Hono } from "hono";

const app = new Hono();

app.get("/", (c) => c.json({ msg: "hello from a Hono Fluid function", bun: process.versions.bun ?? null }));

app.get("/api/boom", () => {
  throw new Error("intentional boom"); // must not take down other requests
});

app.onError((err, c) => c.json({ error: err.message }, 500));

export default {
  port: Number(process.env.PORT || 8000),
  fetch: app.fetch,
};
