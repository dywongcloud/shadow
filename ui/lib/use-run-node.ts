"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { currentTeam } from "./api";
import { resolveTargetFresh } from "./run-node-targets";
import { applyStatus, HOST_ABI_VERSION, initialRunNodeStatus, type RunNodeStatus } from "./run-node-status";
import { presenceState, readPresenceGeo, upsertPresence } from "./run-node-client";

/* The ABI version is part of the worker's IDENTITY, not just something it
 * reports. A SharedWorker is keyed by (url, name) and OUTLIVES page reloads
 * for as long as any tab holds it — so before this, shipping a new worker
 * build did nothing for anyone with a tab already open: every reload
 * re-attached to the OLD instance, which kept requesting the old
 * `?v=<WASM_BUNDLE_VERSION>` wasm (cache-first in sw.js) and kept throwing an
 * error string that no longer exists in the deployed binary. The only
 * documented remedy was "close every tab first", which no user will discover.
 * Versioning the url + name makes a post-deploy page attach to a genuinely
 * new worker immediately; the stale one simply dies with its last tab. */
const WORKER_URL = `/run-node-worker.js?abi=${HOST_ABI_VERSION}`;
const WORKER_NAME = `shadw-run-node-v${HOST_ABI_VERSION}`;
/** Well inside the backend's 90s presence TTL, so one missed publish never
 *  de-orbits a live node from the constellation. */
const PRESENCE_REFRESH_MS = 45_000;
/** Claimed by whichever mounted `useRunNode` instance publishes presence, so
 *  /run-node (page + sidebar control, two instances) doesn't double the POST
 *  rate for one node. Module scope = per document, which is the right grain:
 *  the node is per-document too (one SharedWorker per origin, surfaced into
 *  each tab). */
let presenceOwner: object | null = null;
// bn-impl-relay-tls: real WSS relays now exist (deployed + live-witnessed
// 2026-08-01 on fc-bangkok/fc-virginia/fc-sanjose) -- before this, an unset
// NEXT_PUBLIC_HIVE_BROWSER_RELAY meant `boot()` always received an empty
// string, which fails RelayUrl parsing immediately (see
// crates/hive-browser/src/lib.rs's `boot`), so a browser node was never
// actually functional in any deployment that hadn't separately configured
// this var. `BrowserNode.boot()` now takes a COMMA-SEPARATED relay list
// (same convention as the fleet's own HIVE_RELAY_URLS) and builds a real
// iroh RelayMap covering all of them, so this hands all 3 real relays by
// default -- iroh's own relay-client picks and fails over between entries in
// a RelayMap on its own; this is real multi-relay coverage, not just one
// static URL. NEXT_PUBLIC_HIVE_BROWSER_RELAY still overrides the whole list
// when set (e.g. a deployment that wants to pin to a subset).
const DEFAULT_RELAYS = [
  "https://fc-sanjose.relay.shadw.app:3343",
  "https://fc-bangkok.relay.shadw.app:3343",
  "https://fc-virginia.relay.shadw.app:3343",
].join(",");
const RELAY = process.env.NEXT_PUBLIC_HIVE_BROWSER_RELAY || DEFAULT_RELAYS;

