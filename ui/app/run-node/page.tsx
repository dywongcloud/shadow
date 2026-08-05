"use client";

import { useEffect, useMemo, useRef, useState } from "react";
import { MapPin, RadioTower, ShieldCheck, TriangleAlert } from "lucide-react";
import { useRunNode } from "@/lib/use-run-node";
import { lifecycleLabel, dbLaneStateLabel, type RunNodeStatus, type DbLaneStatus } from "@/lib/run-node-status";
import { clearPresence, GEO_CONSENT_KEY, GEO_COORDS_KEY } from "@/lib/run-node-client";
import { usePoll, sessionMintStatus, type Deployment } from "@/lib/api";
import { excludedDeployments, resolveTarget, targetsFromDeployments } from "@/lib/run-node-targets";
import { TargetPicker, type TargetSelection } from "./target-picker";
// browser-run-node-target-picker: the persisted selection is the STABLE
// deployment+function pair only — never a digest. The digest is re-derived
// from fresh descriptor metadata on every render/start/replay, so a redeploy
// that rotates the policy digest under the same name keeps working and a
// deleted/ineligible target fails visibly and clears instead of replaying.
// browser-node-optional-serve-target: an EMPTY deployment is the persisted
// form of "attached to nothing", which is a real choice a node runs on — not
// an absent one.
const TARGET_KEY = "hive_run_node_target";
const GEO_QUANT_DEGREES = 0.5; // matches the server-side floor — defense in depth, not the only gate
// Re-derive the fix well before the browser's own 10-minute cache
// (`maximumAge` below) goes stale, so a long-running tab's dot on the
// constellation map tracks a laptop that changed networks instead of
// freezing at wherever it was when the tab was first opened.
const GEO_REFRESH_MS = 8 * 60_000;

function quantize(v: number): number {
  return Math.round(v / GEO_QUANT_DEGREES) * GEO_QUANT_DEGREES;
}

// Surfaces fix age once it's well past a fresh reading, rather than trusting
// the dot silently — `requestLocation`'s own retry (GEO_REFRESH_MS) keeps it
// from ever running too far past this in practice, but a laptop that's been
// asleep or a permission that started silently failing both leave `coords`
// holding an old value with no other visible signal.
function staleSuffix(locatedMs: number | null): string {
  if (locatedMs === null) return "";
  const ageMin = Math.floor((Date.now() - locatedMs) / 60_000);
  return ageMin >= 15 ? ` · fix is ${ageMin}m old` : "";
}


