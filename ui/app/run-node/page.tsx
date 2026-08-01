"use client";

import { useEffect, useRef, useState } from "react";
import { MapPin, RadioTower, ShieldCheck, TriangleAlert } from "lucide-react";
import { useRunNode } from "@/lib/use-run-node";
import { lifecycleLabel } from "@/lib/run-node-status";
import { upsertPresence, clearPresence, type PresenceState } from "@/lib/run-node-client";

const GEO_CONSENT_KEY = "hive_run_node_geo_consent"; // "granted" | "denied"
const GEO_QUANT_DEGREES = 0.5; // matches the server-side floor — defense in depth, not the only gate
const PRESENCE_REFRESH_MS = 45_000; // well inside the backend's 90s presence TTL

function quantize(v: number): number {
  return Math.round(v / GEO_QUANT_DEGREES) * GEO_QUANT_DEGREES;
}

function presenceState(lifecycle: string): PresenceState | null {
  if (lifecycle === "starting") return "starting";
  if (lifecycle === "online") return "online";
  if (lifecycle === "degraded" || lifecycle === "error") return "degraded";
  if (lifecycle === "suspended") return "suspended";
  return null; // "stopped" — no presence record while stopped
}

export default function RunNodePage() {
  const { status, supported, start, stop, setGeoConsent } = useRunNode();
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
  const refreshTimer = useRef<ReturnType<typeof setInterval> | null>(null);

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
      return;
    }
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
      },
      (err) => setGeoError(err.message || "Location request failed or was denied."),
      { enableHighAccuracy: false, maximumAge: 10 * 60_000, timeout: 15_000 },
    );
  }

  async function onStop() {
    stop();
    if (status.endpointId) {
      await clearPresence(status.endpointId).catch(() => {});
    }
  }

  const canStart = deployment.trim() && fn.trim() && digest.trim() && status.lifecycle === "stopped";

  return (
    <div className="mx-auto max-w-2xl">
      <div className="mb-6 flex items-center gap-3">
        <RadioTower className="h-6 w-6 text-fg" />
        <h1 className="text-xl font-semibold text-fg">Run a node</h1>
      </div>

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
              className="flex-1 rounded-md border border-border-strong px-3 py-1.5 text-sm font-medium text-fg hover:bg-subtle"
            >
              Don&apos;t share
            </button>
            <button
              onClick={() => decideGeo("granted")}
              className="flex-1 rounded-md bg-fg px-3 py-1.5 text-sm font-medium text-bg hover:opacity-90"
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
                  ? `Sharing ~(${coords.lat.toFixed(1)}, ${coords.lon.toFixed(1)})`
                  : "Sharing enabled — waiting for a location fix"
                : "Location sharing declined"}
            </span>
            <button
              onClick={() => {
                localStorage.removeItem(GEO_CONSENT_KEY);
                setGeoDecision("undecided");
                setCoords(null);
              }}
              className="text-link hover:underline"
            >
              Reset
            </button>
          </div>
        )}
        {geoError && <div className="mt-2 text-xs text-red-500">{geoError}</div>}
      </section>

      <section className="rounded-lg border border-border bg-card p-4">
        <div className="mb-3 flex items-center justify-between">
          <h2 className="text-sm font-medium text-fg">Status</h2>
          <span className="text-sm font-medium text-fg">{lifecycleLabel(status.lifecycle)}</span>
        </div>
        <dl className="grid grid-cols-2 gap-y-1.5 text-xs">
          <dt className="text-muted">Node id</dt>
          <dd className="truncate font-mono text-secondary">{status.endpointId ?? "—"}</dd>
          <dt className="text-muted">Relay</dt>
          <dd className="truncate font-mono text-secondary">{status.relay ?? "—"}</dd>
          <dt className="text-muted">Admission</dt>
          <dd className="text-secondary">{status.admission}</dd>
        </dl>
        {status.lastError && (
          <div className="mt-3 flex items-start gap-2 rounded-md bg-red-500/10 p-2 text-xs text-red-500">
            <TriangleAlert className="mt-0.5 h-3.5 w-3.5 shrink-0" />
            <span>{status.lastError}</span>
          </div>
        )}
        <div className="mt-4 flex gap-2">
          {status.lifecycle === "stopped" ? (
            <button
              onClick={() => start({ deployment, fn, digest, scope })}
              disabled={!canStart || !supported}
              className="flex-1 rounded-md bg-fg px-3 py-1.5 text-sm font-medium text-bg hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-50"
            >
              Start
            </button>
          ) : (
            <button
              onClick={onStop}
              className="flex-1 rounded-md border border-border-strong px-3 py-1.5 text-sm font-medium text-fg hover:bg-subtle"
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
