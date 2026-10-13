"use client";

import { useEffect } from "react";
import { mintSessionToken, ensureSessionMinted } from "@/lib/api";

const REMINT_INTERVAL_MS = 50 * 60_000;

/** Keeps the httpOnly `hive_jwt` cookie (minted by `/api/token`, carrying the current team) fresh so same-origin `/cloud` calls authenticate: one shared mint on mount
 *  (`ensureSessionMinted`, the gate every API call awaits) and a re-mint every 50 min of the 1 h TTL. Team switches re-mint inside `switchTeam()` before pollers
 *  re-fetch, so this component deliberately ignores `hive-team-changed` (it would double-fire and race). */
export function SessionToken() {
  useEffect(() => {
    let cancelled = false;
    void ensureSessionMinted();
    const id = setInterval(() => {
      if (!cancelled) mintSessionToken();
    }, REMINT_INTERVAL_MS);
    return () => {
      cancelled = true;
      clearInterval(id);
    };
  }, []);
  return null;
}
