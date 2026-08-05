"use client";

import { useEffect, useMemo, useRef, useState } from "react";
import { MapPin, RadioTower, ShieldCheck, TriangleAlert } from "lucide-react";
import { useRunNode } from "@/lib/use-run-node";
import { lifecycleLabel, type RunNodeStatus } from "@/lib/run-node-status";
import { clearPresence, GEO_CONSENT_KEY, GEO_COORDS_KEY } from "@/lib/run-node-client";
import { usePoll, type Deployment } from "@/lib/api";
import { resolveTarget, targetsFromDeployments } from "@/lib/run-node-targets";
import { TargetPicker, type TargetSelection } from "./target-picker";
// browser-run-node-target-picker: the persisted selection is the STABLE
// deployment+function pair only — never a digest. The digest is re-derived
// from fresh descriptor metadata on every render/start/replay, so a redeploy
// that rotates the policy digest under the same name keeps working and a
// deleted/ineligible target fails visibly and clears instead of replaying.
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


// bn-run-node-db-sync-wiring: the worker's additive db-lane status field.
// Present only while the admitted project's capability carries a
// server-derived `db` block (a fluid.json top-level browser_db opt-in) — the
// typed mirror in lib/run-node-status.ts is owned by another row, so this
// page widens the type locally, the same way the field rides the wire.
type DbLaneStatus = {
  project: string | null;
  dbFile: string | null;
  access: "read_write" | "read_only" | null;
  state: "opening" | "idle" | "syncing" | "sealed" | "error";
  persisted: boolean | "unknown" | null;
  peers: number;
  lastSyncMs: number | null;
  sites: number;
  siteVersion: number;
  error: string | null;
};

function dbLaneStateLabel(db: DbLaneStatus): string {
  switch (db.state) {
    case "opening":
      return "opening local replica…";
    case "syncing":
      return "syncing…";
    case "sealed":
      return "sealed — resumes on re-admission";
    case "error":
      return "error";
    case "idle":
      return db.lastSyncMs ? `last synced ${Math.max(0, Math.round((Date.now() - db.lastSyncMs) / 1000))}s ago` : "idle";
  }
}

