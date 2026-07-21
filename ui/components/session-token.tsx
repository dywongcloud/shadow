"use client";

import { useEffect } from "react";
import { mintSessionToken, ensureSessionMinted } from "@/lib/api";

/**
 * Keeps a fresh httpOnly `hive_jwt` cookie (minted by `/api/token`) so the
 * dashboard's same-origin `/cloud` calls authenticate at the admin ingress when
 * the platform enforces JWT. The cookie carries the CURRENTLY-selected team (the
 * backend derives the tenant from it), so every mint passes `currentTeam()`.
 * Re-mints on mount and every ~50 min (token TTL is 1h). Team SWITCHES re-mint
 * synchronously inside `switchTeam()` BEFORE pollers re-fetch, so we don't also
 * re-mint on `hive-team-changed` here (that would double-fire + race). Dev-safe:
 * when the backend isn't enforcing, `/api/token` is a no-op.
 *
 * The INITIAL mount call routes through `ensureSessionMinted()` — the same
 * shared, idempotent gate every `fetchWithTimeout` call awaits before its first
 * request — so this effect and every OTHER component's own data-fetch mount
 * effect (which may run first) are guaranteed to share the exact same in-flight
 * mint rather than racing to fire two separate ones.
 */
export function SessionToken() {
  useEffect(() => {
    let cancelled = false;
    void ensureSessionMinted();
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
