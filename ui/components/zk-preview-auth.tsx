"use client";

import { useEffect } from "react";
import { useUser } from "@clerk/nextjs";
import { currentTeam } from "@/lib/api";

// EXPERIMENT (NEXT_PUBLIC_ZKAUTH=1): in the background, enroll the signed-in
// member into the CURRENT team's ZK roster — but via the server route
// `/api/zk-enroll`, which verifies (server-side, via Clerk) that the user really
// belongs to that team before enrolling on the node. The old version called the
// node's open `/v1/zkauth/register` directly, which let anyone self-enroll into
// any team (preview-access bypass). No-op unless the flag is set.
const ENABLED = process.env.NEXT_PUBLIC_ZKAUTH === "1";

export function ZkPreviewAuth() {
  const { user, isSignedIn } = useUser();

  useEffect(() => {
    if (!ENABLED || !isSignedIn || !user) return;
    const enroll = () => {
      fetch("/api/zk-enroll", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ team: currentTeam() }),
      }).catch(() => {});
    };
    enroll();
    // Re-enroll when the user switches teams so they're in each team's roster.
    window.addEventListener("hive-team-changed", enroll);
    return () => window.removeEventListener("hive-team-changed", enroll);
  }, [isSignedIn, user?.id]); // eslint-disable-line react-hooks/exhaustive-deps

  return null;
}
