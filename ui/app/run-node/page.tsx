"use client";

import { useEffect, useMemo, useRef, useState } from "react";
import { MapPin, RadioTower, ShieldCheck, TriangleAlert } from "lucide-react";
import { useRunNode } from "@/lib/use-run-node";
import {
  lifecycleLabel,
  dbLaneStateLabel,
  syncFsStateLabel,
  isolationLabel,
  type RunNodeStatus,
  type DbLaneStatus,
  type SyncFsLaneStatus,
  type FunctionLaneStatus,
  type ServeMode,
} from "@/lib/run-node-status";
import { clearPresence, GEO_CONSENT_KEY, GEO_COORDS_KEY } from "@/lib/run-node-client";
import { usePoll, sessionMintStatus, type Deployment } from "@/lib/api";
import { excludedDeployments, resolveTarget, targetsFromDeployments } from "@/lib/run-node-targets";
import { TargetPicker, type PickerSelection, type TargetSelection } from "./target-picker";
const TARGET_KEY = "hive_run_node_target";
const GEO_QUANT_DEGREES = 0.5;
const GEO_REFRESH_MS = 8 * 60_000;

function quantize(v: number): number {
  return Math.round(v / GEO_QUANT_DEGREES) * GEO_QUANT_DEGREES;
}

function staleSuffix(locatedMs: number | null): string {
  if (locatedMs === null) return "";
  const ageMin = Math.floor((Date.now() - locatedMs) / 60_000);
  return ageMin >= 15 ? ` · fix is ${ageMin}m old` : "";
}


