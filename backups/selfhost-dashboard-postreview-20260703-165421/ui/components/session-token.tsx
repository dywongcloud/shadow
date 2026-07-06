"use client";

import { useEffect } from "react";

/**
 * Keeps a fresh httpOnly `hive_jwt` cookie (minted by `/api/token`) so the
 * dashboard's same-origin `/cloud` calls authenticate at the admin ingress when
 * the platform enforces JWT. Re-mints on mount, on team/account switch, and every
 * ~50 min (token TTL is 1h). Fire-and-forget and dev-safe: when the backend isn't
 * enforcing, `/api/token` is a no-op and nothing changes.
 */
export function SessionToken() {
  useEffect(() => {
    let cancelled = false;
    const mint = () => {
      fetch("/api/token", { method: "POST" }).catch(() => {});
    };
    mint();
    const onTeam = () => mint();
    window.addEventListener("hive-team-changed", onTeam);
    const id = setInterval(() => {
      if (!cancelled) mint();
    }, 50 * 60_000);
    return () => {
      cancelled = true;
      window.removeEventListener("hive-team-changed", onTeam);
      clearInterval(id);
    };
  }, []);
  return null;
}
