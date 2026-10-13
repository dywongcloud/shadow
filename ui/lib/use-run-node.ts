"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { currentTeam } from "./api";
import { resolveTargetFresh } from "./run-node-targets";
import { applyStatus, HOST_ABI_VERSION, initialRunNodeStatus, type RunNodeStatus } from "./run-node-status";
import { presenceState, readPresenceGeo, upsertPresence } from "./run-node-client";

const WORKER_URL = `/run-node-worker.js?abi=${HOST_ABI_VERSION}`;
const WORKER_NAME = `shadw-run-node-v${HOST_ABI_VERSION}`;
/** Well inside the backend's 90s presence TTL, so one missed publish never
 *  de-orbits a live node from the constellation. */
const PRESENCE_REFRESH_MS = 45_000;
/** The one mounted `useRunNode` instance that publishes presence (module scope = per document). */
let presenceOwner: object | null = null;
const DEFAULT_RELAYS = [
  "https://fc-sanjose.relay.shadw.app:3343",
  "https://fc-bangkok.relay.shadw.app:3343",
  "https://fc-virginia.relay.shadw.app:3343",
].join(",");
const RELAY = process.env.NEXT_PUBLIC_HIVE_BROWSER_RELAY || DEFAULT_RELAYS;

/** Page half of the browser↔browser direct lane: `RTCPeerConnection` is Window-only, so the
 *  run-node worker brokers a peer connection through a connected page (`meshPeerRequest` →
 *  `meshPeerPort` + transferred port → `meshPeerDone`). The page never learns a peer id, ICE
 *  server, capability or grant; not hosting is a supported state the worker's broker timeout
 *  reports. */
type MeshAgentModule = { startMeshAgent: (port: MessagePort) => { stop: () => void } };

function createMeshAgentBroker(post: (message: unknown, transfer?: Transferable[]) => void) {
  let agent: { stop: () => void; requestId: number } | null = null;
  let generation = 0;

  const end = (requestId?: number) => {
    generation += 1;
    if (!agent) return;
    if (requestId !== undefined && agent.requestId !== requestId) return;
    try {
      agent.stop();
    } catch {}
    agent = null;
  };

  const host = (requestId: number) => {
    end();
    if (typeof RTCPeerConnection !== "function") return;
    const mine = generation;
    const url = `${window.location.origin}/browser-node/peer-mesh-agent.js`;
    void (import(/* webpackIgnore: true */ /* turbopackIgnore: true */ url) as Promise<MeshAgentModule>)
      .then((module) => {
        if (mine !== generation) return;
        const channel = new MessageChannel();
        const started = module.startMeshAgent(channel.port1);
        agent = { stop: () => started.stop(), requestId };
        post({ type: "meshPeerPort", requestId }, [channel.port2]);
      })
      .catch(() => {
      });
  };

  return { host, end };
}

/** Page half of node-worker's synchronous-`fs` bridge: `navigator.serviceWorker` is Window-only,
 *  so the run-node worker brokers the sw.js registration through a connected page
 *  (`syncFsRegisterRequest` → `syncFsRegisterResult`); the registration logic lives in the
 *  published agent module, loaded from the deployed origin at runtime. */
type SyncFsAgentModule = {
  registerNodeWorkerSyncFs: (options: {
    swUrl: string;
    workerUrl: string;
    scope?: string;
  }) => Promise<{ scope: string; scriptURL: string; prefix: string; workerCovered: boolean }>;
};

function createSyncFsBroker(post: (message: unknown) => void) {
  let generation = 0;

  const host = (requestId: number, swUrl: string, workerUrl: string, agentUrl?: string) => {
    const mine = ++generation;
    const url = agentUrl || `${window.location.origin}/browser-node/sync-fs-agent.js`;
    void (import(/* webpackIgnore: true */ /* turbopackIgnore: true */ url) as Promise<SyncFsAgentModule>)
      .then((module) =>
        mine === generation ? module.registerNodeWorkerSyncFs({ swUrl, workerUrl }) : null,
      )
      .then((result) => {
        if (mine !== generation || !result) return;
        post({
          type: "syncFsRegisterResult",
          requestId,
          ok: true,
          scope: result.scope,
          scriptURL: result.scriptURL,
          prefix: result.prefix,
        });
      })
      .catch((error: unknown) => {
        if (mine !== generation) return;
        post({
          type: "syncFsRegisterResult",
          requestId,
          ok: false,
          error: String((error as { message?: string })?.message ?? error).slice(0, 300),
        });
      });
  };

  return { host };
}

