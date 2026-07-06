// A Fluid function running on the Bun runtime: a long-lived HTTP server that
// handles many concurrent requests per instance, exactly like `examples/hello`
// (Python) and a plain Node function. Listens on $PORT. Bun's own event loop
// (like Node's) is I/O-friendly, so one instance safely serves many concurrent
// requests — see `recommended_safe_concurrency`'s "node"/"bun" arm.

const PORT = Number(process.env.PORT || 8000);
const PID = process.pid;
const STARTED = Date.now();

function json(obj, init = {}) {
  return new Response(JSON.stringify(obj), {
    headers: { "content-type": "application/json", ...(init.headers || {}) },
    status: init.status || 200,
  });
}

Bun.serve({
  port: PORT,
  hostname: "127.0.0.1",
  async fetch(req) {
    const url = new URL(req.url);

    if (url.pathname.startsWith("/api/slow")) {
      await Bun.sleep(1000); // simulate an I/O-bound wait (e.g. an LLM call)
      return json({ slow: true, pid: PID, path: url.pathname });
    }

    if (url.pathname.startsWith("/api/boom")) {
      throw new Error("intentional boom"); // must not take down other requests
    }

    if (url.pathname.startsWith("/api/echo") && req.method === "POST") {
      const ctx = req.headers.get("x-ctx") || "";
      const body = await req.text();
      return json({ path: url.pathname, ctx, body, pid: PID }, { headers: { "x-ctx": ctx } });
    }

    if (url.pathname.startsWith("/api/versions")) {
      // Proves this is really Bun, not Node — process.versions.bun only
      // exists under Bun. Used by the e2e "which runtime actually served
      // this" verification.
      return json({ bun: process.versions.bun ?? null, node: process.versions.node ?? null });
    }

    if (url.pathname.startsWith("/api/bg")) {
      // waitUntil (background continuation): respond immediately; the
      // `x-fluid-wait-until-ms` header tells the gateway to hold this
      // instance's lease open for that many ms AFTER responding, so
      // background work has a guaranteed window before the instance is
      // considered idle/eligible for scale-down. This is a pure HTTP response
      // header convention — no runtime-specific API needed, proven identical
      // under Bun by `fluid-gateway/tests/gateway.rs`'s
      // `waitUntil_lease_held_open_after_response_for_a_bun_function`.
      return json({ ok: true, note: "responded now; bg via waitUntil", pid: PID }, { headers: { "x-fluid-wait-until-ms": "600" } });
    }

    return json({
      msg: "hello from a Bun Fluid function",
      pid: PID,
      uptimeMs: Date.now() - STARTED,
      path: url.pathname,
    });
  },
});

console.log(`listening on ${PORT}`);
