"use client";

import { useEffect } from "react";
import { mintSessionToken } from "@/lib/api";

/**
 * Keeps a fresh httpOnly `hive_jwt` cookie (minted by `/api/token`) so the
 * dashboard's same-origin `/cloud` calls authenticate at the admin ingress when
 * the platform enforces JWT. The cookie carries the CURRENTLY-selected team (the
 * backend derives the tenant from it), so every mint passes `currentTeam()`.
 * Re-mints on mount and every ~50 min (token TTL is 1h). Team SWITCHES re-mint
 * synchronously inside `switchTeam()` BEFORE pollers re-fetch, so we don't also
 * re-mint on `hive-team-changed` here (that would double-fire + race). Dev-safe:
 * when the backend isn't enforcing, `/api/token` is a no-op.
 */
export function SessionToken() {
  useEffect(() => {
    let cancelled = false;
    mintSessionToken();
    const id = setInterval(() => {
      if (!cancelled) mintSessionToken();
    }, 50 * 60_000);
    return () => {
      cancelled = true;
      clearInterval(id);
    };
  }, []);
  return null;
}
