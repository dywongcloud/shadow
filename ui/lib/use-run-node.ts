"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { currentTeam } from "./api";
import { applyStatus, initialRunNodeStatus, type RunNodeStatus } from "./run-node-status";

const WORKER_URL = "/run-node-worker.js";
const RELAY = process.env.NEXT_PUBLIC_HIVE_BROWSER_RELAY || "";

export interface StartArgs {
  deployment: string;
  fn: string;
  digest: string;
  scope?: "team" | "public";
}

/** Owns (or attaches to) the single per-origin `run-node` SharedWorker and
 *  exposes its live status + control actions. Multiple mounted consumers
 *  (navbar control + the /run-node page open in another tab) share the SAME
 *  worker instance — the browser's own SharedWorker semantics give us the
 *  "exactly one owner" property for free; this hook is just a typed port. */
export function useRunNode() {
  const [status, setStatusState] = useState<RunNodeStatus>(initialRunNodeStatus());
  // Environment capability check as a lazy initializer (runs once, during
  // this component's own render) rather than an effect-time setState — SSR
  // has no `window`/`SharedWorker`, so it starts `true` there and the real
  // client-side answer lands on the client's own first render, no extra
  // effect-driven re-render in between. A SharedWorker constructor that
  // throws DESPITE the capability check passing (e.g. a CSP blocking module
  // workers) is a distinct runtime failure, not a feature-detection result,
  // and is handled by the try/catch below.
  const [supported, setSupported] = useState(() => typeof window === "undefined" || "SharedWorker" in window);
  const portRef = useRef<MessagePort | null>(null);

  useEffect(() => {
    if (typeof window === "undefined" || !("SharedWorker" in window)) return;
    let worker: SharedWorker;
    try {
      worker = new SharedWorker(WORKER_URL, { type: "module", name: "shadw-run-node" });
    } catch {
      setSupported(false);
      return;
    }
    const port = worker.port;
    portRef.current = port;
    port.onmessage = (e: MessageEvent) => {
      const msg = e.data;
      if (msg && msg.type === "status") {
        setStatusState((prev) => applyStatus(prev, msg.status as RunNodeStatus));
      }
    };
    port.start();
    port.postMessage({ type: "status" });
    return () => {
      port.close();
      portRef.current = null;
    };
  }, []);

  const start = useCallback((args: StartArgs) => {
    portRef.current?.postMessage({
      type: "start",
      relay: RELAY,
      deployment: args.deployment,
      fn: args.fn,
      digest: args.digest,
      scope: args.scope ?? "team",
      team: currentTeam(),
    });
  }, []);

  const stop = useCallback(() => {
    portRef.current?.postMessage({ type: "stop" });
  }, []);

  const setGeoConsent = useCallback((value: "granted" | "denied") => {
    portRef.current?.postMessage({ type: "geoConsent", value });
  }, []);

  return { status, supported, start, stop, setGeoConsent };
}
