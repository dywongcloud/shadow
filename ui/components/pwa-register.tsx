"use client";

import { useEffect } from "react";

// Registers the service worker so shadw is installable + works offline.
// No-op in browsers without service-worker support.
export function PwaRegister() {
  useEffect(() => {
    if (typeof window === "undefined" || !("serviceWorker" in navigator)) return;

    // If a SW already controls this page, a later controllerchange means an
    // UPDATED service worker took over (e.g. after a deploy) — reload ONCE so the
    // page runs the fresh build instead of stale cached code. Skip on the very
    // first install (no prior controller) to avoid an unnecessary reload.
    const hadController = !!navigator.serviceWorker.controller;
    let refreshing = false;
    const onControllerChange = () => {
      if (refreshing || !hadController) return;
      refreshing = true;
      window.location.reload();
    };
    navigator.serviceWorker.addEventListener("controllerchange", onControllerChange);

    const register = () => {
      navigator.serviceWorker
        .register("/sw.js")
        .then((reg) => reg.update().catch(() => {})) // proactively check for a newer SW
        .catch(() => {
          /* registration is best-effort */
        });
    };
    if (document.readyState === "complete") register();
    else window.addEventListener("load", register, { once: true });

    return () => navigator.serviceWorker.removeEventListener("controllerchange", onControllerChange);
  }, []);
  return null;
}
