"use client";

import { useEffect } from "react";

/**
 * Core Web Vitals beacon for the dashboard itself (Speed Insights).
 *
 * Mirrors the collector the gateway injects into tenant deployments
 * (`/_vercel/speed-insights/script.js`) but self-contained so it works on the
 * dashboard host regardless of proxy path: it observes FCP / LCP / CLS / INP /
 * TTFB with `PerformanceObserver` and flushes a single beacon to the platform's
 * vitals sink (`POST /_vercel/speed-insights/vitals`, recorded per-tenant +
 * surfaced in the Observability → Speed Insights view) when the page is hidden.
 *
 * Runs on EVERY device — the samples carry the real user's viewport/UA so the
 * Speed Insights Desktop/Mobile toggle reflects actual mobile performance. Zero
 * layout impact (no DOM), fails silent, and never blocks paint (effect after
 * hydration). Registered once in the root layout so it covers all pages.
 */
export function VitalsBeacon() {
  useEffect(() => {
    // SSR / unsupported-browser guard.
    if (typeof window === "undefined" || typeof PerformanceObserver === "undefined") return;

    const vitals: Record<string, number> = {};
    const observers: PerformanceObserver[] = [];
    const obs = (type: string, cb: (entries: PerformanceEntryList) => void, extra?: PerformanceObserverInit) => {
      try {
        const po = new PerformanceObserver((list) => cb(list.getEntries()));
        po.observe({ type, buffered: true, ...(extra || {}) } as PerformanceObserverInit);
        observers.push(po);
      } catch {
        /* entry type unsupported on this browser — skip that vital */
      }
    };

    // FCP — first contentful paint.
    obs("paint", (entries) => {
      for (const e of entries) if (e.name === "first-contentful-paint") vitals.FCP = e.startTime;
    });
    // LCP — largest contentful paint (last reported wins).
    obs("largest-contentful-paint", (entries) => {
      if (entries.length) vitals.LCP = entries[entries.length - 1].startTime;
    });
    // CLS — cumulative layout shift (excluding recent-input shifts).
    let cls = 0;
    obs("layout-shift", (entries) => {
      for (const e of entries) {
        const ls = e as PerformanceEntry & { hadRecentInput?: boolean; value?: number };
        if (!ls.hadRecentInput) cls += ls.value || 0;
      }
      vitals.CLS = cls;
    });
    // INP — worst interaction latency (approx via event durations ≥ 40ms).
    obs(
      "event",
      (entries) => {
        for (const e of entries) vitals.INP = Math.max(vitals.INP || 0, e.duration);
      },
      { durationThreshold: 40 } as PerformanceObserverInit,
    );
    // TTFB — from navigation timing.
    try {
      const nav = performance.getEntriesByType("navigation")[0] as PerformanceNavigationTiming | undefined;
      if (nav) vitals.TTFB = nav.responseStart;
    } catch {
      /* navigation timing unavailable */
    }

    let sent = false;
    const flush = () => {
      if (sent) return;
      sent = true;
      try {
        const body = JSON.stringify({ href: location.href, vitals });
        navigator.sendBeacon?.("/_vercel/speed-insights/vitals", body);
      } catch {
        /* beacon best-effort */
      }
    };
    const onHidden = () => {
      if (document.visibilityState === "hidden") flush();
    };
    document.addEventListener("visibilitychange", onHidden);
    window.addEventListener("pagehide", flush);

    return () => {
      document.removeEventListener("visibilitychange", onHidden);
      window.removeEventListener("pagehide", flush);
      observers.forEach((o) => o.disconnect());
    };
  }, []);

  return null;
}