/** Page half of the node-worker EXECUTION lane: a SharedWorker scope has no `Worker`
 *  constructor and the substrate's synchronous `fs` needs a service worker, so the run-node
 *  worker asks a connected page to host the guest and bridge a MessageChannel
 *  (`nodeWorkerRequest` → `nodeWorkerPort` + port → `nodeWorkerDone`). The agent learns the
 *  artifact's source bytes and nothing else. */
type NodeWorkerAgentModule = {
  startNodeWorkerAgent: (port: MessagePort) => { stop: () => void };
};

function createNodeWorkerBroker(post: (message: unknown, transfer?: Transferable[]) => void) {
  let generation = 0;
  let agent: { stop: () => void; requestId: number } | null = null;

  const end = (requestId?: number) => {
    generation += 1;
    if (!agent) return;
    if (requestId !== undefined && agent.requestId !== requestId) return;
    try {
      agent.stop();
    } catch {}
    agent = null;
  };

  const host = (requestId: number) => {
    end();
    const mine = generation;
    const url = `${window.location.origin}/browser-node/node-worker-agent.js`;
    void (import(/* webpackIgnore: true */ /* turbopackIgnore: true */ url) as Promise<NodeWorkerAgentModule>)
      .then(module => {
        if (mine !== generation) return;
        const channel = new MessageChannel();
        const started = module.startNodeWorkerAgent(channel.port1);
        agent = { stop: () => started.stop(), requestId };
        post({ type: "nodeWorkerPort", requestId }, [channel.port2]);
      })
      .catch(() => {
      });
  };

  return { host, end };
}

const LAST_START_KEY = "hive_run_node_last_start";
function persistLastStart(args: StartArgs) {
  try {
    localStorage.setItem(
      LAST_START_KEY,
      JSON.stringify({
        deployment: args.deployment,
        fn: args.fn,
        scope: args.scope ?? "team",
        serveMode: args.serveMode ?? "auto",
      }),
    );
  } catch {}
}
function readLastStart(): StartArgs | null {
  try {
    const raw = localStorage.getItem(LAST_START_KEY);
    if (!raw) return null;
    const parsed = JSON.parse(raw);
    if (!parsed || typeof parsed.deployment !== "string" || typeof parsed.fn !== "string") return null;
    return parsed as StartArgs;
  } catch {
    return null;
  }
}
function clearLastStart() {
  try {
    localStorage.removeItem(LAST_START_KEY);
  } catch {}
}

export interface StartArgs {
  /** Empty = attach to nothing: mesh, relay identity and presence only, no serve lane and no database. */
  deployment: string;
  /** Empty with a non-empty `deployment` = database-only node; empty with an empty `deployment` = no target. */
  fn: string;
  /** Policy digest resolved from fresh deployment metadata by the picker or the handoff replay; empty when nothing is served. */
  digest: string;
  /** What to serve when no deployment is pinned: `"auto"` lets the fleet derive this tenant's
   *  eligible set on every renewal; `"none"` is retired and only read back from older persisted
   *  records, which replay as `"auto"`. Ignored whenever `deployment` is set. */
  serveMode?: "auto" | "none";
  scope?: "team" | "public";
}

/** Owns (or attaches to) the single per-origin `run-node` SharedWorker and exposes its live
 *  status + control actions; every mounted consumer shares the same worker. */
/** Network Information API `saveData` — the one explicit, broadly supported user signal;
 *  `false` wherever `navigator.connection` is absent. */
function dataSaverActive(): boolean {
  if (typeof navigator === "undefined") return false;
  const nav = navigator as Navigator & { connection?: { saveData?: boolean; addEventListener?: (type: string, listener: () => void) => void; removeEventListener?: (type: string, listener: () => void) => void } };
  return !!nav.connection?.saveData;
}

