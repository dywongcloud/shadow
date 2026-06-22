"use client";

import { useEffect, useRef, useState } from "react";
import { useUser } from "@clerk/nextjs";

// Cross-domain "one-and-done" preview unlock. The deployment gate (on a different
// origin, e.g. *.deployment.…ngrok.pizza) bounces a protected-preview navigation
// here with ?host=&project=&team=&next=. Using the signed-in dashboard session we
// (1) ensure the member is enrolled in the team's ZK roster, (2) mint a membership
// proof, then (3) hand off to `https://<host>/_shadw/zk?…` which verifies the
// proof, drops the access cookie ON THE DEPLOYMENT DOMAIN, and lands on `next`.
const BASE = "/cloud";
const clerkEnabled = !!process.env.NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY;

export default function PreviewUnlockPage() {
  // useUser may only be called when the ClerkProvider is mounted (clerkEnabled).
  return clerkEnabled ? <ClerkUnlock /> : <Unlock userId="local-dev" signedIn loaded />;
}

function ClerkUnlock() {
  const { user, isSignedIn, isLoaded } = useUser();
  return <Unlock userId={user?.id ?? ""} signedIn={!!isSignedIn} loaded={isLoaded} />;
}

function Unlock({ userId, signedIn, loaded }: { userId: string; signedIn: boolean; loaded: boolean }) {
  const [status, setStatus] = useState("Verifying your access…");
  const [error, setError] = useState("");
  const ran = useRef(false);

  useEffect(() => {
    if (!loaded || ran.current) return;
    const p = new URLSearchParams(window.location.search);
    const host = p.get("host") || "";
    const project = p.get("project") || "";
    const team = p.get("team") || "personal";
    const next = p.get("next") || "/";
    if (!host || !project) {
      setError("This unlock link is missing deployment details.");
      return;
    }
    // Not signed in (and auth is on) → sign in first, then come back here.
    if (clerkEnabled && !signedIn) {
      window.location.href = `/sign-in?redirect_url=${encodeURIComponent(window.location.href)}`;
      return;
    }
    ran.current = true;
    const headers = { "content-type": "application/json", "x-hive-team": team };
    const uid = userId || "local-dev";
    (async () => {
      try {
        // 1) Enroll (idempotent) — backfills the roster if it was empty.
        await fetch(`${BASE}/v1/zkauth/register`, { method: "POST", headers, body: JSON.stringify({ user_id: uid }) }).catch(() => {});
        // 2) Mint a membership proof for this project.
        const r = await fetch(`${BASE}/v1/zkauth/preview-proof`, { method: "POST", headers, body: JSON.stringify({ user_id: uid, project }) });
        if (!r.ok) {
          setError(`You don't have access to the "${team}" team's previews.`);
          return;
        }
        const d = await r.json();
        // 3) Hand off to the deployment domain to set the cookie + land on `next`.
        const scheme = host.includes("localhost") ? "http" : "https";
        const url = `${scheme}://${host}/_shadw/zk?p=${encodeURIComponent(d.proof)}&m=${encodeURIComponent(d.message)}&t=${encodeURIComponent(d.team)}&next=${encodeURIComponent(next)}`;
        setStatus("Unlocking preview…");
        window.location.href = url;
      } catch {
        setError("Could not verify access. Please try again.");
      }
    })();
  }, [loaded, signedIn, userId]);

  return (
    <div className="mx-auto flex min-h-[60vh] max-w-md flex-col items-center justify-center px-6 text-center">
      <div className="mb-4 h-8 w-8 animate-spin rounded-full border-2 border-border border-t-fg" />
      <h1 className="text-lg font-semibold">{error ? "Preview locked" : "Verifying access"}</h1>
      <p className="mt-2 text-sm text-secondary">{error || status}</p>
      {error && (
        <a href="/" className="mt-6 rounded-full border border-border px-4 py-2 text-sm hover:bg-subtle">
          Back to dashboard
        </a>
      )}
    </div>
  );
}
