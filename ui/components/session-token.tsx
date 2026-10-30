"use client";

import { useEffect } from "react";
import { ensureSessionMinted } from "@/lib/api";
import { startSessionKeeper } from "@/lib/session-keeper";

/** Keeps the httpOnly `hive_jwt` cookie (minted by `/api/token`, carrying the current team) fresh so same-origin `/cloud` calls authenticate: one shared mint on mount
 *  (`ensureSessionMinted`, the gate every API call awaits), then the session keeper (`ui/lib/session-keeper.ts`) re-mints at about 70% of the lifetime each mint
 *  reports, once per browser rather than once per tab, and never from a hidden tab. Team switches re-mint inside `switchTeam()` before pollers re-fetch, so this
 *  component deliberately ignores `hive-team-changed` (it would double-fire and race). */
export function SessionToken() {
  useEffect(() => {
    void ensureSessionMinted();
    return startSessionKeeper();
  }, []);
  return null;
}