export default function RunNodePage() {
  const { status, supported, start, stop, setGeoConsent, dataSaverBlocked, replayError } = useRunNode();
  const dbStatus = (status as RunNodeStatus & { db?: DbLaneStatus | null }).db ?? null;
  // Eligible serve targets come from the same authenticated /deployments list
  // the rest of the dashboard polls — served locally with leader/owner
  // fallback server-side (admin_ingress + the /cloud proxy), so this page
  // never needs a node-specific endpoint. Re-polled live so rotation,
  // deletion, and team switches are revalidated against current metadata.
  const { data: deployments, error: deploymentsError, loading: deploymentsLoading } =
    usePoll<Deployment[]>("/deployments", 5000);
  const targets = useMemo(() => targetsFromDeployments(deployments ?? []), [deployments]);
  const excludedCount = useMemo(
    () => (deployments ?? []).filter((d) => d.state !== "ready" || (d.browser_functions ?? []).length === 0).length,
    [deployments],
  );
  // Persisted selection restored lazily (localStorage is unavailable during
  // SSR — same pattern as geoDecision below). Scope rides along so a restored
  // selection also restores the visibility it was last used with.
  const [selection, setSelection] = useState<(TargetSelection & { scope?: "team" | "public" }) | null>(() => {
    if (typeof window === "undefined") return null;
    try {
      const raw = localStorage.getItem(TARGET_KEY);
      if (!raw) return null;
      const parsed = JSON.parse(raw);
      if (!parsed || typeof parsed.deployment !== "string" || typeof parsed.fn !== "string") return null;
      return parsed;
    } catch {
      return null;
    }
  });
  const [scope, setScope] = useState<"team" | "public">(() => {
    if (typeof window === "undefined") return "team";
    try {
      const raw = localStorage.getItem(TARGET_KEY);
      const parsed = raw ? JSON.parse(raw) : null;
      return parsed && (parsed.scope === "public" || parsed.scope === "team") ? parsed.scope : "team";
    } catch {
      return "team";
    }
  });
  // The digest handed to Start is resolved from the CURRENT snapshot on every
  // render — never from the persisted selection — so a target rotated between
  // page load and click starts the current artifact, not a stale digest.
  const resolved = selection && deployments ? resolveTarget(deployments, selection.deployment, selection.fn) : null;
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

  function choose(sel: TargetSelection) {
    setSelection(sel);
    setClearedError(null);
    try {
      localStorage.setItem(TARGET_KEY, JSON.stringify({ ...sel, scope }));
    } catch {
      /* storage unavailable — the selection just won't survive a reload */
    }
  }

  function chooseScope(next: "team" | "public") {
    setScope(next);
    if (selection) {
      try {
        localStorage.setItem(TARGET_KEY, JSON.stringify({ ...selection, scope: next }));
      } catch {
        /* ignore */
      }
    }
  }

  // Restored via a lazy initializer (runs once, during this component's own
  // first render) rather than an effect — localStorage is unavailable during
  // SSR, so the guard falls back to "undecided" there and the real value
  // lands on the client's own first render, with no extra effect-driven
  // re-render in between.
  const [geoDecision, setGeoDecision] = useState<"undecided" | "granted" | "denied">(() => {
    if (typeof window === "undefined") return "undecided";
    const saved = localStorage.getItem(GEO_CONSENT_KEY);
    return saved === "granted" || saved === "denied" ? saved : "undecided";
  });
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
  const geoDecisionMounted = useRef(false);
  useEffect(() => {
    if (!geoDecisionMounted.current) {
      geoDecisionMounted.current = true;
      return;
    }
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

  const canStart = selectedTarget !== null && status.lifecycle === "stopped" && !dataSaverBlocked;

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
            This browser doesn&apos;t support SharedWorker, which &quot;Run a node&quot; requires. Try a recent
            Chrome, Edge, or Firefox desktop build.
          </span>
        </div>
      )}

      <p className="mb-5 text-sm leading-relaxed text-secondary">
        Donate spare capacity in this browser tab to serve one of your own deployed functions over a direct,
        end-to-end encrypted peer connection. Your browser gets its own low-trust identity — it never joins the
        platform&apos;s trusted fleet, is never counted toward fleet capacity or health, and can be revoked
        instantly at any time from this page.
      </p>

      <section className="mb-5 rounded-lg border border-border bg-card p-4">
        <h2 className="mb-3 text-sm font-medium text-fg">What to serve</h2>
        {(selectionError || replayError) && (
          <div
            role="alert"
            className="mb-3 flex items-start gap-2 rounded-md bg-red-500/10 p-2 text-xs text-red-500"
          >
            <TriangleAlert className="mt-0.5 h-3.5 w-3.5 shrink-0" />
            <span>
              {selectionError ?? replayError} Pick another target below.
            </span>
          </div>
        )}
        <TargetPicker
          targets={targets}
          excludedCount={excludedCount}
          loading={deploymentsLoading && !deployments}
          fetchError={deploymentsError}
          selected={selection}
          onSelect={choose}
          disabled={status.lifecycle !== "stopped"}
        />
        <div className="mt-3">
          <Field label="Visibility">
            <select
              value={scope}
              onChange={(e) => chooseScope(e.target.value as "team" | "public")}
              disabled={status.lifecycle !== "stopped"}
              className="w-full rounded-md border border-border bg-bg px-2.5 py-1.5 text-sm disabled:opacity-60"
            >
              <option value="team">Team only</option>
              <option value="public">Public (requires owner/admin)</option>
            </select>
          </Field>
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
                if (selectedTarget) {
                  start({
                    deployment: selectedTarget.deployment,
                    fn: selectedTarget.fn,
                    digest: selectedTarget.policyDigest,
                    scope,
                  });
                }
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