export default function RunNodePage() {
  const { status, supported, start, stop, setGeoConsent, dataSaverBlocked, replayError } = useRunNode();
  const dbStatus = (status as RunNodeStatus & { db?: DbLaneStatus | null }).db ?? null;
  const syncFsStatus = (status as RunNodeStatus & { syncFs?: SyncFsLaneStatus | null }).syncFs ?? null;
  const serving = (status as RunNodeStatus & { serving?: boolean }).serving === true;
  const widened = status as RunNodeStatus & {
    serveMode?: ServeMode;
    functions?: FunctionLaneStatus | null;
  };
  const serveMode: ServeMode = widened.serveMode ?? "none";
  const serveModeReported = widened.serveMode !== undefined;
  const servingList = widened.functions?.serving ?? [];
  const pinnedCount = widened.functions?.pinned.length ?? 0;
  const functionFailures = widened.functions?.failed ?? [];
  const { data: deployments, error: deploymentsError, loading: deploymentsLoading } =
    usePoll<Deployment[]>("/deployments", 5000);
  const targets = useMemo(() => targetsFromDeployments(deployments ?? []), [deployments]);
  const excluded = useMemo(() => excludedDeployments(deployments ?? []), [deployments]);
  const [selection, setSelection] = useState<PickerSelection>("auto");
  const explicit: TargetSelection | null = typeof selection === "object" ? selection : null;
  const [scope, setScope] = useState<"team" | "public">("team");
  const mintState = sessionMintStatus().state;
  const authWindow =
    deployments !== null &&
    deployments.length === 0 &&
    (mintState === "backoff" || mintState === "failed");
  const resolved =
    explicit && deployments && !authWindow
      ? resolveTarget(deployments, explicit.deployment, explicit.fn)
      : null;
  const selectedTarget = resolved && resolved.ok ? resolved.target : null;
  const revalidationFailure = explicit && resolved && !resolved.ok ? resolved.reason : null;
  const [clearedError, setClearedError] = useState<string | null>(null);
  const selectionError = revalidationFailure ?? clearedError;

  useEffect(() => {
    if (!revalidationFailure) return;
    queueMicrotask(() => {
      try {
        localStorage.removeItem(TARGET_KEY);
      } catch {}
      setClearedError(revalidationFailure);
      setSelection("auto");
    });
  }, [revalidationFailure]);

  function persistChoice(sel: PickerSelection, nextScope: "team" | "public") {
    try {
      localStorage.setItem(
        TARGET_KEY,
        JSON.stringify({
          deployment: typeof sel === "object" ? sel.deployment : "",
          fn: typeof sel === "object" ? sel.fn : "",
          mode: typeof sel === "object" ? "target" : "auto",
          scope: nextScope,
        }),
      );
    } catch {}
  }

  function choose(sel: PickerSelection) {
    setSelection(sel);
    setClearedError(null);
    persistChoice(sel, scope);
  }

  function chooseScope(next: "team" | "public") {
    setScope(next);
    persistChoice(selection, next);
  }

  const [geoDecision, setGeoDecision] = useState<"undecided" | "granted" | "denied">("undecided");

  useEffect(() => {
    try {
      const raw = localStorage.getItem(TARGET_KEY);
      if (raw) {
        const parsed = JSON.parse(raw);
        if (parsed && typeof parsed.deployment === "string" && typeof parsed.fn === "string") {
          // eslint-disable-next-line react-hooks/set-state-in-effect -- deliberate post-mount hydration to avoid an SSR/CSR mismatch (#418, see comment above); a lazy initializer would read localStorage during the server render and diverge from the client's first paint
          setSelection(parsed.deployment ? { deployment: parsed.deployment, fn: parsed.fn } : "auto");
          if (parsed.scope === "public" || parsed.scope === "team") setScope(parsed.scope);
        }
      }
    } catch {}
    try {
      const savedGeo = localStorage.getItem(GEO_CONSENT_KEY);
      if (savedGeo === "granted" || savedGeo === "denied") setGeoDecision(savedGeo);
    } catch {}
  }, []);
  const [geoError, setGeoError] = useState<string | null>(null);
  const [coords, setCoords] = useState<{ lat: number; lon: number } | null>(null);
  const [locatedMs, setLocatedMs] = useState<number | null>(null);
  const geoTimer = useRef<ReturnType<typeof setInterval> | null>(null);
  const geoResetBtnRef = useRef<HTMLButtonElement>(null);
  const geoShareBtnRef = useRef<HTMLButtonElement>(null);
  const geoUserActed = useRef(false);
  useEffect(() => {
    if (!geoUserActed.current) return;
    if (geoDecision === "undecided") geoShareBtnRef.current?.focus();
    else geoResetBtnRef.current?.focus();
  }, [geoDecision]);

  const requestLocation = useRef(() => {
    if (typeof navigator === "undefined" || !navigator.geolocation) {
      setGeoError("This browser has no Geolocation API — location will show as unknown.");
      return;
    }
    navigator.geolocation.getCurrentPosition(
      (pos) => {
        setGeoError(null);
        setCoords({ lat: quantize(pos.coords.latitude), lon: quantize(pos.coords.longitude) });
        setLocatedMs(Date.now());
      },
      (err) => {
        if (err.code === err.PERMISSION_DENIED) {
          localStorage.setItem(GEO_CONSENT_KEY, "denied");
          setGeoDecision("denied");
          setCoords(null);
          setLocatedMs(null);
          setGeoError("Location permission was denied in your browser — location sharing turned off. Re-enable it in your browser's site settings to share again.");
          return;
        }
        setGeoError(
          err.code === err.POSITION_UNAVAILABLE
            ? "Location temporarily unavailable — will keep retrying."
            : err.message || "Location request failed or was denied.",
        );
      },
      { enableHighAccuracy: false, maximumAge: 10 * 60_000, timeout: 15_000 },
    );
  });

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

  useEffect(() => {
    if (typeof window === "undefined") return;
    if (geoDecision === "granted" && coords) {
      localStorage.setItem(GEO_COORDS_KEY, JSON.stringify({ lat: coords.lat, lon: coords.lon }));
    } else if (geoDecision !== "granted") {
      localStorage.removeItem(GEO_COORDS_KEY);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps -- intentionally narrowed to primitives; see comment above
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
  }

  async function onStop() {
    stop();
    if (status.endpointId) {
      await clearPresence(status.endpointId).catch(() => {});
    }
  }

  const canStart = status.lifecycle === "stopped" && !dataSaverBlocked;

  const startDisabledReason =
    status.lifecycle !== "stopped" || canStart
      ? null
      : !supported
        ? null
        : dataSaverBlocked
          ? null
          : null;

  const eligibleCount = targets.filter((t) => t.kind === "function").length;
  const hasDatabase = (deployments ?? []).some((d) => d.state === "ready" && !!d.browser_db);
  const serveNotice =
    selection !== "auto"
      ? null
      : authWindow
        ? "Still signing in — this node starts either way and picks up whatever this team has once the session lands, with no restart."
        : eligibleCount === 0 && hasDatabase
          ? "No function in this team can run in a browser engine yet, so this node starts by replicating the database the fleet assigns it — and starts serving functions on its own the moment one becomes eligible, without restarting."
          : eligibleCount === 0 && excluded.length > 0
            ? "Nothing in this team can run in a browser engine yet, so this node starts with an idle serve lane. It still joins the mesh, holds a relay identity, appears on the constellation map, and counts as donated capacity — and it starts serving on its own the moment something becomes eligible."
            : eligibleCount === 0
              ? "You have nothing browser-eligible deployed yet, so this node starts with an idle serve lane — and picks up your first eligible function automatically, without restarting."
              : `This node will serve every browser-eligible function in this team (${eligibleCount} right now)${
                  hasDatabase ? ", plus a replica of one of this team's browser databases" : ""
                }, and picks up new deployments automatically without restarting.`;

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
        counted toward fleet capacity or health, and can be revoked instantly at any time from this page. Work is
        dispatched to it automatically: the fleet decides which of your own deployed functions this tab can run,
        and re-decides on every lease renewal, so nothing here has to be picked by hand.
      </p>

      <section className="mb-5 rounded-lg border border-border bg-card p-4">
        <h2 className="mb-1 text-sm font-medium text-fg">What this node serves</h2>
        <p className="mb-3 text-xs leading-relaxed text-secondary">
          By default this node carries whatever your team has that can run in a browser, plus whichever database
          the fleet assigns it — decided server-side, refreshed every minute, so a function you deploy later
          starts being served here without restarting anything. Narrow it to a single deployment if you want to.
          Either way it contributes: a node whose team has nothing eligible yet joins the mesh now and starts
          carrying work the moment something is.
        </p>
        {(selectionError || replayError) && (
          <div
            role="alert"
            className="mb-3 flex items-start gap-2 rounded-md bg-red-500/10 p-2 text-xs text-red-500"
          >
            <TriangleAlert className="mt-0.5 h-3.5 w-3.5 shrink-0" />
            <span>
              {selectionError ?? replayError} The node still runs — it fell back to serving whatever else this
              team has, and you can pin a different target below.
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
          hasDatabase={hasDatabase}
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
              value={scope}
              onChange={(e) => chooseScope(e.target.value as "team" | "public")}
              disabled={status.lifecycle !== "stopped"}
              className="w-full rounded-md border border-border bg-bg px-2.5 py-1.5 text-sm disabled:opacity-60"
            >
              <option value="team">Team only</option>
              <option value="public">Public (admins &amp; public-node divisions)</option>
            </select>
          </Field>
          {selection === "auto" && scope === "public" && (
            <p className="mt-1 text-[11px] text-muted">
              Public + automatic: every browser-eligible function this team has — including ones deployed later —
              becomes invokable by anyone through this node.
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
          <dt className="text-muted">Serving</dt>
          <dd className="text-secondary">
            {status.lifecycle === "stopped"
              ? "—"
              : !serving
                ? serveMode === "pinned"
                  ? explicit && explicit.fn === ""
                    ? "no function — this node is pinned to a database replica only"
                    : "pinned function not loaded yet — retrying on the next renewal"
                  : serveMode === "none"
                    ?
                      serveModeReported
                      ? "capacity only — started in the retired serve-nothing mode; Stop and Start it to carry this team's work"
                      : "serve lane unreported — this node's background worker predates automatic serving; Stop and Start it to carry this team's work"
                    :
                    eligibleCount === 0
                    ? "nothing yet — no browser-eligible function exists in this team; this node starts serving on its own when one does"
                    : "nothing yet — waiting for the fleet to hand work over, retrying automatically"
                : servingList.length > 0
                  ? servingList
                      .map((f) => (f.project || f.function ? `${f.project || "?"} / ${f.function || "?"}` : f.digest.slice(0, 8)))
                      .join(", ")
                  : selectedTarget
                    ? `${selectedTarget.project} / ${selectedTarget.fn}`
                    : `${pinnedCount} function${pinnedCount === 1 ? "" : "s"}`}
            {serving && serveMode === "auto" && (
              <span className="text-muted"> · automatic, refreshed every minute</span>
            )}
          </dd>
          {functionFailures.length > 0 && (
            <>
              <dt className="text-muted">Not loaded</dt>
              <dd className="text-amber-600 dark:text-amber-400">
                {functionFailures.map((f) => f.error).join(" · ")}
              </dd>
            </>
          )}
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
          {syncFsStatus && (
            <>
              <dt className="text-muted">Sync fs</dt>
              <dd className={syncFsStatus.state === "unavailable" ? "text-amber-600 dark:text-amber-400" : "text-secondary"}>
                {syncFsStateLabel(syncFsStatus)}
                {syncFsStatus.state === "ready" && syncFsStatus.prefix && (
                  <span className="text-muted"> · {syncFsStatus.prefix}</span>
                )}
                {syncFsStatus.state === "unavailable" && syncFsStatus.error && (
                  <span className="text-muted"> · {syncFsStatus.error}</span>
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
        {status.isolation && !status.isolation.syncBridge && status.lifecycle !== "stopped" ? (
          <div className="mt-3 flex items-start gap-2 rounded-md bg-sky-500/10 p-2 text-xs text-sky-700 dark:text-sky-300" role="status">
            <ShieldCheck className="mt-0.5 h-3.5 w-3.5 shrink-0" />
            <span>{isolationLabel(status.isolation)}</span>
          </div>
        ) : null}
        <div className="mt-4 flex gap-2">
          {status.lifecycle === "stopped" ? (
            <button
              onClick={() => {
                start({
                  deployment: selectedTarget?.deployment ?? "",
                  fn: selectedTarget?.fn ?? "",
                  digest: selectedTarget?.policyDigest ?? "",
                  serveMode: "auto",
                  scope,
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
