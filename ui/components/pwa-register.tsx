"use client";

import { useEffect } from "react";

const SW_RELOAD_WINDOW_MS = 60_000;
const MAX_SW_RELOADS_PER_WINDOW = 2;
const CLERK_BADGE_WATCH_MS = 15000;

// Registers the service worker so shadw is installable and works offline; a no-op without service-worker support.
export function PwaRegister() {
  useEffect(() => {
    if (typeof window === "undefined" || !("serviceWorker" in navigator) || !navigator.serviceWorker) return;

    const hadController = !!navigator.serviceWorker.controller;
    let refreshing = false;
    const onControllerChange = () => {
      if (refreshing || !hadController) return;
      refreshing = true;
      try {
        const raw = sessionStorage.getItem("shadw-sw-reloads");
        const now = Date.now();
        const hits = (raw ? (JSON.parse(raw) as number[]) : []).filter((t) => now - t < SW_RELOAD_WINDOW_MS);
        if (hits.length >= MAX_SW_RELOADS_PER_WINDOW) return;
        hits.push(now);
        sessionStorage.setItem("shadw-sw-reloads", JSON.stringify(hits));
      } catch {}
      window.location.reload();
    };
    navigator.serviceWorker.addEventListener("controllerchange", onControllerChange);

    const register = () => {
      navigator.serviceWorker.register("/sw.js").catch(() => {});
    };
    if (document.readyState === "complete") register();
    else window.addEventListener("load", register, { once: true });

    return () => navigator.serviceWorker.removeEventListener("controllerchange", onControllerChange);
  }, []);

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
    const t = setTimeout(() => obs.disconnect(), CLERK_BADGE_WATCH_MS);
    return () => {
      clearTimeout(t);
      obs.disconnect();
    };
  }, []);
  return null;
}
