"use client";

import { useEffect } from "react";
import { useUser } from "@clerk/nextjs";
import { apiSend } from "@/lib/api";

// EXPERIMENT (NEXT_PUBLIC_ZKAUTH=1): automatically enroll the signed-in member's
// anonymous-membership key into the current team's roster, in the background.
// No manual "publish roster" step. No-op unless the flag is set; failures (e.g.
// the backend feature being off → 404) are ignored.
const ENABLED = process.env.NEXT_PUBLIC_ZKAUTH === "1";

export function ZkPreviewAuth() {
  const { user, isSignedIn } = useUser();

  useEffect(() => {
    if (!ENABLED || !isSignedIn || !user) return;
    const register = () => {
      // apiSend scopes to the current team via the x-hive-team header.
      apiSend("POST", "/v1/zkauth/register", { user_id: user.id }).catch(() => {});
    };
    register();
    // Re-enroll when the user switches teams so they're in each team's roster.
    window.addEventListener("hive-team-changed", register);
    return () => window.removeEventListener("hive-team-changed", register);
  }, [isSignedIn, user?.id]); // eslint-disable-line react-hooks/exhaustive-deps

  return null;
}