export default function RunNodePage() {
  const { status, supported, start, stop, setGeoConsent, dataSaverBlocked, replayError } = useRunNode();
  const dbStatus = (status as RunNodeStatus & { db?: DbLaneStatus | null }).db ?? null;
  // Additive worker field (browser-node-optional-serve-target): whether a
  // function artifact is actually PINNED right now, derived worker-side from
  // the runtime rather than from what was requested — so the page can state
  // plainly that a running node is not serving anything.
  const serving = (status as RunNodeStatus & { serving?: boolean }).serving === true;
  // Eligible serve targets come from the same authenticated /deployments list
  // the rest of the dashboard polls — served locally with leader/owner
  // fallback server-side (admin_ingress + the /cloud proxy), so this page
  // never needs a node-specific endpoint. Re-polled live so rotation,
  // deletion, and team switches are revalidated against current metadata.
  const { data: deployments, error: deploymentsError, loading: deploymentsLoading } =
    usePoll<Deployment[]>("/deployments", 5000);
  const targets = useMemo(() => targetsFromDeployments(deployments ?? []), [deployments]);
  // Not a count: the REASON each excluded deployment is excluded. A bare number
  // answered the one question nobody asks ("are some hidden?") and none of the
  // question every report actually asks ("which of mine, and why?").
  const excluded = useMemo(() => excludedDeployments(deployments ?? []), [deployments]);
  // Persisted selection + scope are hydrated AFTER mount (the effect below),
  // NOT in a lazy useState initializer. Reading localStorage during the first
  // render makes the client's initial tree diverge from the SSR HTML (the
  // server has no `window`, so it renders the defaults), and React aborts
  // hydration with #418 — regenerating the whole tree and, witnessed live,
  // leaving "Start" dead on any returning device. Initial state is the
  // SSR-safe default; the one-frame restore flash is the correct tradeoff.
  const [selection, setSelection] = useState<(TargetSelection & { scope?: "team" | "public" }) | null>(null);
  const [scope, setScope] = useState<"team" | "public">("team");
  // Auth-window guard (bn-picker-auth-window-empty-list-clears-selection):
  // while the session mint is in backoff/failed, requests proceed
  // unauthenticated and /deployments answers a REAL 200 scoped to an
  // anonymous/empty tenant — an [] that is indistinguishable from a genuinely
  // empty tenant. Judging the selection against THAT [] wiped the persisted
  // target permanently. Only the two KNOWN-degraded mint states hold
  // judgment: "ok" is authoritative, "unattempted" covers Clerk-off dev, and
  // a non-empty list is real data by construction.
  const mintState = sessionMintStatus().state;
  const authWindow =
    deployments !== null &&
    deployments.length === 0 &&
    (mintState === "backoff" || mintState === "failed");
  // The digest handed to Start is resolved from the CURRENT snapshot on every
  // render — never from the persisted selection — so a target rotated between
  // page load and click starts the current artifact, not a stale digest.
  // A null selection is the explicit "attach to nothing" choice and resolves
  // to nothing, which is not a failure and never blocks Start.
  const resolved =
    selection && deployments && !authWindow
      ? resolveTarget(deployments, selection.deployment, selection.fn)
      : null;
  const selectedTarget = resolved && resolved.ok ? resolved.target : null;
  // Revalidation failure, derived per render: while the list is still loading
  // (including the team-switch window, where usePoll drops data to null)
  // nothing is judged; the moment real data lands, a selection that no longer
  // resolves produces its reason immediately — no effect round trip.
  const revalidationFailure = selection && resolved && !resolved.ok ? resolved.reason : null;
  // …and its STICKY counterpart: the clear below un-sets the selection, which
  // makes the derived reason vanish on the very next render — a one-frame
  // flash, not "fails visibly". So the reason is copied into state at the
  // moment of failure and survives the clear until the user picks a new
  // target (choose) or starts one (which clears replayError the same way).
  const [clearedError, setClearedError] = useState<string | null>(null);
  const selectionError = revalidationFailure ?? clearedError;

  // A failed revalidation must also CLEAR (state + localStorage) so the dead
  // selection can never replay forever — "fail visibly and clear". This is a
  // genuine sync from an external system (the polled deployments list) into
  // state, so per react-hooks/set-state-in-effect's own rationale the fix is
  // deferral (one microtask turn, invisible), not removal — same precedent as
  // the SharedWorker-constructor failure path in use-run-node.ts.
  useEffect(() => {
    if (!revalidationFailure) return;
    queueMicrotask(() => {
      try {
        localStorage.removeItem(TARGET_KEY);
      } catch {
        /* ignore */
      }
      setClearedError(revalidationFailure);
      setSelection(null);
    });
  }, [revalidationFailure]);

  // `null` = the explicit "serve nothing" option, persisted as an empty
  // deployment so the choice (and the scope beside it) survives a reload
  // exactly like a real target does.
  function choose(sel: TargetSelection | null) {
    setSelection(sel);
    setClearedError(null);
    try {
      localStorage.setItem(
        TARGET_KEY,
        JSON.stringify({ deployment: sel?.deployment ?? "", fn: sel?.fn ?? "", scope }),
      );
    } catch {
      /* storage unavailable — the selection just won't survive a reload */
    }
  }

  function chooseScope(next: "team" | "public") {
    setScope(next);
    try {
      localStorage.setItem(
        TARGET_KEY,
        JSON.stringify({ deployment: selection?.deployment ?? "", fn: selection?.fn ?? "", scope: next }),
      );
    } catch {
      /* ignore */
    }
  }

  // Also hydrated after mount (the effect below), same #418 reason as
  // selection/scope above. geoDecision swaps ENTIRE subtrees (the consent
  // prompt vs the granted/denied block), so a lazy-initializer read of
  // GEO_CONSENT_KEY was the loudest of the three SSR≠client mismatches.
  const [geoDecision, setGeoDecision] = useState<"undecided" | "granted" | "denied">("undecided");

  // The ONE place persisted UI state is read from localStorage on load, run
  // once AFTER the first render has already committed the SSR-identical
  // defaults above — this is what keeps the hydrated tree byte-identical to the
  // server's HTML (no #418). Malformed/absent entries leave the defaults.
  useEffect(() => {
    try {
      const raw = localStorage.getItem(TARGET_KEY);
      if (raw) {
        const parsed = JSON.parse(raw);
        if (parsed && typeof parsed.deployment === "string" && typeof parsed.fn === "string") {
          // An empty deployment is the persisted "attached to nothing" choice
          // — restore it as a null selection, not as a target named "".
          setSelection(parsed.deployment ? { deployment: parsed.deployment, fn: parsed.fn } : null);
          if (parsed.scope === "public" || parsed.scope === "team") setScope(parsed.scope);
        }
      }
    } catch {
      /* ignore malformed persisted selection */
    }
    try {
      const savedGeo = localStorage.getItem(GEO_CONSENT_KEY);
      if (savedGeo === "granted" || savedGeo === "denied") setGeoDecision(savedGeo);
    } catch {
      /* ignore */
    }
  }, []);
  const [geoError, setGeoError] = useState<string | null>(null);
  const [coords, setCoords] = useState<{ lat: number; lon: number } | null>(null);
  const [locatedMs, setLocatedMs] = useState<number | null>(null);
  const geoTimer = useRef<ReturnType<typeof setInterval> | null>(null);
  // Focus restoration (bn-ui-accessibility): the "Don't share"/"Share" and
  // "Reset" buttons each unmount the moment they're clicked (the whole block
  // they're in swaps for a different one), which drops focus to <body> with
  // no browser-default landing spot — silently stranding a keyboard/screen-
  // reader user. Moved explicitly to the newly-mounted control that replaces
  // the one just clicked.
  const geoResetBtnRef = useRef<HTMLButtonElement>(null);
  const geoShareBtnRef = useRef<HTMLButtonElement>(null);
  // Focus moves ONLY in response to a real click (Share / Don't share / Reset),
  // never on mount and never on the post-mount localStorage hydration above —
  // both set geoDecision without user intent and must not steal focus. The old
  // first-render-skip guard only skipped ONE settle; hydration adds a second
  // non-interactive settle, so gate on user action instead.
  const geoUserActed = useRef(false);
  useEffect(() => {
    if (!geoUserActed.current) return;
    if (geoDecision === "undecided") geoShareBtnRef.current?.focus();
    else geoResetBtnRef.current?.focus();
  }, [geoDecision]);

  // Ask the browser's Geolocation API for a fresh fix. Split from `decideGeo`
  // so it can also run (a) on mount, when consent was already granted in a
  // PRIOR session — restoring `geoDecision` from localStorage alone never
  // called this, so a returning user's "waiting for a location fix" message
  // never cleared until they hit Reset — and (b) on a recurring timer, so a
  // long-lived tab's dot doesn't go stale.
  const requestLocation = useRef(() => {
    if (typeof navigator === "undefined" || !navigator.geolocation) {
      setGeoError("This browser has no Geolocation API — location will show as unknown.");
      return;
    }
    navigator.geolocation.getCurrentPosition(
      (pos) => {
        setGeoError(null);
        // Quantized client-side too, before it ever leaves the tab — the
        // server re-quantizes unconditionally regardless, but a client that
        // never transmits a precise fix is a real (not merely cosmetic)
        // privacy improvement against a network observer.
        setCoords({ lat: quantize(pos.coords.latitude), lon: quantize(pos.coords.longitude) });
        setLocatedMs(Date.now());
      },
      (err) => {
        // PERMISSION_DENIED (code 1) is a hard, durable block — the OS/browser
        // will refuse every future call identically until the user changes a
        // site setting, so treating it as "granted but erroring" would retry
        // forever against a wall. Revoke consent for real (and persist the
        // revocation) so the UI matches what's actually happening, and stop
        // publishing a location that's frozen at its last value.
        if (err.code === err.PERMISSION_DENIED) {
          localStorage.setItem(GEO_CONSENT_KEY, "denied");
          setGeoDecision("denied");
          setCoords(null);
          setLocatedMs(null);
          setGeoError("Location permission was denied in your browser — location sharing turned off. Re-enable it in your browser's site settings to share again.");
          return;
        }
        // POSITION_UNAVAILABLE (2) / TIMEOUT (3) are transient — a GPS/Wi-Fi
        // fix that failed once often succeeds on the next periodic retry, so
        // consent and any already-published coords are left alone rather than
        // flickering the satellite dot on and off the map.
        setGeoError(
          err.code === err.POSITION_UNAVAILABLE
            ? "Location temporarily unavailable — will keep retrying."
            : err.message || "Location request failed or was denied.",
        );
      },
      { enableHighAccuracy: false, maximumAge: 10 * 60_000, timeout: 15_000 },
    );
  });

  // Fire once whenever sharing is (or becomes) granted — covers both a fresh
  // "Share" click and a returning tab whose consent was restored from
  // localStorage — and keep re-firing on a timer while it stays granted so a
  // long session's fix doesn't go stale.
  useEffect(() => {
    if (geoDecision !== "granted") {
      if (geoTimer.current) {
        clearInterval(geoTimer.current);
        geoTimer.current = null;
      }
      return;
    }
    requestLocation.current();
    geoTimer.current = setInterval(() => requestLocation.current(), GEO_REFRESH_MS);
    return () => {
      if (geoTimer.current) {
        clearInterval(geoTimer.current);
        geoTimer.current = null;
      }
    };
  }, [geoDecision]);

  // The heartbeat itself now lives in `useRunNode` (mounted app-wide by the
  // sidebar's run-node control). It used to be an effect HERE, and this
  // page's unmount cleanup cleared its interval — so navigating to /network
  // to look at the constellation was itself what stopped refreshing this
  // node's satellite, which the server's 90s TTL then expired off the map.
  // What remains here is the half only this page can do: persist the
  // consented fix so the app-wide heartbeat can publish it from any route.
  useEffect(() => {
    if (typeof window === "undefined") return;
    if (geoDecision === "granted" && coords) {
      localStorage.setItem(GEO_COORDS_KEY, JSON.stringify({ lat: coords.lat, lon: coords.lon }));
    } else if (geoDecision !== "granted") {
      localStorage.removeItem(GEO_COORDS_KEY);
    }
  }, [geoDecision, coords?.lat, coords?.lon]);

  function decideGeo(next: "granted" | "denied") {
    geoUserActed.current = true;
    setGeoDecision(next);
    localStorage.setItem(GEO_CONSENT_KEY, next);
    setGeoConsent(next);
    if (next === "denied") {
      setCoords(null);
      setLocatedMs(null);
      setGeoError(null);
      return;
    }
    // The mount/consent-change effect above fires `requestLocation` whenever
    // `geoDecision` becomes "granted", including this transition — no direct
    // call needed here.
  }

  async function onStop() {
    stop();
    if (status.endpointId) {
      await clearPresence(status.endpointId).catch(() => {});
    }
  }

  // browser-node-optional-serve-target: running a node NEVER depends on having
  // something to serve. A donor whose deployments are all long-running servers
  // (Next.js, Express), containers, TypeScript or Python/Go has nothing a
  // browser engine can execute — a real engine constraint, not a policy — and
  // used to be locked out of the whole feature by this one boolean. The serve
  // lane is now the optional part; the node itself is not.
  const canStart = status.lifecycle === "stopped" && !dataSaverBlocked;

  // Explicit reason the Start button is disabled, so it is NEVER a silent dead
  // end — the "works on my other device but not this one" report was a node
  // that couldn't start with no visible cause. Both remaining causes have
  // their own banner above, so this stays null for them rather than saying the
  // same thing twice; a missing/failed TARGET is deliberately not a cause any
  // more, it just leaves the serve lane idle (see `serveNotice`).
  const startDisabledReason =
    status.lifecycle !== "stopped" || canStart
      ? null
      : !supported
        ? null // covered by the SharedWorker banner above
        : dataSaverBlocked
          ? null // covered by the Data Saver banner above
          : null;

  // What this node will actually do, stated before it starts — the honest
  // counterpart to no longer gating Start. Never implies a browser can run
  // something it cannot: with nothing attached the serve lane is simply idle,
  // and the reason (an all-servers/containers tenant vs. a deliberate choice)
  // is named rather than left to be inferred from a disabled button.
  const serveNotice =
    selection !== null
      ? null
      : authWindow
        ? "Still signing in — the list below fills in automatically. You can start the node now either way; it just won't serve a function."
        : targets.length === 0 && excluded.length > 0
          ? "Nothing in this team can run in a browser engine, so this node will serve no function. It still joins the mesh, holds a relay identity, appears on the constellation map, and counts as donated capacity."
          : targets.length === 0
            ? "You have nothing deployed to attach to yet, so this node will serve no function. It still joins the mesh, holds a relay identity, and appears on the constellation map."
            : "This node will serve no function — it joins the mesh, holds a relay identity, and appears on the constellation map. Pick something below to also serve it.";

  return (
    <div className="mx-auto max-w-2xl">
      <div className="mb-6 flex items-center gap-3">
        <RadioTower className="h-6 w-6 text-fg" />
        <h1 className="text-xl font-semibold text-fg">Run a node</h1>
      </div>

      {dataSaverBlocked && (
        <div
          role="alert"
          className="mb-5 flex items-start gap-2 rounded-lg border border-amber-500/30 bg-amber-500/10 p-4 text-sm text-amber-600 dark:text-amber-400"
        >
          <TriangleAlert className="mt-0.5 h-4 w-4 shrink-0" />
          <span>
            Data Saver is on for this connection. Donating capacity would relay other tenants&apos; traffic over
            data you&apos;ve asked your device to conserve, so &quot;Run a node&quot; won&apos;t start (or has been
            stopped) until it&apos;s turned off.
          </span>
        </div>
      )}

      {!supported && (
        <div className="mb-5 flex items-start gap-2 rounded-lg border border-amber-500/30 bg-amber-500/10 p-4 text-sm text-amber-600 dark:text-amber-400">
          <TriangleAlert className="mt-0.5 h-4 w-4 shrink-0" />
          <span>
            This browser supports neither SharedWorker nor the Web Locks fallback that &quot;Run a node&quot;
            needs to keep a node alive across tabs. Try a recent Chrome, Edge, Firefox, or Safari build (a
            private/incognito window can also disable these).
          </span>
        </div>
      )}

      <p className="mb-5 text-sm leading-relaxed text-secondary">
        Donate spare capacity in this browser tab over a direct, end-to-end encrypted peer connection. Your
        browser gets its own low-trust identity — it never joins the platform&apos;s trusted fleet, is never
        counted toward fleet capacity or health, and can be revoked instantly at any time from this page.
        Attaching one of your own deployed functions is optional: without one the node still joins the mesh and
        appears on the constellation map, it just doesn&apos;t serve traffic.
      </p>

      <section className="mb-5 rounded-lg border border-border bg-card p-4">
        <h2 className="mb-1 text-sm font-medium text-fg">What to serve — optional</h2>
        <p className="mb-3 text-xs leading-relaxed text-secondary">
          Attach this node to one of your own deployments, or leave it unattached. Either way it runs.
        </p>
        {(selectionError || replayError) && (
          <div
            role="alert"
            className="mb-3 flex items-start gap-2 rounded-md bg-red-500/10 p-2 text-xs text-red-500"
          >
            <TriangleAlert className="mt-0.5 h-3.5 w-3.5 shrink-0" />
            <span>
              {selectionError ?? replayError} The node still runs unattached — pick another target below to serve
              something.
            </span>
          </div>
        )}
        {serveNotice && (
          <p className="mb-3 rounded-md bg-subtle p-2 text-xs leading-relaxed text-secondary" role="status">
            {serveNotice}
          </p>
        )}
        <TargetPicker
          targets={targets}
          excluded={excluded}
          loading={deploymentsLoading && !deployments}
          fetchError={deploymentsError}
          selected={selection}
          onSelect={choose}
          disabled={status.lifecycle !== "stopped"}
        />
        <div className="mt-3">
          <Field label="Visibility">
            <select
              value={selection === null ? "team" : scope}
              onChange={(e) => chooseScope(e.target.value as "team" | "public")}
              // Visibility decides who may INVOKE what this node serves (and,
              // for a database grant, whether an anonymous donor gets a
              // read-only replica). With nothing attached there is nothing to
              // expose, so the choice is not merely inert — offering "Public"
              // would send a non-admin into a guaranteed FORBIDDEN
              // (`public_scope_forbidden`) for a node that exposes nothing.
              disabled={status.lifecycle !== "stopped" || selection === null}
              className="w-full rounded-md border border-border bg-bg px-2.5 py-1.5 text-sm disabled:opacity-60"
            >
              <option value="team">Team only</option>
              <option value="public">Public (admins &amp; public-node divisions)</option>
            </select>
          </Field>
          {selection === null && (
            <p className="mt-1 text-[11px] text-muted">
              Nothing is attached, so there is nothing for anyone to invoke — this node runs as team-only.
            </p>
          )}
        </div>
      </section>

      <section className="mb-5 rounded-lg border border-border bg-card p-4">
        <h2 className="mb-2 flex items-center gap-1.5 text-sm font-medium text-fg">
          <MapPin className="h-4 w-4" /> Approximate location
        </h2>
        <p className="mb-3 text-xs leading-relaxed text-secondary">
          Optional. If you share it, we round your location to roughly a {Math.round(GEO_QUANT_DEGREES * 111)}km
          grid cell before it ever leaves your device — and the server rounds it again independently — so it places
          a rough dot on the constellation map, never your exact address. This never affects routing, placement, or
          billing. You can revoke this at any time.
        </p>
        {geoDecision === "undecided" && (
          <div className="flex gap-2">
            <button
              onClick={() => decideGeo("denied")}
              className="min-h-11 flex-1 rounded-md border border-border-strong px-3 py-1.5 text-sm font-medium text-fg hover:bg-subtle"
            >
              Don&apos;t share
            </button>
            <button
              ref={geoShareBtnRef}
              onClick={() => decideGeo("granted")}
              className="min-h-11 flex-1 rounded-md bg-fg px-3 py-1.5 text-sm font-medium text-bg hover:opacity-90"
            >
              Share approximate location
            </button>
          </div>
        )}
        {geoDecision !== "undecided" && (
          <div className="flex items-center justify-between text-xs text-secondary">
            <span className="flex items-center gap-1.5">
              <ShieldCheck className="h-3.5 w-3.5" />
              {geoDecision === "granted"
                ? coords
                  ? `Sharing ~(${coords.lat.toFixed(1)}, ${coords.lon.toFixed(1)})${staleSuffix(locatedMs)}`
                  : "Sharing enabled — waiting for a location fix"
                : "Location sharing declined"}
            </span>
            <button
              ref={geoResetBtnRef}
              onClick={() => {
                geoUserActed.current = true;
                localStorage.removeItem(GEO_CONSENT_KEY);
                setGeoDecision("undecided");
                setCoords(null);
                setLocatedMs(null);
              }}
              className="-m-2 min-h-11 min-w-11 p-2 text-link hover:underline"
            >
              Reset
            </button>
          </div>
        )}
        {geoError && (
          <div role="alert" className="mt-2 text-xs text-red-500">
            {geoError}
          </div>
        )}
      </section>

      <section className="rounded-lg border border-border bg-card p-4">
        <div className="mb-3 flex items-center justify-between">
          <h2 className="text-sm font-medium text-fg">Status</h2>
          <span className="text-sm font-medium text-fg" role="status" aria-live="polite">
            {lifecycleLabel(status.lifecycle)}
          </span>
        </div>
        <dl className="grid grid-cols-2 gap-y-1.5 text-xs">
          <dt className="text-muted">Node id</dt>
          <dd className="truncate font-mono text-secondary">{status.endpointId ?? "—"}</dd>
          <dt className="text-muted">Relay</dt>
          <dd className="truncate font-mono text-secondary">{status.relay ?? "—"}</dd>
          <dt className="text-muted">Admission</dt>
          <dd className="text-secondary">{status.admission}</dd>
          {/* browser-node-optional-serve-target: a running node that serves
              nothing is a supported, useful state, so it says so plainly here
              instead of leaving a blank where a function name would be.
              `serving` is derived by the worker from what is actually PINNED,
              never from what was requested. */}
          <dt className="text-muted">Serving</dt>
          <dd className="text-secondary">
            {status.lifecycle === "stopped"
              ? "—"
              : serving && selectedTarget
                ? `${selectedTarget.project} / ${selectedTarget.fn}`
                : serving
                  ? "a function"
                  : "not serving a function — contributing mesh presence and relay capacity"}
          </dd>
          {dbStatus && (
            <>
              <dt className="text-muted">Database</dt>
              <dd className="text-secondary">
                {dbStatus.project ?? "unknown"} · {dbStatus.access === "read_write" ? "read-write" : "read-only"} ·{" "}
                {dbLaneStateLabel(dbStatus)}
                {dbStatus.state !== "error" && dbStatus.state !== "opening" && (
                  <>
                    {" "}· v{dbStatus.siteVersion} across {dbStatus.sites} site{dbStatus.sites === 1 ? "" : "s"},{" "}
                    {dbStatus.peers} sync peer{dbStatus.peers === 1 ? "" : "s"}
                  </>
                )}
                {dbStatus.persisted === false && (
                  <>
                    {" "}·{" "}
                    <span className="text-amber-600 dark:text-amber-400">
                      browser storage not persistent — the local copy can be evicted
                    </span>
                  </>
                )}
              </dd>
            </>
          )}
          {status.tabCount > 1 && (
            <>
              <dt className="text-muted">Tabs</dt>
              <dd className="text-secondary">
                {status.tabCount} tabs open — this node keeps running as long as any of them is
              </dd>
            </>
          )}
        </dl>
        {dbStatus?.error && (
          <div className="mt-3 flex items-start gap-2 rounded-md bg-red-500/10 p-2 text-xs text-red-500" role="alert">
            <TriangleAlert className="mt-0.5 h-3.5 w-3.5 shrink-0" />
            <span>Database sync: {dbStatus.error}</span>
          </div>
        )}
        {status.lifecycle === "suspended" && (
          // Background/suspension honesty (bn-ui-mobile-lifecycle): "suspended"
          // means every tab that could observe this node is currently hidden —
          // the underlying admission is still held (so a backgrounded tab
          // brought back to the foreground resumes instantly), but no traffic is
          // actually being served right now. On mobile especially, a backgrounded
          // browser tab is not a reliable long-running server: the OS can freeze
          // or fully discard it at any time with no further warning from this
          // page, so this never claims "still working in the background" — the
          // one thing this feature must not promise where the platform can't
          // actually provide it.
          <div className="mt-3 rounded-md bg-amber-500/10 p-2 text-xs text-amber-600 dark:text-amber-400" role="status">
            Not currently serving — every tab is in the background. Traffic resumes the instant you bring one back to
            the foreground, but a backgrounded browser tab (especially on mobile) can be frozen or fully closed by
            the OS at any time with no warning here.
          </div>
        )}
        {status.hostAbiStale ? (
          <div className="mt-3 flex items-start gap-2 rounded-md bg-red-500/10 p-2 text-xs text-red-500" role="alert">
            <TriangleAlert className="mt-0.5 h-3.5 w-3.5 shrink-0" />
            <span>
              Your background node is running an outdated version. A page reload alone won&apos;t fix this — close
              every SHADW tab, then reopen this page.
            </span>
          </div>
        ) : status.protocolMismatch === "outdated" ? (
          <div className="mt-3 flex items-start gap-2 rounded-md bg-red-500/10 p-2 text-xs text-red-500" role="alert">
            <TriangleAlert className="mt-0.5 h-3.5 w-3.5 shrink-0" />
            <div className="flex-1">
              <div>This browser bundle is outdated and can no longer connect. Reload to update.</div>
              <button
                onClick={() => window.location.reload()}
                className="mt-2 min-h-11 rounded-md bg-red-500 px-2.5 py-1 text-xs font-medium text-white hover:opacity-90"
              >
                Reload now
              </button>
            </div>
          </div>
        ) : status.protocolMismatch === "server_upgrading" ? (
          <div className="mt-3 flex items-start gap-2 rounded-md bg-amber-500/10 p-2 text-xs text-amber-600 dark:text-amber-400" role="status">
            <TriangleAlert className="mt-0.5 h-3.5 w-3.5 shrink-0" />
            <span>This node hasn&apos;t finished a rolling upgrade yet — retrying automatically, no action needed.</span>
          </div>
        ) : status.sessionStale ? (
          // Auth-renewal input (bn-p2p-reconnect-state): calm, not alarming —
          // this self-heals the moment the platform session cookie refreshes
          // in the background, unlike protocolMismatch === "outdated" above.
          <div className="mt-3 flex items-start gap-2 rounded-md bg-amber-500/10 p-2 text-xs text-amber-600 dark:text-amber-400" role="status">
            <TriangleAlert className="mt-0.5 h-3.5 w-3.5 shrink-0" />
            <span>Waiting for your platform session to refresh before this node can renew — retrying automatically, no action needed.</span>
          </div>
        ) : (
          status.lastError && (
            <div className="mt-3 flex items-start gap-2 rounded-md bg-red-500/10 p-2 text-xs text-red-500" role="alert">
              <TriangleAlert className="mt-0.5 h-3.5 w-3.5 shrink-0" />
              <span>{status.lastError}</span>
            </div>
          )
        )}
        <div className="mt-4 flex gap-2">
          {status.lifecycle === "stopped" ? (
            <button
              onClick={() => {
                // browser-node-optional-serve-target: with no resolvable
                // target this starts a node attached to NOTHING (all three
                // fields empty) rather than doing nothing at all — the server
                // then issues an admission with no artifact capability and no
                // serve route, which is exactly the intended shape.
                start({
                  deployment: selectedTarget?.deployment ?? "",
                  fn: selectedTarget?.fn ?? "",
                  digest: selectedTarget?.policyDigest ?? "",
                  // Scope only means something when something is attached; an
                  // unattached node is always team-only (see the Visibility
                  // field above), so a stale "public" choice can never turn a
                  // bare node into a guaranteed `public_scope_forbidden`.
                  scope: selectedTarget ? scope : "team",
                });
              }}
              disabled={!canStart || !supported}
              className="min-h-11 flex-1 rounded-md bg-fg px-3 py-1.5 text-sm font-medium text-bg hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-50"
            >
              Start
            </button>
          ) : (
            <button
              onClick={onStop}
              className="min-h-11 flex-1 rounded-md border border-border-strong px-3 py-1.5 text-sm font-medium text-fg hover:bg-subtle"
            >
              Stop
            </button>
          )}
        </div>
        {startDisabledReason && (
          <p className="mt-2 text-xs text-secondary" role="status">
            {startDisabledReason}
          </p>
        )}
      </section>
    </div>
  );
}

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <label className="block">
      <span className="mb-1 block text-xs font-medium text-secondary">{label}</span>
      {children}
    </label>
  );
}
