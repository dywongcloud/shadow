// Proves Bun's native TypeScript execution: no separate `tsc`/ts-node compile
// step, no build command — Bun transpiles + runs `.ts` directly, and (unlike
// plain Node) gives accurate TS-source stack traces out of the box. This is
// the concrete "automatic source maps for un-bundled Bun execution" case: Bun
// provides this for free; only the BUNDLED bytecode-cache path (see
// `warmup_bun_bytecode`) needs an explicit `--sourcemap` flag.

interface Greeting {
  msg: string;
  pid: number;
  path: string;
}

const PORT: number = Number(process.env.PORT || 8000);

function greet(path: string): Greeting {
  return { msg: "hello from a Bun+TypeScript Fluid function", pid: process.pid, path };
}

Bun.serve({
  port: PORT,
  hostname: "127.0.0.1",
  fetch(req: Request): Response {
    const url = new URL(req.url);
    if (url.pathname.startsWith("/api/type-error")) {
      // Trigger a real runtime TypeError to prove Bun's stack trace points at
      // THIS .ts source line, not a transpiled intermediate.
      const x: any = null;
      return new Response(String(x.doesNotExist));
    }
    return Response.json(greet(url.pathname));
  },
});

console.log(`listening on ${PORT}`);
