"use client";

import { useEffect } from "react";

// Registers the service worker so shadw is installable + works offline.
// No-op in browsers without service-worker support.
export function PwaRegister() {
  useEffect(() => {
    if (typeof window === "undefined" || !("serviceWorker" in navigator) || !navigator.serviceWorker) return;

    // If a SW already controls this page, a later controllerchange means an
    // UPDATED service worker took over (e.g. after a deploy) — reload ONCE so the
    // page runs the fresh build instead of stale cached code. Skip on the very
    // first install (no prior controller) to avoid an unnecessary reload.
    //
    // The reload is LATCHED via sessionStorage: at most 2 SW-driven reloads per
    // minute per tab. The sw.js no longer calls skipWaiting, so takeover while
    // a tab is open should not happen at all — this latch is the backstop that
    // turns any residual repeated-takeover pathology (the mobile flicker class)
    // into a page that simply keeps running the older build until the next
    // natural navigation, instead of reloading forever.
    const hadController = !!navigator.serviceWorker.controller;
    let refreshing = false;
    const onControllerChange = () => {
      if (refreshing || !hadController) return;
      refreshing = true;
      try {
        const raw = sessionStorage.getItem("shadw-sw-reloads");
        const now = Date.now();
        const hits = (raw ? (JSON.parse(raw) as number[]) : []).filter((t) => now - t < 60_000);
        if (hits.length >= 2) return; // latched: ride the current build
        hits.push(now);
        sessionStorage.setItem("shadw-sw-reloads", JSON.stringify(hits));
      } catch {
        /* storage unavailable (private mode) — reload unlatched, worst case matches old behavior */
      }
      window.location.reload();
    };
    navigator.serviceWorker.addEventListener("controllerchange", onControllerChange);

    // No per-mount reg.update(): forcing an update check on EVERY page load
    // turned each navigation into a chance to discover a new SW mid-session,
    // accelerating reload churn during deploys. The browser's own registration
    // update checks (navigation heuristics + 24h cap) are enough.
    const register = () => {
      navigator.serviceWorker.register("/sw.js").catch(() => {
        /* registration is best-effort */
      });
    };
    if (document.readyState === "complete") register();
    else window.addEventListener("load", register, { once: true });

    return () => navigator.serviceWorker.removeEventListener("controllerchange", onControllerChange);
  }, []);

  // Remove Clerk's development-instance floating badge (the white rounded box in
  // the bottom-right corner). CSS handles the common cases; this JS sweep is the
  // robust catch-all across Clerk's shifting class names — hide any FIXED-position
  // anchor pointing at clerk.com (never a legit in-page link on our surfaces).
  useEffect(() => {
    if (typeof window === "undefined") return;
    const sweep = () => {
      document.querySelectorAll<HTMLElement>('a[href*="clerk.com"], .cl-badge, [data-localization-key="badge__development"]').forEach((el) => {
        const fixed = getComputedStyle(el).position === "fixed" || el.className.toString().includes("badge");
        if (fixed) el.style.setProperty("display", "none", "important");
      });
    };
    sweep();
    const obs = new MutationObserver(sweep);
    obs.observe(document.body, { childList: true, subtree: true });
    const t = setTimeout(() => obs.disconnect(), 15000); // badge injects early; stop watching after
    return () => {
      clearTimeout(t);
      obs.disconnect();
    };
  }, []);
  return null;
}
