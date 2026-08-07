"use client";

import { useEffect, useMemo, useRef, useState } from "react";
import { MapPin, RadioTower, ShieldCheck, TriangleAlert } from "lucide-react";
import { useRunNode } from "@/lib/use-run-node";
import {
  lifecycleLabel,
  dbLaneStateLabel,
  type RunNodeStatus,
  type DbLaneStatus,
  type FunctionLaneStatus,
  type ServeMode,
} from "@/lib/run-node-status";
import { clearPresence, GEO_CONSENT_KEY, GEO_COORDS_KEY } from "@/lib/run-node-client";
import { usePoll, sessionMintStatus, type Deployment } from "@/lib/api";
import { excludedDeployments, resolveTarget, targetsFromDeployments } from "@/lib/run-node-targets";
import { TargetPicker, type PickerSelection, type TargetSelection } from "./target-picker";
// browser-run-node-target-picker: the persisted selection is the STABLE
// deployment+function pair only — never a digest. The digest is re-derived
// from fresh descriptor metadata on every render/start/replay, so a redeploy
// that rotates the policy digest under the same name keeps working and a
// deleted/ineligible target fails visibly and clears instead of replaying.
// browser-node-optional-serve-target: an EMPTY deployment is the persisted
// form of "nothing pinned", which is a real state a node runs on — not an
// absent one.
// browser-auto-serve-eligible-set: the persisted shape carries `mode`
// ("auto" | "target").
// bn-picker-drop-capacity-only: `mode: "none"` was the third value and is
// RETIRED — the node it described (serves no function, replicates no database)
// is not a shape the platform produces any more, now that
// `browser_artifacts::eligible_for_tenant` derives the serve set unasked and
// `browser_db::auto_db_deployment_for_tenant` assigns a replica instead of
// refusing when a tenant has several browser_db projects. Any persisted record
// with an empty deployment — written by an older build as "none", or by this
// one as "auto" — restores as "auto". Reading it back as a mode that can no
// longer be chosen would leave the page unable to render its own selection.
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
  // browser-auto-serve-eligible-set (additive worker fields): WHICH functions
  // are actually pinned right now, and which authorized artifact the worker
  // could not load. Both describe the live runtime, not the selection — a node
  // in automatic mode is carrying whatever the fleet last authorized, which the
  // page cannot derive on its own.
  const widened = status as RunNodeStatus & {
    serveMode?: ServeMode;
    functions?: FunctionLaneStatus | null;
  };
  const serveMode: ServeMode = widened.serveMode ?? "none";
  // Whether the worker REPORTED a serve lane at all. The `?? "none"` above is a
  // safe default for every comparison, but it collapses two different worlds:
  // a session that genuinely asked for the retired capacity-only mode, and a
  // background worker so old it has no `serveMode` field to report. Telling the
  // second "you started in serve-nothing mode" is a plain falsehood about a
  // choice the person never made, so the idle copy below tells them apart.
  const serveModeReported = widened.serveMode !== undefined;
  const servingList = widened.functions?.serving ?? [];
  const pinnedCount = widened.functions?.pinned.length ?? 0;
  const functionFailures = widened.functions?.failed ?? [];
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
  // Default "auto" (browser-auto-serve-eligible-set): a node serves whatever
  // this team has, with no choice required. SSR-safe — the same value renders
  // on the server, and the persisted restore happens in the mount effect below.
  const [selection, setSelection] = useState<PickerSelection>("auto");
  const explicit: TargetSelection | null = typeof selection === "object" ? selection : null;
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
    explicit && deployments && !authWindow
      ? resolveTarget(deployments, explicit.deployment, explicit.fn)
      : null;
  const selectedTarget = resolved && resolved.ok ? resolved.target : null;
  // Revalidation failure, derived per render: while the list is still loading
  // (including the team-switch window, where usePoll drops data to null)
  // nothing is judged; the moment real data lands, a selection that no longer
  // resolves produces its reason immediately — no effect round trip.
  const revalidationFailure = explicit && resolved && !resolved.ok ? resolved.reason : null;
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
      // A pin that no longer resolves falls back to AUTOMATIC, not to serving
      // nothing: the deployment it named is gone, but the rest of the team's
      // eligible work still exists and this node can still carry it.
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
    } catch {
      /* storage unavailable — the selection just won't survive a reload */
    }
  }

  // Every mode is a real, supported way to run a node; the choice (and the
  // scope beside it) survives a reload exactly like a real target does.
  function choose(sel: PickerSelection) {
    setSelection(sel);
    setClearedError(null);
    persistChoice(sel, scope);
  }

  function chooseScope(next: "team" | "public") {
    setScope(next);
    persistChoice(selection, next);
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
          // An empty deployment is a MODE, never a target named "". The only
          // unpinned mode left is "auto", so a record persisting the retired
          // "none" restores as "auto" too (see TARGET_KEY's note) — the node it
          // named would contribute nothing, which is exactly what this build
          // stopped offering.
          setSelection(parsed.deployment ? { deployment: parsed.deployment, fn: parsed.fn } : "auto");
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
  // something it cannot: with nothing eligible the serve lane is simply idle,
  // and the reason (an all-servers/containers tenant, a session still landing)
  // is named rather than left to be inferred from a disabled button.
  const eligibleCount = targets.filter((t) => t.kind === "function").length;
  // Whether this tenant has ANY ready deployment with a `browser_db` block —
  // read straight off the deployment list rather than off `targets`, which
  // surfaces a database target only for deployments that have no
  // browser-eligible function. The fleet picks WHICH replica this endpoint
  // holds (`auto_db_deployment_for_tenant` rendezvous-hashes by endpoint id);
  // this only decides whether replication is mentioned at all.
  const hasDatabase = (deployments ?? []).some((d) => d.state === "ready" && !!d.browser_db);
  // bn-picker-drop-capacity-only: no "serve nothing" branch, because there is
  // no such selection any more. Every unpinned node is automatic, and automatic
  // with an empty eligible set is a STATE (nothing eligible yet), not a choice
  // to be useless — so the copy says what happens next instead of describing a
  // node that contributes nothing.
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
              // Visibility decides who may INVOKE what this node serves (and,
              // for a database grant, whether an anonymous donor gets a
              // read-only replica). It used to be force-disabled for the
              // retired "serve nothing" selection, where offering "Public"
              // would have sent a non-admin into a guaranteed FORBIDDEN
              // (`public_scope_forbidden`) for a node that exposed nothing.
              // Every remaining selection exposes something — "public +
              // automatic" means every browser-eligible function this team has,
              // invokable by anyone — so lifecycle is the only thing that
              // disables it.
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
          {/* browser-node-optional-serve-target: a running node with an idle
              serve lane is a supported state, so it says WHY plainly here
              instead of leaving a blank where a function name would be.
              `serving` is derived by the worker from what is actually PINNED,
              never from what was requested — and the idle causes
              (database-only pin, a pin still loading, a retired capacity-only
              session, a worker too old to report its lane, nothing eligible
              yet, and nothing handed over yet) are genuinely different things
              to tell someone, so none is collapsed into the others. */}
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
                    ? // bn-picker-drop-capacity-only: unreachable from THIS
                      // build's picker. Two distinct pasts land here and the
                      // remedy is the same, but the cause is not: a session an
                      // older page really did start capacity-only, versus a
                      // background worker predating the field entirely (the
                      // `?? "none"` default above), for which the
                      // outdated-worker banner further down is the real story.
                      // Naming the fix beats leaving a running node
                      // permanently idle with no explanation — but not at the
                      // cost of blaming someone for a mode they never picked.
                      serveModeReported
                      ? "capacity only — started in the retired serve-nothing mode; Stop and Start it to carry this team's work"
                      : "serve lane unreported — this node's background worker predates automatic serving; Stop and Start it to carry this team's work"
                    : // Two different causes, and this page can tell them apart from
                    // the deployment list it already polls: the team genuinely has
                    // nothing eligible, versus it does and the fleet hasn't handed
                    // any over yet (a mid-rollout node, a lease still settling).
                    // Never assert the first when the second is what's happening.
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
              {/* Honest per-artifact reporting: one artifact this browser could
                  not verify/fetch never blanks the ones it IS serving, and is
                  never silently swallowed — the next renewal retries it. */}
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
                // browser-auto-serve-eligible-set: with no PIN this starts an
                // automatic node — empty deployment/fn/digest plus
                // `serveMode: "auto"`, and the server derives the whole
                // eligible set itself from this tenant's Ready deployments.
                // bn-picker-drop-capacity-only: `serveMode` is now ALWAYS
                // "auto". With a pin the worker overrides it to "pinned" from
                // the non-empty deployment anyway (run-node-worker.js's start
                // handler), so this is the one honest value: nothing the page
                // can send asks the fleet for a node that carries nothing.
                // That also covers the one-render window where a persisted pin
                // has failed revalidation and the fallback-to-auto effect has
                // not committed yet — it starts automatic, not empty.
                start({
                  deployment: selectedTarget?.deployment ?? "",
                  fn: selectedTarget?.fn ?? "",
                  digest: selectedTarget?.policyDigest ?? "",
                  serveMode: "auto",
                  // Every startable selection now exposes something, so the
                  // chosen scope always applies (see the Visibility field).
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
