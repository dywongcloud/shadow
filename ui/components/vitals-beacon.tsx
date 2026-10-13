"use client";

import { useEffect } from "react";

const INP_MIN_EVENT_DURATION_MS = 40;

/** Core Web Vitals beacon for the dashboard itself (Speed Insights): it mirrors the collector the gateway injects into tenant deployments
 *  (`/_vercel/speed-insights/script.js`) but is self-contained, observing FCP/LCP/CLS/INP/TTFB with `PerformanceObserver` and flushing one beacon to
 *  `POST /_vercel/speed-insights/vitals` (recorded per tenant) when the page is hidden. It runs on EVERY device (samples carry the real viewport/UA, so the
 *  Desktop/Mobile toggle reflects real mobile performance), has no DOM, fails silent and never blocks paint. */
export function VitalsBeacon() {
  useEffect(() => {
    if (typeof window === "undefined" || typeof PerformanceObserver === "undefined") return;

    const vitals: Record<string, number> = {};
    const observers: PerformanceObserver[] = [];
    const obs = (type: string, cb: (entries: PerformanceEntryList) => void, extra?: PerformanceObserverInit) => {
      try {
        const po = new PerformanceObserver((list) => cb(list.getEntries()));
        po.observe({ type, buffered: true, ...(extra || {}) } as PerformanceObserverInit);
        observers.push(po);
      } catch {}
    };

    obs("paint", (entries) => {
      for (const e of entries) if (e.name === "first-contentful-paint") vitals.FCP = e.startTime;
    });
    obs("largest-contentful-paint", (entries) => {
      if (entries.length) vitals.LCP = entries[entries.length - 1].startTime;
    });
    let cls = 0;
    obs("layout-shift", (entries) => {
      for (const e of entries) {
        const ls = e as PerformanceEntry & { hadRecentInput?: boolean; value?: number };
        if (!ls.hadRecentInput) cls += ls.value || 0;
      }
      vitals.CLS = cls;
    });
    obs(
      "event",
      (entries) => {
        for (const e of entries) vitals.INP = Math.max(vitals.INP || 0, e.duration);
      },
      { durationThreshold: INP_MIN_EVENT_DURATION_MS } as PerformanceObserverInit,
    );
    try {
      const nav = performance.getEntriesByType("navigation")[0] as PerformanceNavigationTiming | undefined;
      if (nav) vitals.TTFB = nav.responseStart;
    } catch {}

    let sent = false;
    const flush = () => {
      if (sent) return;
      sent = true;
      try {
        const body = JSON.stringify({ href: location.href, vitals });
        navigator.sendBeacon?.("/_vercel/speed-insights/vitals", body);
      } catch {}
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
