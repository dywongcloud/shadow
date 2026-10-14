"use client";

import { useEffect, useRef, useState } from "react";
import { usePathname, useSearchParams } from "next/navigation";
import { useTheme } from "next-themes";
import { currentTeam, ensureSessionMinted } from "@/lib/api";

// Seamless same-origin iframe embed of the upstream @workflow/web console (served at /wfc): the
// iframe isolates its Tailwind v4 from the dashboard's v3, its `name` carries
// "wfc|<team>|<project>" to the patched rpc client, and the app-wide session keeper
// (components/session-token.tsx) keeps hive_jwt fresh for it: this frame never mints on its own.

const URL_SYNC_INTERVAL_MS = 300;

const WF_THEME_KEY = "workflow-theme";

/** Push the platform's resolved theme into the console: storage key + a direct root-class
 *  reconcile on the live iframe document. */
function syncConsoleTheme(frame: HTMLIFrameElement | null, resolved: string | undefined) {
  const theme = resolved === "light" ? "light" : "dark";
  try {
    localStorage.setItem(WF_THEME_KEY, theme);
  } catch {}
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
  const lastMirroredRef = useRef<string | null>(null);
  const [frameName, setFrameName] = useState<string | null>(null);
  const [src] = useState(() => {
    const qs = syncUrl ? searchParams?.toString() : "";
    return `/wfc${initialPath ? `/${initialPath}` : ""}${qs ? `?${qs}` : ""}`;
  });

  useEffect(() => {
    let alive = true;
    syncConsoleTheme(null, resolvedTheme);
    const warm = document.createElement("link");
    warm.rel = "prefetch";
    warm.as = "document";
    warm.href = src;
    document.head.appendChild(warm);
    void ensureSessionMinted().finally(() => {
      if (alive) setFrameName(`wfc|${currentTeam()}|${project ?? ""}`);
    });
    return () => {
      alive = false;
      warm.remove();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [project]);

  useEffect(() => {
    syncConsoleTheme(frameRef.current, resolvedTheme);
    const frame = frameRef.current;
    if (!frame) return;
    const onLoad = () => syncConsoleTheme(frame, resolvedTheme);
    frame.addEventListener("load", onLoad);
    return () => frame.removeEventListener("load", onLoad);
  }, [resolvedTheme, frameName]);

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

  useEffect(() => {
    if (!syncUrl) return;
    const iv = setInterval(() => {
      const win = frameRef.current?.contentWindow;
      if (!win) return;
      let inner: string;
      try {
        inner = win.location.pathname + win.location.search;
      } catch {
        return;
      }
      if (!inner.startsWith("/wfc")) return;
      const mapped = "/workflows" + inner.slice("/wfc".length);
      const current = window.location.pathname + window.location.search;
      if (mapped !== current && mapped !== lastMirroredRef.current) {
        lastMirroredRef.current = mapped;
        window.history.replaceState({ ...window.history.state, _N: true }, "", mapped);
      }
    }, URL_SYNC_INTERVAL_MS);
    return () => clearInterval(iv);
  }, [syncUrl]);

  useEffect(() => {
    if (!syncUrl) return;
    const expected = "/wfc" + (pathname?.startsWith("/workflows") ? pathname.slice("/workflows".length) : "");
    if (("/workflows" + expected.slice("/wfc".length)) === lastMirroredRef.current) return;
    const win = frameRef.current?.contentWindow;
    if (!win) return;
    try {
      const inner = win.location.pathname;
      if (inner && inner !== expected && inner.startsWith("/wfc")) {
        win.location.replace(expected);
      }
    } catch {}
  }, [pathname, syncUrl]);

  return (
    <div
      ref={wrapRef}
      className={
        bleed
          ? "-mx-4 -my-8 sm:-mx-6 overflow-hidden bg-transparent"
          : "overflow-hidden rounded-xl border border-border bg-transparent"
      }
    >
      {frameName ? (
        <iframe
          ref={frameRef}
          src={src}
          name={frameName}
          title="Workflow console"
          className="block w-full border-0 bg-transparent"
          style={{ height, background: "transparent" }}
          allowTransparency
          loading="lazy"
        />
      ) : (
        <div className="animate-pulse" style={{ height }} />
      )}
    </div>
  );
}
