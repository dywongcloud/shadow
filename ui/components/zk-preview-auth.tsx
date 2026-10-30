"use client";

import { useEffect } from "react";
import { useUser } from "@clerk/nextjs";
import { currentTeam } from "@/lib/api";

const ENABLED = process.env.NEXT_PUBLIC_ZKAUTH === "1";

/** EXPERIMENT (`NEXT_PUBLIC_ZKAUTH=1`, a no-op otherwise): enrolls the signed-in member in the CURRENT team's ZK roster through the server route `/api/zk-enroll`, which verifies
 *  their Clerk membership of that team before enrolling on the node; re-enrolls when the user switches teams. */
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
    window.addEventListener("hive-team-changed", enroll);
    return () => window.removeEventListener("hive-team-changed", enroll);
  }, [isSignedIn, user?.id]); // eslint-disable-line react-hooks/exhaustive-deps

  return null;
}
