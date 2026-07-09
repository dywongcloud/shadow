// Helpers to spawn the GuardianDB PostgreSQL gateway for the runnable demos.
//
// In a real deployment you do NOT do this — you run a long-lived gateway
// (`cargo run --features pgwire --bin guardian-pgwire`, or the replicated
// `postgres_iroh_gateway` example) and point TypeORM straight at it. These
// helpers just make `npm run demo` / `npm run crud` self-contained.

import { spawn, ChildProcess } from "node:child_process";
import net from "node:net";
import path from "node:path";
import fs from "node:fs";

/** Locate the `guardian-pgwire` binary built from the repo root. */
export function gatewayBinary(): string {
  if (process.env.GUARDIAN_PGWIRE_BIN) return process.env.GUARDIAN_PGWIRE_BIN;
  const root = path.resolve(__dirname, "..", "..", "..");
  for (const rel of ["target/debug/guardian-pgwire", "target/release/guardian-pgwire"]) {
    const full = path.join(root, rel);
    if (fs.existsSync(full)) return full;
  }
  throw new Error("Build the gateway first: cargo build --features pgwire --bin guardian-pgwire");
}

/** Reserve an ephemeral TCP port (so concurrent demos never collide). */
export function freePort(): Promise<number> {
  return new Promise((res, rej) => {
    const s = net.createServer();
    s.on("error", rej);
    s.listen(0, "127.0.0.1", () => {
      const p = (s.address() as net.AddressInfo).port;
      s.close(() => res(p));
    });
  });
}

/** Wait until something accepts TCP connections on `port`. */
export async function waitPort(port: number, host = "127.0.0.1"): Promise<void> {
  for (let i = 0; i < 80; i++) {
    const ok = await new Promise<boolean>((res) => {
      const s = net.connect(port, host);
      s.on("connect", () => {
        s.destroy();
        res(true);
      });
      s.on("error", () => res(false));
    });
    if (ok) return;
    await new Promise((r) => setTimeout(r, 120));
  }
  throw new Error("gateway did not start");
}

export interface RunningGateway {
  /** The TCP port the gateway is listening on. */
  port: number;
  /** Stop the gateway process. */
  stop(): void;
}

/**
 * Spawn the in-memory dev gateway on a free port and wait until it is ready.
 * Remember to call `.stop()` when finished.
 */
export async function startGateway(database = "app"): Promise<RunningGateway> {
  const port = await freePort();
  const proc: ChildProcess = spawn(
    gatewayBinary(),
    ["--addr", `127.0.0.1:${port}`, "--database", database],
    { stdio: "ignore" },
  );
  proc.on("error", (e) => {
    throw e;
  });
  await waitPort(port);
  return { port, stop: () => void proc.kill("SIGKILL") };
}
