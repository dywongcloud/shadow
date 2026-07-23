"use client";

import { useEffect, useRef, useState } from "react";
import { usePathname, useSearchParams } from "next/navigation";
import { useTheme } from "next-themes";
import { currentTeam, ensureSessionMinted, mintSessionToken } from "@/lib/api";

// ---------------------------------------------------------------------------
// Seamless embed of the LITERAL upstream @workflow/web console (served at
// /wfc by app/wf-console/[[...slug]]/route.ts) inside the shadw dashboard's
// own chrome. The iframe is the isolation boundary that keeps the console's
// Tailwind v4 stylesheet from colliding with the dashboard's Tailwind v3 —
// visually it is borderless, full-height, and URL-synced, so the console
// reads as a native page under the platform navbar.
//
// Scope handoff: the iframe's `name` attribute carries "wfc|<team>|<project>"
// into the embedded window (window.name survives SPA navigation inside the
// frame). The patched console rpc client (scripts/patch-wf-console.mjs,
// __hive_rpc_guard) reads it and appends hiveTeam/hiveProject to every
// /wfc/api/rpc call; the wf-console route threads them into the World proxy.
//
// Session lifetime: this parent page runs clerk-js (Clerk session stays
// fresh) and re-mints the httpOnly hive_jwt cookie on mount + every few
// minutes + on tab re-focus, so the embedded console — which runs no
// clerk-js of its own — never goes stale. The rpc guard's own
// re-mint-and-retry is the second line of defense.
// ---------------------------------------------------------------------------

const REMINT_INTERVAL_MS = 4 * 60_000;
const URL_SYNC_INTERVAL_MS = 300;

// The embedded console runs its OWN next-themes instance on this storage key
// (same origin — localStorage is shared). Writing the platform's RESOLVED
// theme here before the iframe mounts makes the console's SSR theme
// bootstrap paint correctly on first paint; writing it again on every
// platform toggle fires a same-origin `storage` event that the console's
// next-themes cross-tab listener applies live.
const WF_THEME_KEY = "workflow-theme";

/** Push the platform's resolved theme into the console: storage key (first
 *  paint + next-themes storage listener) AND a direct root-class reconcile on
 *  the live iframe document (covers the pre-hydration window). */
function syncConsoleTheme(frame: HTMLIFrameElement | null, resolved: string | undefined) {
  const theme = resolved === "light" ? "light" : "dark";
  try {
    localStorage.setItem(WF_THEME_KEY, theme);
  } catch {
    /* storage unavailable — the direct reconcile below still applies */
  }
  const doc = frame?.contentDocument;
  if (doc?.documentElement) {
    const root = doc.documentElement;
    root.classList.remove(theme === "dark" ? "light" : "dark");
    root.classList.add(theme);
    root.style.colorScheme = theme;
  }
}

