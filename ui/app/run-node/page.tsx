"use client";

import { useEffect, useRef, useState } from "react";
import { MapPin, RadioTower, ShieldCheck, TriangleAlert } from "lucide-react";
import { useRunNode } from "@/lib/use-run-node";
import { lifecycleLabel } from "@/lib/run-node-status";
import { upsertPresence, clearPresence, type PresenceState } from "@/lib/run-node-client";

const GEO_CONSENT_KEY = "hive_run_node_geo_consent"; // "granted" | "denied"
const GEO_QUANT_DEGREES = 0.5; // matches the server-side floor — defense in depth, not the only gate
const PRESENCE_REFRESH_MS = 45_000; // well inside the backend's 90s presence TTL
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

function presenceState(lifecycle: string): PresenceState | null {
  if (lifecycle === "starting") return "starting";
  if (lifecycle === "online") return "online";
  if (lifecycle === "degraded" || lifecycle === "error") return "degraded";
  if (lifecycle === "suspended") return "suspended";
  return null; // "stopped" — no presence record while stopped
}

export default function RunNodePage() {
  const { status, supported, start, stop, setGeoConsent, dataSaverBlocked } = useRunNode();
  const [deployment, setDeployment] = useState("");
  const [fn, setFn] = useState("");
  const [digest, setDigest] = useState("");
  const [scope, setScope] = useState<"team" | "public">("team");
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
  const refreshTimer = useRef<ReturnType<typeof setInterval> | null>(null);
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

  // Push a fresh presence record whenever there's something live to publish,
  // and keep it alive on an interval while the node is up — the presence TTL
  // is short (90s) so a satellite disappears promptly if this page (or the
  // node) actually stops updating it.
  useEffect(() => {
    const state = presenceState(status.lifecycle);
    if (!status.endpointId || !state) {
      if (refreshTimer.current) {
        clearInterval(refreshTimer.current);
        refreshTimer.current = null;
      }
      return;
    }
    const publish = () => {
      upsertPresence({
        endpoint_id: status.endpointId!,
        lat: geoDecision === "granted" ? coords?.lat ?? null : null,
        lon: geoDecision === "granted" ? coords?.lon ?? null : null,
        relay_hint: status.relay ?? "",
        state,
      }).catch(() => {});
    };
    publish();
    refreshTimer.current = setInterval(publish, PRESENCE_REFRESH_MS);
    return () => {
      if (refreshTimer.current) clearInterval(refreshTimer.current);
      refreshTimer.current = null;
    };
  }, [status.endpointId, status.lifecycle, status.relay, geoDecision, coords?.lat, coords?.lon]);

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

  const canStart =
    deployment.trim() && fn.trim() && digest.trim() && status.lifecycle === "stopped" && !dataSaverBlocked;

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
        <div className="grid gap-3 sm:grid-cols-2">
          <Field label="Deployment ID">
            <input
              value={deployment}
              onChange={(e) => setDeployment(e.target.value)}
              disabled={status.lifecycle !== "stopped"}
              placeholder="dpl-abc123"
              className="w-full rounded-md border border-border bg-bg px-2.5 py-1.5 text-sm disabled:opacity-60"
            />
          </Field>
          <Field label="Function name">
            <input
              value={fn}
              onChange={(e) => setFn(e.target.value)}
              disabled={status.lifecycle !== "stopped"}
              placeholder="web"
              className="w-full rounded-md border border-border bg-bg px-2.5 py-1.5 text-sm disabled:opacity-60"
            />
          </Field>
          <Field label="Function digest (BLAKE3 hex)">
            <input
              value={digest}
              onChange={(e) => setDigest(e.target.value)}
              disabled={status.lifecycle !== "stopped"}
              placeholder="from the deployment's function manifest"
              className="w-full rounded-md border border-border bg-bg px-2.5 py-1.5 text-sm font-mono text-xs disabled:opacity-60"
            />
          </Field>
          <Field label="Visibility">
            <select
              value={scope}
              onChange={(e) => setScope(e.target.value as "team" | "public")}
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
          {status.tabCount > 1 && (
            <>
              <dt className="text-muted">Tabs</dt>
              <dd className="text-secondary">
                {status.tabCount} tabs open — this node keeps running as long as any of them is
              </dd>
            </>
          )}
        </dl>
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
              onClick={() => start({ deployment, fn, digest, scope })}
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
