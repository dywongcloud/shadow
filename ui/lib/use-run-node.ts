"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { currentTeam } from "./api";
import { applyStatus, HOST_ABI_VERSION, initialRunNodeStatus, type RunNodeStatus } from "./run-node-status";

const WORKER_URL = "/run-node-worker.js";
const RELAY = process.env.NEXT_PUBLIC_HIVE_BROWSER_RELAY || "";

// Web-Locks-fallback owner handoff (bn-ui-sharedworker-owner): unlike a real
// SharedWorker (which outlives any single tab), a Web Locks re-election spawns
// a BRAND NEW dedicated Worker with no memory of what was running before —
// the crashed/closed owner's node silently never comes back unless something
// tells the fresh worker to start again. Persisted (not just in-memory) so it
// survives the owner tab actually closing, which is exactly the case that
// needs it. `relay`/`team` are deliberately NOT persisted here — they're
// re-derived fresh (RELAY from env, team via currentTeam()) at replay time
// rather than trusting a stale snapshot.
const LAST_START_KEY = "hive_run_node_last_start";
function persistLastStart(args: StartArgs) {
  try {
    localStorage.setItem(
      LAST_START_KEY,
      JSON.stringify({ deployment: args.deployment, fn: args.fn, digest: args.digest, scope: args.scope ?? "team" }),
    );
  } catch {
    /* storage unavailable/full — replay-on-handoff just won't happen */
  }
}
function readLastStart(): StartArgs | null {
  try {
    const raw = localStorage.getItem(LAST_START_KEY);
    if (!raw) return null;
    const parsed = JSON.parse(raw);
    if (!parsed || typeof parsed.deployment !== "string") return null;
    return parsed as StartArgs;
  } catch {
    return null;
  }
}
function clearLastStart() {
  try {
    localStorage.removeItem(LAST_START_KEY);
  } catch {
    /* ignore */
  }
}

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
  const hasSharedWorker = typeof window !== "undefined" && "SharedWorker" in window;
  const hasLocksFallback =
    typeof window !== "undefined" && "Worker" in window && typeof navigator !== "undefined" && "locks" in navigator;
  const [supported, setSupported] = useState(
    () => typeof window === "undefined" || hasSharedWorker || hasLocksFallback,
  );
  // Every control action (start/stop/geoConsent) goes through this ref rather
  // than a fixed `portRef`, because WHERE it needs to go differs by path: the
  // SharedWorker port directly, the owner tab's own dedicated Worker
  // directly, or (a non-owner tab under the Web Locks fallback) a
  // BroadcastChannel the lock-holding tab is listening on. Callers never need
  // to know which.
  const sendRef = useRef<(msg: object) => void>(() => {});

  const applyIncomingStatus = useCallback((raw: RunNodeStatus & { abiVersion?: number }) => {
    // hostAbiStale is derived HERE, not carried by the worker: the worker can
    // only report its OWN abiVersion, never whether it's stale relative to
    // what THIS page build expects. Absent entirely means a pre-versioning
    // worker instance (still running from before this field existed) — also
    // stale by definition.
    const hostAbiStale = (raw.abiVersion ?? 0) < HOST_ABI_VERSION;
    setStatusState((prev) => applyStatus(prev, { ...raw, hostAbiStale }));
  }, []);

  useEffect(() => {
    if (typeof window === "undefined") return;

    if (hasSharedWorker) {
      let worker: SharedWorker;
      try {
        worker = new SharedWorker(WORKER_URL, { type: "module", name: "shadw-run-node" });
      } catch {
        // Real, pre-existing eslint react-hooks/set-state-in-effect violation,
        // fixed while touching this file for the host-ABI-versioning change
        // above: a setState call synchronous within the effect body risks
        // cascading renders per the rule's own rationale. queueMicrotask defers
        // it by one microtask turn (functionally instantaneous, no visible
        // delay) without changing behavior -- this DOES need to set state (a
        // SharedWorker constructor throwing despite the capability check
        // passing is a real runtime failure the UI must reflect), so the fix is
        // deferral, not removal.
        queueMicrotask(() => setSupported(false));
        return;
      }
      const port = worker.port;
      sendRef.current = (msg) => port.postMessage(msg);
      port.onmessage = (e: MessageEvent) => {
        const msg = e.data;
        if (msg && msg.type === "status") applyIncomingStatus(msg.status);
      };
      port.start();
      port.postMessage({ type: "status" });

      // Page Lifecycle: tell the worker whether THIS tab can currently promise
      // foreground reliability, so it can report "suspended" honestly instead
      // of claiming "online" while every connected tab is hidden/frozen/gone.
      // The SharedWorker itself is not subject to page lifecycle (it keeps the
      // real connection alive across a bfcache entry, which is the point of a
      // shared owner) — only the STATUS communicated to the user changes.
      const reportVisibility = () => {
        const visible = document.visibilityState === "visible";
        port.postMessage({ type: "visibility", visible });
      };
      const onPageHide = (e: PageTransitionEvent) => {
        // persisted=true: entering bfcache, may resume — report hidden, keep
        // the worker connection running. persisted=false: this tab is truly
        // going away — best-effort tell the worker so a last-tab-closed
        // detection isn't the only signal (real close is unload-adjacent and
        // unreliable to detect from the worker side alone).
        port.postMessage({ type: "visibility", visible: false, unloading: !e.persisted });
      };
      const onPageShow = () => reportVisibility();
      document.addEventListener("visibilitychange", reportVisibility);
      window.addEventListener("pagehide", onPageHide);
      window.addEventListener("pageshow", onPageShow);
      // freeze/resume (Page Lifecycle API) — narrower than visibilitychange on
      // browsers that support it (e.g. a visible-but-discarded background tab).
      document.addEventListener("freeze", reportVisibility);
      document.addEventListener("resume", reportVisibility);
      reportVisibility();

      return () => {
        document.removeEventListener("visibilitychange", reportVisibility);
        window.removeEventListener("pagehide", onPageHide);
        window.removeEventListener("pageshow", onPageShow);
        document.removeEventListener("freeze", reportVisibility);
        document.removeEventListener("resume", reportVisibility);
        port.postMessage({ type: "visibility", visible: false, unloading: true });
        port.close();
        sendRef.current = () => {};
      };
    }

    if (hasLocksFallback) {
      // Web Locks fallback (bn-ui-sharedworker-owner): no SharedWorker in this
      // browser (Safari private mode, an older engine). Exactly one tab per
      // origin wins the `shadw-run-node-owner` exclusive lock and runs a
      // plain dedicated Worker hosting the SAME run-node-worker.js script
      // (see its own SharedWorkerGlobalScope-detection branch) — every other
      // tab relays control through BroadcastChannel and mirrors status from
      // it, never running a second Worker/BrowserNode instance of its own.
      const statusChannel = new BroadcastChannel("shadw-run-node-status");
      const controlChannel = new BroadcastChannel("shadw-run-node-control");
      let dedicatedWorker: Worker | null = null;
      let releaseLock: (() => void) | null = null;
      let cancelled = false;

      // Every tab (owner or not) mirrors relayed status immediately — cheap,
      // and correct even for the owner tab itself (its own relayed broadcast
      // is a strict duplicate of what applyIncomingStatus already applied
      // directly, and applyStatus's generation fencing drops the duplicate).
      statusChannel.onmessage = (e: MessageEvent) => applyIncomingStatus(e.data);

      navigator.locks.request("shadw-run-node-owner", { mode: "exclusive" }, () => {
        if (cancelled) return;
        return new Promise<void>((resolve) => {
          releaseLock = resolve;
          dedicatedWorker = new Worker(WORKER_URL, { type: "module" });
          const worker = dedicatedWorker;
          worker.onmessage = (e: MessageEvent) => {
            const msg = e.data;
            if (msg && msg.type === "status") {
              applyIncomingStatus(msg.status);
              statusChannel.postMessage(msg.status);
            }
          };
          worker.postMessage({ type: "status" });
          // Owner handoff replay: this fresh worker boots at lifecycle
          // "stopped" with no memory of anything — if the LAST thing this
          // origin was doing was running a node (persisted by start(),
          // cleared by stop()), replay it now instead of silently staying
          // stopped until a human notices and re-clicks Start. Harmless
          // no-op on a normal first-ever start (nothing persisted yet).
          const last = readLastStart();
          if (last) {
            worker.postMessage({
              type: "start",
              relay: RELAY,
              deployment: last.deployment,
              fn: last.fn,
              digest: last.digest,
              scope: last.scope ?? "team",
              team: currentTeam(),
            });
          }
          // Forward control messages non-owner tabs couldn't send directly.
          controlChannel.onmessage = (e: MessageEvent) => worker.postMessage(e.data);
          sendRef.current = (msg) => worker.postMessage(msg);
        });
      });

      // Until (or unless) this tab wins the lock above, and for every OTHER
      // tab for this dedicated Worker's whole lifetime, control goes out over
      // the broadcast channel instead of a direct reference — the effect
      // above overwrites this the instant this tab itself wins ownership.
      if (!dedicatedWorker) {
        sendRef.current = (msg) => controlChannel.postMessage(msg);
      }

      return () => {
        cancelled = true;
        statusChannel.close();
        controlChannel.close();
        if (dedicatedWorker) {
          const worker = dedicatedWorker;
          // Give the worker a chance to see this as its last connection and
          // self-stop (releasing its backend admission) via the SAME
          // unloading-visibility path the SharedWorker branch above already
          // uses (connectPort/onPortVisibility in run-node-worker.js is
          // shared code, not a reimplementation) — a bare terminate() with no
          // signal leaves the admission to live out its full lease
          // unnecessarily. Bounded: stop() does an awaited network DELETE,
          // which terminate() would otherwise cut off mid-flight regardless.
          try {
            worker.postMessage({ type: "visibility", visible: false, unloading: true });
          } catch {
            /* already gone */
          }
          setTimeout(() => worker.terminate(), 500);
        }
        releaseLock?.();
        sendRef.current = () => {};
      };
    }

    queueMicrotask(() => setSupported(false));
    return;
    // hasSharedWorker/hasLocksFallback are derived from `window`/`navigator`
    // once per module evaluation (both read a global that doesn't change
    // within a single page load), so they're intentionally excluded from the
    // dependency array — including them would be redundant, never wrong.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [applyIncomingStatus]);

  const start = useCallback((args: StartArgs) => {
    persistLastStart(args);
    sendRef.current({
      type: "start",
      relay: RELAY,
      deployment: args.deployment,
      fn: args.fn,
      digest: args.digest,
      scope: args.scope ?? "team",
      team: currentTeam(),
    });
  }, []);

  // Reconnect machine (bn-p2p-reconnect-state, one real slice of it): real
  // network drop/restore events, reported through the SAME sendRef used by
  // every other control action — works unchanged whether the SharedWorker or
  // Web Locks fallback path is live, since both assign sendRef.current to
  // whatever the correct live transport is. A separate effect (not folded
  // into the connection effect above) because it has no cleanup dependency
  // on which transport is active; it just needs sendRef to exist.
  useEffect(() => {
    if (typeof window === "undefined") return;
    const report = () => sendRef.current({ type: "network", online: navigator.onLine });
    window.addEventListener("online", report);
    window.addEventListener("offline", report);
    report();
    return () => {
      window.removeEventListener("online", report);
      window.removeEventListener("offline", report);
    };
  }, []);

  const stop = useCallback(() => {
    clearLastStart();
    sendRef.current({ type: "stop" });
  }, []);

  const setGeoConsent = useCallback((value: "granted" | "denied") => {
    sendRef.current({ type: "geoConsent", value });
  }, []);

  return { status, supported, start, stop, setGeoConsent };
}