export function WfConsoleFrame({
  project,
  initialPath = "",
  syncUrl = false,
  bleed = false,
}: {
  /** Scope the console to one project (project tab); omit for the global view. */
  project?: string;
  /** Console-internal path to open initially, e.g. "run/wrun_x" (no /wfc). */
  initialPath?: string;
  /** Mirror console-internal navigation onto the parent URL (/workflows/*). */
  syncUrl?: boolean;
  /** Reclaim the layout <main> padding (standalone /workflows page only). */
  bleed?: boolean;
}) {
  const frameRef = useRef<HTMLIFrameElement>(null);
  const wrapRef = useRef<HTMLDivElement>(null);
  const [height, setHeight] = useState<number>(640);
  const searchParams = useSearchParams();
  const pathname = usePathname();
  const { resolvedTheme } = useTheme();
  // The parent URL we last wrote from an iframe navigation — distinguishes a
  // real outer navigation (Next router) from our own replaceState mirroring.
  const lastMirroredRef = useRef<string | null>(null);
  // CLIENT-ONLY, set after the session mint below: a useState initializer
  // runs during SSR and its value survives hydration, which froze the name
  // as "wfc||" (no team) in production — the scope must come from the real
  // browser-side identity.
  const [frameName, setFrameName] = useState<string | null>(null);
  // The initial src is computed ONCE — subsequent navigation happens inside
  // the frame (or via contentWindow.location.replace below), never by
  // re-setting src (which would reload the whole console app).
  // Parent search params pass through ONLY on the URL-synced standalone page
  // (/workflows?tab=hooks → console tab=hooks, legacy-link compatible); a
  // tabbed embed must NOT leak its host page's own ?tab= into the console.
  const [src] = useState(() => {
    const qs = syncUrl ? searchParams?.toString() : "";
    return `/wfc${initialPath ? `/${initialPath}` : ""}${qs ? `?${qs}` : ""}`;
  });

  // Keep the hive_jwt session perpetually fresh while the console is open.
  // The iframe is NOT mounted until the first mint resolves (frameName gates
  // the render below): the console's very first rpc otherwise races the mint
  // and receives the backend's cookie-less 200-EMPTY response, settling the
  // runs table on a false "No workflow runs found" (the session-mint-race
  // class, witnessed live on production).
  useEffect(() => {
    let alive = true;
    // Theme BEFORE mount: the console's SSR bootstrap reads the storage key
    // synchronously, so the first paint matches the platform theme (no
    // wrong-theme flash). The frame isn't mounted yet — frame arg null.
    syncConsoleTheme(null, resolvedTheme);
    void ensureSessionMinted().finally(() => {
      if (alive) setFrameName(`wfc|${currentTeam()}|${project ?? ""}`);
    });
    const tick = () => void mintSessionToken();
    const iv = setInterval(tick, REMINT_INTERVAL_MS);
    const onVis = () => {
      if (document.visibilityState === "visible") tick();
    };
    document.addEventListener("visibilitychange", onVis);
    return () => {
      alive = false;
      clearInterval(iv);
      document.removeEventListener("visibilitychange", onVis);
    };
  }, [project]);

  // Live theme sync: every platform toggle re-writes the console's storage
  // key (its next-themes storage listener applies it) and reconciles the
  // live iframe root class directly. Also re-applied on iframe load so a
  // frame that navigated internally starts in the right theme.
  useEffect(() => {
    syncConsoleTheme(frameRef.current, resolvedTheme);
    const frame = frameRef.current;
    if (!frame) return;
    const onLoad = () => syncConsoleTheme(frame, resolvedTheme);
    frame.addEventListener("load", onLoad);
    return () => frame.removeEventListener("load", onLoad);
  }, [resolvedTheme, frameName]);

  // Fill the viewport below the navbar (measured, not hardcoded — survives
  // chrome-height changes) and track window resizes.
  useEffect(() => {
    const compute = () => {
      const el = wrapRef.current;
      if (!el) return;
      const docTop = el.getBoundingClientRect().top + window.scrollY;
      setHeight(Math.max(480, window.innerHeight - docTop));
    };
    compute();
    window.addEventListener("resize", compute);
    return () => window.removeEventListener("resize", compute);
  }, []);

  // iframe -> parent URL mirroring (same-origin read of the frame location).
  useEffect(() => {
    if (!syncUrl) return;
    const iv = setInterval(() => {
      const win = frameRef.current?.contentWindow;
      if (!win) return;
      let inner: string;
      try {
        inner = win.location.pathname + win.location.search;
      } catch {
        return; // transiently unreadable (mid-navigation)
      }
      if (!inner.startsWith("/wfc")) return;
      const mapped = "/workflows" + inner.slice("/wfc".length);
      const current = window.location.pathname + window.location.search;
      if (mapped !== current && mapped !== lastMirroredRef.current) {
        lastMirroredRef.current = mapped;
        // Raw replaceState (not the Next router): no page re-render, the
        // iframe keeps its state; deep links stay copy-pastable.
        window.history.replaceState(window.history.state, "", mapped);
      }
    }, URL_SYNC_INTERVAL_MS);
    return () => clearInterval(iv);
  }, [syncUrl]);

  // parent -> iframe: a REAL outer navigation to a different /workflows path
  // (e.g. a pasted deep link resolving through Next, or browser navigation
  // that re-rendered the page) points the existing frame at the new path.
  useEffect(() => {
    if (!syncUrl) return;
    const expected = "/wfc" + (pathname?.startsWith("/workflows") ? pathname.slice("/workflows".length) : "");
    if (("/workflows" + expected.slice("/wfc".length)) === lastMirroredRef.current) return; // our own mirror write
    const win = frameRef.current?.contentWindow;
    if (!win) return;
    try {
      const inner = win.location.pathname;
      if (inner && inner !== expected && inner.startsWith("/wfc")) {
        win.location.replace(expected);
      }
    } catch {
      /* frame not ready yet — initial src already carries the right path */
    }
  }, [pathname, syncUrl]);

  return (
    <div
      ref={wrapRef}
      className={
        bleed
          ? "-mx-4 -my-8 sm:-mx-6 overflow-hidden bg-bg"
          : "overflow-hidden rounded-xl border border-border bg-bg"
      }
    >
      {frameName ? (
        <iframe
          ref={frameRef}
          src={src}
          name={frameName}
          title="Workflow console"
          className="block w-full border-0"
          style={{ height }}
        />
      ) : (
        <div className="animate-pulse" style={{ height }} />
      )}
    </div>
  );
}