// Web-Locks-fallback owner handoff (bn-ui-sharedworker-owner): unlike a real
// SharedWorker (which outlives any single tab), a Web Locks re-election spawns
// a BRAND NEW dedicated Worker with no memory of what was running before —
// the crashed/closed owner's node silently never comes back unless something
// tells the fresh worker to start again. Persisted (not just in-memory) so it
// survives the owner tab actually closing, which is exactly the case that
// needs it. `relay`/`team` are deliberately NOT persisted here — they're
// re-derived fresh (RELAY from env, team via currentTeam()) at replay time
// rather than trusting a stale snapshot.
// browser-run-node-target-picker: the DIGEST is no longer persisted either —
// only the stable deployment+function selection. The digest is re-derived
// from fresh deployment descriptor metadata at replay time (see
// replayLastStart), so a target rotation (redeploy → new policy_digest under
// the same name) replays the CURRENT artifact, and a stale/deleted/ineligible
// selection fails visibly and clears instead of replaying forever.
const LAST_START_KEY = "hive_run_node_last_start";
function persistLastStart(args: StartArgs) {
  try {
    localStorage.setItem(
      LAST_START_KEY,
      JSON.stringify({ deployment: args.deployment, fn: args.fn, scope: args.scope ?? "team" }),
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
    if (!parsed || typeof parsed.deployment !== "string" || typeof parsed.fn !== "string") return null;
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
  /** The descriptor's policy digest, resolved from fresh deployment metadata
   *  by the picker (or the handoff replay) — never a donor-typed value. */
  digest: string;
  scope?: "team" | "public";
}

/** Owns (or attaches to) the single per-origin `run-node` SharedWorker and
 *  exposes its live status + control actions. Multiple mounted consumers
 *  (navbar control + the /run-node page open in another tab) share the SAME
 *  worker instance — the browser's own SharedWorker semantics give us the
 *  "exactly one owner" property for free; this hook is just a typed port. */
/** Network Information API's `saveData` flag (bn-ui-mobile-lifecycle's
 *  network-policy item): the one broadly-supported, EXPLICIT signal a user
 *  actually chose ("Data Saver" in Chrome/Android settings) — unlike
 *  `effectiveType`/`downlink`, which are heuristic estimates that vary
 *  minute to minute and would make this policy flap. Deliberately does NOT
 *  attempt battery-based policy: the Battery Status API this row's text
 *  names has been removed from Firefox/Safari and is fingerprinting-gated
 *  even in Chrome, so a battery check would only ever fire on a shrinking
 *  slice of browsers while silently no-op'ing everywhere else — a real
 *  signal with universal support beats a nominally-broader one with none.
 *  `navigator.connection` itself is Chromium-only; every read here is
 *  optional-chained and this returns `false` (never block) where it's
 *  absent, matching every other progressive-feature-detection pattern in
 *  this file (hasSharedWorker/hasLocksFallback above). */
function dataSaverActive(): boolean {
  if (typeof navigator === "undefined") return false;
  const nav = navigator as Navigator & { connection?: { saveData?: boolean; addEventListener?: (type: string, listener: () => void) => void; removeEventListener?: (type: string, listener: () => void) => void } };
  return !!nav.connection?.saveData;
}

export function useRunNode() {
  const [status, setStatusState] = useState<RunNodeStatus>(initialRunNodeStatus());
  // Stable per-instance identity for the presence-publisher claim below.
  const ownerToken = useRef<object>({}).current;
  // Network policy (bn-ui-mobile-lifecycle): distinct from `status.lastError`
  // (a worker-reported failure) — this is a CLIENT-SIDE policy refusal that
  // never reaches the worker at all, so it needs its own state rather than
  // overloading RunNodeStatus's wire shape with a field the worker itself
  // has no way to produce.
  // SSR-safe: starts false (the server has no `navigator`, so it must render as
  // "not blocked" to match), and the real value + live updates land in the
  // effect below. Reading dataSaverActive() in the initializer diverged the
  // first client render from the SSR HTML for any data-saver user (React #418,
  // the same hydration-mismatch class as the run-node page's persisted state),
  // and it also never updated when the connection changed mid-session.
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
  // Stable per-TAB (not per-worker) identity (bn-ui-sharedworker-owner, the
  // multi-tab-visibility item): under the Web Locks fallback several tabs
  // share ONE real transport to the worker (`self`, owned by whichever tab
  // won the lock) — tagging this tab's own control/visibility messages with
  // an id lets the worker track each tab's contribution independently
  // instead of every message colliding on the single shared channel. Lazily
  // created once per hook mount; SSR has no `crypto`, so it's created only
  // when first needed (inside the client-only effect below), never here.
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
  // Worker crash recovery (bn-ui-sharedworker-owner, the row's last remaining
  // item): a background worker dying mid-session — an OOM reclaim, a wasm
  // panic that takes the whole global scope with it, anything short of the
  // owning tab itself closing (already handled by Web Locks re-election / a
  // fresh SharedWorker connection) — previously had no detector at all; the
  // node would just silently stop responding until a human noticed and
  // manually reloaded. lastHeartbeatMs is bumped on EVERY inbound status
  // message (the one chokepoint every transport already funnels through —
  // same "count where every caller converges" reasoning as this repo's own
  // request-cancellation guidance), so a live worker's periodic broadcasts
  // (and this hook's own watchdog pings, below) both count as proof of life.
  // Seeded 0 (not Date.now()) -- calling an impure function as a useRef
  // initializer runs during render, which react-hooks/purity correctly flags.
  // The real timestamp is stamped in the connection effect below instead,
  // which runs post-render; a 0 default briefly read by the watchdog before
  // that effect commits would just look "very stale", never worse than a
  // harmless, no-op-on-a-fresh-mount reconnect attempt.
  const lastHeartbeatRef = useRef<number>(0);
  const reconnectRef = useRef<() => void>(() => {});
  // Owner-handoff replay failure (browser-run-node-target-picker): when a
  // persisted selection no longer resolves against fresh deployment metadata
  // (deleted deployment, no-longer-ready, function no longer browser-eligible)
  // the replay is dropped, the persisted start is cleared, and the reason is
  // surfaced here for the page to render — "fail visibly and clear", never a
  // silent skip that would replay forever on the next handoff.
  const [replayError, setReplayError] = useState<string | null>(null);

  // Owner-handoff replay, revalidated: a persisted selection is never replayed
  // verbatim — the digest is re-derived from FRESH descriptor metadata, so a
  // rotated target (redeploy → new policy_digest under the same
  // deployment+function) picks up the CURRENT artifact instead of a stale one.
  // A fetch failure is transient: the persisted start is left alone for the
  // next handoff/reload to retry, and nothing is cleared or reported.
  const replayLastStart = useCallback((post: (msg: object) => void) => {
    const last = readLastStart();
    if (!last) return;
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
        /* transient fetch failure — leave the persisted start for the next handoff */
      });
  }, []);

  const applyIncomingStatus = useCallback((raw: RunNodeStatus & { abiVersion?: number }) => {
    lastHeartbeatRef.current = Date.now();
    // hostAbiStale is derived HERE, not carried by the worker: the worker can
    // only report its OWN abiVersion, never whether it's stale relative to
    // what THIS page build expects. Absent entirely means a pre-versioning
    // worker instance (still running from before this field existed) — also
    // stale by definition.
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
      // Definite-assignment assertion: `port` is written inside connect()/wire()
      // (nested functions, so TS's linear control-flow analysis can't see the
      // assignment from here) but is always assigned before reportVisibility/
      // onPageHide/cleanup below ever run — `if (!connect()) return;`
      // guarantees connect() succeeded (and therefore wire() assigned `port`)
      // before any of those closures are even registered.
      let port!: MessagePort;
      let reconnecting = false;

      // db-worker broker (bn-dashboard-db-worker-broker-hook): a SharedWorker
      // global scope has no `Worker` constructor, so the run-node worker's db
      // lane asks a connected page to host the sqlite DedicatedWorker and
      // bridge a MessageChannel to it (protocol spec in run-node-worker.js's
      // header). One broker per page — a fresh request supersedes; the worker
      // re-asks another page if this one dies (OPFS + watermarks survive).
      let dbBridge: { worker: Worker; requestId: string } | null = null;
      const endDbBridge = (requestId?: string) => {
        if (!dbBridge) return;
        if (requestId !== undefined && dbBridge.requestId !== requestId) return;
        dbBridge.worker.terminate();
        dbBridge = null;
      };

      const wire = (p: MessagePort) => {
        sendRef.current = (msg) => p.postMessage(msg);
        p.onmessage = (e: MessageEvent) => {
          const msg = e.data;
          if (msg && msg.type === "status") applyIncomingStatus(msg.status);
          else if (msg && msg.type === "dbWorkerRequest") {
            endDbBridge();
            try {
              const w = new Worker("/browser-node/sqlite/sqlite-worker.js", { type: "module" });
              const chan = new MessageChannel();
              // Two-way bridge: everything the worker posts to its end goes to
              // the sqlite worker and vice versa (plain JSON envelopes only).
              chan.port1.onmessage = (ev) => w.postMessage(ev.data);
              w.onmessage = (ev) => chan.port1.postMessage(ev.data);
              dbBridge = { worker: w, requestId: msg.requestId };
              p.postMessage({ type: "dbWorkerPort", requestId: msg.requestId }, [chan.port2]);
            } catch {
              // Unanswered request: the worker's own timeout surfaces an
              // honest status.db error — never pretend a broker exists.
            }
          } else if (msg && msg.type === "dbWorkerDone") {
            endDbBridge(msg.requestId);
          }
        };
        p.start();
        p.postMessage({ type: "status" });
        // Owner handoff replay (bn-ui-mobile-lifecycle, OS-process-kill item):
        // the Web Locks fallback branch below has always done this, but this
        // SharedWorker branch never did -- if the browser's whole SharedWorker
        // process is reclaimed under memory pressure (real on mobile Chrome,
        // and the reason the Locks fallback exists at all; also reachable on
        // desktop under genuine OOM), the NEXT `new SharedWorker(...)` this
        // page issues (on reload, or via reconnectRef's crash-recovery retry
        // above) spawns a genuinely fresh worker with no memory of anything,
        // and it would otherwise sit at "stopped" until a human noticed.
        // Harmless when the worker is already alive and running: the worker's
        // own `start()` guard (`if (node || status.lifecycle === "starting")
        // return;`) makes a replay against an ALREADY-running node a no-op,
        // so every ordinary tab connecting to an already-live SharedWorker
        // (the common case) costs one ignored message, never a duplicate boot.
        replayLastStart((msg) => p.postMessage(msg));
      };

      const connect = (): boolean => {
        try {
          worker = new SharedWorker(WORKER_URL, { type: "module", name: WORKER_NAME });
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
          return false;
        }
        port = worker.port;
        wire(port);
        // Worker crash recovery (bn-ui-sharedworker-owner): an ErrorEvent on
        // the SharedWorker object itself (distinct from the port) is the
        // browser's own signal that the shared worker's global scope threw
        // uncaught — reconnect immediately rather than waiting out the
        // watchdog's slower silence-based detection below.
        worker.onerror = () => reconnectRef.current();
        return true;
      };

      if (!connect()) return;

      reconnectRef.current = () => {
        if (reconnecting) return;
        reconnecting = true;
        try {
          port.close();
        } catch {
          /* already gone */
        }
        // Optimistic reset: don't let the watchdog re-trigger on its very
        // next tick before this fresh connection has had a chance to prove
        // itself — the `new SharedWorker(...)` call below either attaches to
        // the same still-alive process (a transient in-worker error) or the
        // browser spins up a fresh one (the process genuinely died); either
        // way a real status response lands within one heartbeat interval.
        lastHeartbeatRef.current = Date.now();
        connect();
        reconnecting = false;
      };

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
        endDbBridge();
        port.postMessage({ type: "visibility", visible: false, unloading: true });
        port.close();
        sendRef.current = () => {};
        reconnectRef.current = () => {};
      };
    }

    if (hasLocksFallback) {
      // Web Locks fallback (bn-ui-sharedworker-owner): no SharedWorker in this
      // browser (an older engine, or a context whose SharedWorker constructor
      // exists but cannot spawn). NOT "Safari private mode": Safari 26 HAS
      // SharedWorker in normal AND private windows (witnessed live on Safari
      // 26.3.1), so modern Safari takes the SharedWorker branch above — its
      // remaining context quirk (a CryptoKey is not structured-cloneable
      // inside a SharedWorker) is handled by identity.js's memory-custody
      // mode (bn-safari-sharedworker-cryptokey-dataclone), not by routing
      // here. Exactly one tab per origin wins the `shadw-run-node-owner`
      // exclusive lock and runs a plain dedicated Worker hosting the SAME
      // run-node-worker.js script (see its own SharedWorkerGlobalScope-
      // detection branch) — every other tab relays control through
      // BroadcastChannel and mirrors status from it, never running a second
      // Worker/BrowserNode instance of its own.
      const statusChannel = new BroadcastChannel("shadw-run-node-status");
      const controlChannel = new BroadcastChannel("shadw-run-node-control");
      let dedicatedWorker: Worker | null = null;
      let releaseLock: (() => void) | null = null;
      let cancelled = false;
      const myTabId = tabId();

      // Every tab (owner or not) mirrors relayed status immediately — cheap,
      // and correct even for the owner tab itself (its own relayed broadcast
      // is a strict duplicate of what applyIncomingStatus already applied
      // directly, and applyStatus's generation fencing drops the duplicate).
      statusChannel.onmessage = (e: MessageEvent) => applyIncomingStatus(e.data);

      // Page Lifecycle (bn-ui-sharedworker-owner, the row's 2nd remaining
      // item): previously wired ONLY for the SharedWorker branch above —
      // under this fallback neither the owner tab nor any observer tab ever
      // reported visibility, so online<->suspended toggling never happened
      // here at all. Every tab (owner or not) reports through `sendRef`,
      // which already resolves to whichever transport is live for THIS tab
      // (direct to the worker once owned, controlChannel otherwise) — tagged
      // with this tab's own id so several tabs sharing the owner's single
      // real `self` channel are tracked independently by the worker's
      // per-tabId onPortVisibility path instead of colliding on one key.
      const reportVisibility = () => {
        const visible = document.visibilityState === "visible";
        sendRef.current({ type: "visibility", tabId: myTabId, visible });
      };
      const onPageHide = (e: PageTransitionEvent) => {
        sendRef.current({ type: "visibility", tabId: myTabId, visible: false, unloading: !e.persisted });
      };
      const onPageShow = () => reportVisibility();

      const wireOwnerWorker = (worker: Worker) => {
        worker.onmessage = (e: MessageEvent) => {
          const msg = e.data;
          if (msg && msg.type === "status") {
            applyIncomingStatus(msg.status);
            statusChannel.postMessage(msg.status);
          }
        };
        // Worker crash recovery (bn-ui-sharedworker-owner, the row's 3rd
        // remaining item): mirrors the SharedWorker branch's onerror handler
        // above — only the OWNER tab can see this (only it holds the real
        // Worker reference), and only the owner can act on it (spawn a
        // replacement). A non-owner tab has nothing to reconnect itself; if
        // the owner TAB is what died (not just its worker), that's already
        // covered by the Web Locks re-election electing a different tab.
        worker.onerror = () => reconnectRef.current();
        worker.postMessage({ type: "status" });
        // Forward control messages non-owner tabs couldn't send directly.
        controlChannel.onmessage = (e: MessageEvent) => worker.postMessage(e.data);
        sendRef.current = (msg) => worker.postMessage(msg);
        reportVisibility(); // this tab's own visibility, now that it owns a direct channel
      };

      const spawnOwnerWorker = (replay: boolean) => {
        const worker = new Worker(WORKER_URL, { type: "module" });
        dedicatedWorker = worker;
        wireOwnerWorker(worker);
        // Owner handoff replay: this fresh worker boots at lifecycle
        // "stopped" with no memory of anything — if the LAST thing this
        // origin was doing was running a node (persisted by start(),
        // cleared by stop()), replay it now instead of silently staying
        // stopped until a human notices and re-clicks Start. Harmless
        // no-op on a normal first-ever start (nothing persisted yet), and
        // the SAME replay a genuine crash-recovery restart needs — a worker
        // that died mid-session should come back serving the same thing.
        // The replay is revalidated against fresh deployment metadata
        // (browser-run-node-target-picker) — see replayLastStart.
        if (replay) {
          replayLastStart((msg) => worker.postMessage(msg));
        }
      };

      reconnectRef.current = () => {
        if (!dedicatedWorker) return; // not the owner — nothing here to restart
        const dead = dedicatedWorker;
        try {
          dead.terminate();
        } catch {
          /* already gone */
        }
        lastHeartbeatRef.current = Date.now(); // same optimistic-reset reasoning as the SharedWorker branch
        spawnOwnerWorker(true);
      };

      navigator.locks.request("shadw-run-node-owner", { mode: "exclusive" }, () => {
        if (cancelled) return;
        return new Promise<void>((resolve) => {
          releaseLock = resolve;
          spawnOwnerWorker(true);
        });
      });

      // Until (or unless) this tab wins the lock above, and for every OTHER
      // tab for this dedicated Worker's whole lifetime, control goes out over
      // the broadcast channel instead of a direct reference — the effect
      // above overwrites this the instant this tab itself wins ownership.
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
        statusChannel.close();
        controlChannel.close();
        reconnectRef.current = () => {};
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

  // Worker crash recovery watchdog (bn-ui-sharedworker-owner, the row's last
  // remaining item): periodically pings whatever transport is currently live
  // and, if no status response — direct or relayed, both flow through
  // applyIncomingStatus which bumps lastHeartbeatRef — has landed within
  // HEARTBEAT_TIMEOUT_MS, treats the worker as gone and reconnects via
  // reconnectRef (assigned by whichever branch of the connection effect
  // above is active). A separate effect, same reasoning as the network
  // online/offline effect below: no cleanup dependency on which transport is
  // active, it only needs sendRef/reconnectRef to exist. The timeout is
  // deliberately generous relative to the interval — a legitimately busy
  // worker (mid cold-boot wasm init, a slow admission round-trip) must never
  // be mistaken for a crash and torn down mid-operation.
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

  // Network policy (bn-ui-mobile-lifecycle): watch for Data Saver turning ON
  // WHILE a node is running, not just at start-gate time — a user can enable
  // it from OS settings mid-session without this tab ever reloading. Auto-
  // stops rather than merely degrading the displayed status: continuing to
  // relay OTHER tenants' invocation traffic over a connection the user has
  // explicitly told the OS to conserve is the one case this feature must
  // never do silently, matching the row's own "never promising continuous
  // serving where the platform cannot provide it" framing — here inverted to
  // "never CONSUMING what the user asked to conserve," the donor-side
  // analogue of the same honesty requirement.
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
    if (!conn?.addEventListener) return; // Network Information API unsupported — nothing to watch
    const onChange = () => {
      const active = !!conn.saveData;
      setDataSaverBlocked(active);
      if (active) {
        clearLastStart(); // don't let owner-handoff replay resurrect a node this policy just stopped
        sendRef.current({ type: "stop" });
      }
    };
    conn.addEventListener("change", onChange);
    return () => conn.removeEventListener?.("change", onChange);
  }, []);

  const start = useCallback((args: StartArgs) => {
    if (dataSaverActive()) {
      // Refuse at the source rather than starting then immediately being
      // stopped by the change-listener above — Data Saver already being on
      // BEFORE the user clicks Start never fires a "change" event to catch.
      setDataSaverBlocked(true);
      return;
    }
    persistLastStart(args);
    setReplayError(null); // an explicit manual start supersedes any stale handoff-replay failure
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

  // PRESENCE HEARTBEAT — lives here, in the hook, because the hook is mounted
  // app-wide by the sidebar's run-node control while the node itself is owned
  // by a SharedWorker that outlives any single page. It used to be a
  // `useEffect` inside the /run-node PAGE, whose cleanup cleared the interval
  // on unmount: navigating to /network to LOOK at the constellation was itself
  // what stopped the heartbeat, and the server's 90s presence TTL then expired
  // the satellite off the very map the user had just opened. The record is
  // republished well inside that TTL (PRESENCE_REFRESH_MS) for as long as the
  // node reports a publishable lifecycle, from whatever page is open.
  useEffect(() => {
    if (typeof window === "undefined") return;
    const endpointId = status.endpointId;
    const state = presenceState(status.lifecycle);
    if (!endpointId || !state) return;
    // One publisher per document: /run-node mounts this hook alongside the
    // sidebar control, and both would otherwise POST the same record on the
    // same interval. The upsert is idempotent, so this is purely about not
    // doubling the request rate.
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
        // Transient publish failures are recovered by the next tick; the
        // server TTL is 2x this interval, so a single miss never de-orbits
        // a live node.
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