export function useRunNode() {
  const [status, setStatusState] = useState<RunNodeStatus>(initialRunNodeStatus());
  const [ownerToken] = useState<object>(() => ({}));
  const [dataSaverBlocked, setDataSaverBlocked] = useState(false);
  useEffect(() => {
    const update = () => setDataSaverBlocked(dataSaverActive());
    update();
    const conn = (
      navigator as Navigator & {
        connection?: { addEventListener?: (t: string, l: () => void) => void; removeEventListener?: (t: string, l: () => void) => void };
      }
    ).connection;
    conn?.addEventListener?.("change", update);
    return () => conn?.removeEventListener?.("change", update);
  }, []);
  const hasSharedWorker = typeof window !== "undefined" && "SharedWorker" in window;
  const hasLocksFallback =
    typeof window !== "undefined" && "Worker" in window && typeof navigator !== "undefined" && "locks" in navigator;
  const [supported, setSupported] = useState(
    () => typeof window === "undefined" || hasSharedWorker || hasLocksFallback,
  );
  const sendRef = useRef<(msg: object) => void>(() => {});
  const tabIdRef = useRef<string | null>(null);
  function tabId(): string {
    if (!tabIdRef.current) {
      tabIdRef.current =
        typeof crypto !== "undefined" && "randomUUID" in crypto
          ? crypto.randomUUID()
          : `tab-${Date.now()}-${Math.random().toString(36).slice(2)}`;
    }
    return tabIdRef.current;
  }
  const lastHeartbeatRef = useRef<number>(0);
  const reconnectRef = useRef<() => void>(() => {});
  const [replayError, setReplayError] = useState<string | null>(null);

  const replayLastStart = useCallback((post: (msg: object) => void) => {
    const last = readLastStart();
    if (!last) return;
    if (!last.deployment) {
      setReplayError(null);
      post({
        type: "start",
        relay: RELAY,
        deployment: "",
        fn: "",
        digest: "",
        serveMode: "auto",
        scope: last.scope ?? "team",
        team: currentTeam(),
      });
      return;
    }
    resolveTargetFresh(last.deployment, last.fn)
      .then((res) => {
        if (!res.ok) {
          clearLastStart();
          setReplayError(res.reason);
          return;
        }
        setReplayError(null);
        post({
          type: "start",
          relay: RELAY,
          deployment: res.target.deployment,
          fn: res.target.fn,
          digest: res.target.policyDigest,
          scope: last.scope ?? "team",
          team: currentTeam(),
        });
      })
      .catch(() => {
      });
  }, []);

  const applyIncomingStatus = useCallback((raw: RunNodeStatus & { abiVersion?: number }) => {
    lastHeartbeatRef.current = Date.now();
    const hostAbiStale = (raw.abiVersion ?? 0) < HOST_ABI_VERSION;
    setStatusState((prev) =>
      applyStatus(prev, { ...raw, hostAbiStale, tabCount: raw.tabCount ?? 1, sessionStale: !!raw.sessionStale }),
    );
  }, []);

  useEffect(() => {
    if (typeof window === "undefined") return;
    lastHeartbeatRef.current = Date.now();

    if (hasSharedWorker) {
      let worker: SharedWorker;
      let port!: MessagePort;
      let reconnecting = false;

      let dbBridge: { worker: Worker; requestId: string } | null = null;
      const endDbBridge = (requestId?: string) => {
        if (!dbBridge) return;
        if (requestId !== undefined && dbBridge.requestId !== requestId) return;
        dbBridge.worker.terminate();
        dbBridge = null;
      };
      let meshBroker: ReturnType<typeof createMeshAgentBroker> | null = null;
      let syncFsBroker: ReturnType<typeof createSyncFsBroker> | null = null;
      let nodeWorkerBroker: ReturnType<typeof createNodeWorkerBroker> | null = null;

      const wire = (p: MessagePort) => {
        sendRef.current = (msg) => p.postMessage(msg);
        meshBroker?.end();
        meshBroker = createMeshAgentBroker((message, transfer) => p.postMessage(message, transfer ?? []));
        syncFsBroker = createSyncFsBroker((message) => p.postMessage(message));
        nodeWorkerBroker = createNodeWorkerBroker((message, transfer) => p.postMessage(message, transfer ?? []));
        p.onmessage = (e: MessageEvent) => {
          const msg = e.data;
          if (msg && msg.type === "status") applyIncomingStatus(msg.status);
          else if (msg && msg.type === "meshPeerRequest") meshBroker?.host(msg.requestId);
          else if (msg && msg.type === "meshPeerDone") meshBroker?.end(msg.requestId);
          else if (msg && msg.type === "syncFsRegisterRequest") {
            syncFsBroker?.host(msg.requestId, msg.swUrl, msg.workerUrl, msg.agentUrl);
          } else if (msg && msg.type === "nodeWorkerRequest") {
            nodeWorkerBroker?.host(msg.requestId);
          } else if (msg && msg.type === "nodeWorkerDone") {
            nodeWorkerBroker?.end(msg.requestId);
          } else if (msg && msg.type === "dbWorkerRequest") {
            endDbBridge();
            try {
              const w = new Worker("/browser-node/sqlite/sqlite-worker.js", { type: "module" });
              const chan = new MessageChannel();
              chan.port1.onmessage = (ev) => w.postMessage(ev.data);
              w.onmessage = (ev) => chan.port1.postMessage(ev.data);
              dbBridge = { worker: w, requestId: msg.requestId };
              p.postMessage({ type: "dbWorkerPort", requestId: msg.requestId }, [chan.port2]);
            } catch {}
          } else if (msg && msg.type === "dbWorkerDone") {
            endDbBridge(msg.requestId);
          }
        };
        p.start();
        p.postMessage({ type: "status" });
        replayLastStart((msg) => p.postMessage(msg));
      };

      const connect = (): boolean => {
        try {
          worker = new SharedWorker(WORKER_URL, { type: "module", name: WORKER_NAME });
        } catch {
          queueMicrotask(() => setSupported(false));
          return false;
        }
        port = worker.port;
        wire(port);
        worker.onerror = () => reconnectRef.current();
        return true;
      };

      if (!connect()) return;

      reconnectRef.current = () => {
        if (reconnecting) return;
        reconnecting = true;
        try {
          port.close();
        } catch {}
        lastHeartbeatRef.current = Date.now();
        connect();
        reconnecting = false;
      };

      const reportVisibility = () => {
        const visible = document.visibilityState === "visible";
        port.postMessage({ type: "visibility", visible });
      };
      const onPageHide = (e: PageTransitionEvent) => {
        port.postMessage({ type: "visibility", visible: false, unloading: !e.persisted });
      };
      const onPageShow = () => reportVisibility();

      let audioCtx: AudioContext | null = null;
      const startKeepAlive = () => {
        if (audioCtx) return;
        try {
          const Ctor =
            window.AudioContext ??
            (window as unknown as { webkitAudioContext?: typeof AudioContext }).webkitAudioContext;
          if (!Ctor) return;
          const ctx = new Ctor();
          const osc = ctx.createOscillator();
          const gain = ctx.createGain();
          gain.gain.value = 0;
          osc.connect(gain).connect(ctx.destination);
          osc.start();
          audioCtx = ctx;
          void ctx.resume().catch(() => {});
        } catch {
          audioCtx = null;
        }
      };
      const stopKeepAlive = () => {
        const ctx = audioCtx;
        audioCtx = null;
        if (ctx) void ctx.close().catch(() => {});
      };
      startKeepAlive();

      document.addEventListener("visibilitychange", reportVisibility);
      window.addEventListener("pagehide", onPageHide);
      window.addEventListener("pageshow", onPageShow);
      document.addEventListener("freeze", reportVisibility);
      document.addEventListener("resume", reportVisibility);
      reportVisibility();

      return () => {
        document.removeEventListener("visibilitychange", reportVisibility);
        window.removeEventListener("pagehide", onPageHide);
        window.removeEventListener("pageshow", onPageShow);
        document.removeEventListener("freeze", reportVisibility);
        document.removeEventListener("resume", reportVisibility);
        stopKeepAlive();
        endDbBridge();
        meshBroker?.end();
        nodeWorkerBroker?.end();
        port.postMessage({ type: "visibility", visible: false, unloading: true });
        port.close();
        sendRef.current = () => {};
        reconnectRef.current = () => {};
      };
    }

    if (hasLocksFallback) {
      const statusChannel = new BroadcastChannel("shadw-run-node-status");
      const controlChannel = new BroadcastChannel("shadw-run-node-control");
      let dedicatedWorker: Worker | null = null;
      let releaseLock: (() => void) | null = null;
      let cancelled = false;
      const myTabId = tabId();

      statusChannel.onmessage = (e: MessageEvent) => applyIncomingStatus(e.data);

      const reportVisibility = () => {
        const visible = document.visibilityState === "visible";
        sendRef.current({ type: "visibility", tabId: myTabId, visible });
      };
      const onPageHide = (e: PageTransitionEvent) => {
        sendRef.current({ type: "visibility", tabId: myTabId, visible: false, unloading: !e.persisted });
      };
      const onPageShow = () => reportVisibility();

      let meshBroker: ReturnType<typeof createMeshAgentBroker> | null = null;
      let syncFsBroker: ReturnType<typeof createSyncFsBroker> | null = null;
      let nodeWorkerBroker: ReturnType<typeof createNodeWorkerBroker> | null = null;

      const wireOwnerWorker = (worker: Worker) => {
        meshBroker?.end();
        meshBroker = createMeshAgentBroker((message, transfer) => worker.postMessage(message, transfer ?? []));
        syncFsBroker = createSyncFsBroker((message) => worker.postMessage(message));
        nodeWorkerBroker = createNodeWorkerBroker((message, transfer) => worker.postMessage(message, transfer ?? []));
        worker.onmessage = (e: MessageEvent) => {
          const msg = e.data;
          if (msg && msg.type === "status") {
            applyIncomingStatus(msg.status);
            statusChannel.postMessage(msg.status);
          } else if (msg && msg.type === "meshPeerRequest") {
            meshBroker?.host(msg.requestId);
          } else if (msg && msg.type === "meshPeerDone") {
            meshBroker?.end(msg.requestId);
          } else if (msg && msg.type === "syncFsRegisterRequest") {
            syncFsBroker?.host(msg.requestId, msg.swUrl, msg.workerUrl, msg.agentUrl);
          } else if (msg && msg.type === "nodeWorkerRequest") {
            nodeWorkerBroker?.host(msg.requestId);
          } else if (msg && msg.type === "nodeWorkerDone") {
            nodeWorkerBroker?.end(msg.requestId);
          }
        };
        worker.onerror = () => reconnectRef.current();
        worker.postMessage({ type: "status" });
        controlChannel.onmessage = (e: MessageEvent) => worker.postMessage(e.data);
        sendRef.current = (msg) => worker.postMessage(msg);
        reportVisibility();
      };

      const spawnOwnerWorker = (replay: boolean) => {
        const worker = new Worker(WORKER_URL, { type: "module" });
        dedicatedWorker = worker;
        wireOwnerWorker(worker);
        if (replay) {
          replayLastStart((msg) => worker.postMessage(msg));
        }
      };

      reconnectRef.current = () => {
        if (!dedicatedWorker) return;
        const dead = dedicatedWorker;
        try {
          dead.terminate();
        } catch {}
        lastHeartbeatRef.current = Date.now();
        spawnOwnerWorker(true);
      };

      navigator.locks.request("shadw-run-node-owner", { mode: "exclusive" }, () => {
        if (cancelled) return;
        return new Promise<void>((resolve) => {
          releaseLock = resolve;
          spawnOwnerWorker(true);
        });
      });

      if (!dedicatedWorker) {
        sendRef.current = (msg) => controlChannel.postMessage(msg);
      }

      document.addEventListener("visibilitychange", reportVisibility);
      window.addEventListener("pagehide", onPageHide);
      window.addEventListener("pageshow", onPageShow);
      document.addEventListener("freeze", reportVisibility);
      document.addEventListener("resume", reportVisibility);
      reportVisibility();

      return () => {
        document.removeEventListener("visibilitychange", reportVisibility);
        window.removeEventListener("pagehide", onPageHide);
        window.removeEventListener("pageshow", onPageShow);
        document.removeEventListener("freeze", reportVisibility);
        document.removeEventListener("resume", reportVisibility);
        sendRef.current({ type: "visibility", tabId: myTabId, visible: false, unloading: true });
        cancelled = true;
        meshBroker?.end();
        nodeWorkerBroker?.end();
        statusChannel.close();
        controlChannel.close();
        reconnectRef.current = () => {};
        if (dedicatedWorker) {
          const worker = dedicatedWorker;
          try {
            worker.postMessage({ type: "visibility", visible: false, unloading: true });
          } catch {}
          setTimeout(() => worker.terminate(), 500);
        }
        releaseLock?.();
        sendRef.current = () => {};
      };
    }

    queueMicrotask(() => setSupported(false));
    return;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [applyIncomingStatus]);

  useEffect(() => {
    if (typeof window === "undefined") return;
    const HEARTBEAT_INTERVAL_MS = 15_000;
    const HEARTBEAT_TIMEOUT_MS = 45_000;
    const id = setInterval(() => {
      sendRef.current({ type: "status" });
      if (Date.now() - lastHeartbeatRef.current > HEARTBEAT_TIMEOUT_MS) {
        reconnectRef.current();
      }
    }, HEARTBEAT_INTERVAL_MS);
    return () => clearInterval(id);
  }, []);

  useEffect(() => {
    if (typeof window === "undefined" || typeof navigator === "undefined") return;
    const nav = navigator as Navigator & {
      connection?: {
        saveData?: boolean;
        addEventListener?: (type: string, listener: () => void) => void;
        removeEventListener?: (type: string, listener: () => void) => void;
      };
    };
    const conn = nav.connection;
    if (!conn?.addEventListener) return;
    const onChange = () => {
      const active = !!conn.saveData;
      setDataSaverBlocked(active);
      if (active) {
        clearLastStart();
        sendRef.current({ type: "stop" });
      }
    };
    conn.addEventListener("change", onChange);
    return () => conn.removeEventListener?.("change", onChange);
  }, []);

  const start = useCallback((args: StartArgs) => {
    if (dataSaverActive()) {
      setDataSaverBlocked(true);
      return;
    }
    persistLastStart(args);
    setReplayError(null);
    sendRef.current({
      type: "start",
      relay: RELAY,
      deployment: args.deployment,
      fn: args.fn,
      digest: args.digest,
      serveMode: args.serveMode ?? "auto",
      scope: args.scope ?? "team",
      team: currentTeam(),
    });
  }, []);

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

  useEffect(() => {
    if (typeof window === "undefined") return;
    const endpointId = status.endpointId;
    const state = presenceState(status.lifecycle);
    if (!endpointId || !state) return;
    if (presenceOwner !== null && presenceOwner !== ownerToken) return;
    presenceOwner = ownerToken;
    const publish = () => {
      const { lat, lon } = readPresenceGeo();
      upsertPresence({
        endpoint_id: endpointId,
        lat,
        lon,
        relay_hint: status.relay ?? "",
        state,
      }).catch(() => {
      });
    };
    publish();
    const timer = setInterval(publish, PRESENCE_REFRESH_MS);
    return () => {
      clearInterval(timer);
      if (presenceOwner === ownerToken) presenceOwner = null;
    };
  }, [status.endpointId, status.lifecycle, status.relay, ownerToken]);

  const stop = useCallback(() => {
    clearLastStart();
    sendRef.current({ type: "stop" });
  }, []);

  const setGeoConsent = useCallback((value: "granted" | "denied") => {
    sendRef.current({ type: "geoConsent", value });
  }, []);

  return { status, supported, start, stop, setGeoConsent, dataSaverBlocked, replayError };
}
