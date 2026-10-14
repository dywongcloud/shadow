"use client";

import { useEffect, useRef, useState } from "react";
import { useUser } from "@clerk/nextjs";
import Link from "next/link";

const clerkEnabled = !!process.env.NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY;

/** Protected-preview unlock interstitial: `/api/preview-unlock` verifies membership and mints the proof, then the page redirects into the preview. */
export default function PreviewUnlockPage() {
  return clerkEnabled ? <ClerkGate /> : <Authenticating signedIn loaded />;
}

function ClerkGate() {
  const { isSignedIn, isLoaded } = useUser();
  return <Authenticating signedIn={!!isSignedIn} loaded={isLoaded} />;
}

/** macOS/iOS-style spoke spinner; inherits the foreground color (currentColor). */
function SpokeSpinner() {
  return (
    <svg className="h-9 w-9 animate-spin" style={{ animationDuration: "0.85s" }} viewBox="0 0 24 24" aria-hidden>
      {Array.from({ length: 12 }).map((_, i) => (
        <rect key={i} x="11" y="1.5" width="2" height="6" rx="1" fill="currentColor" opacity={(i + 1) / 12} transform={`rotate(${i * 30} 12 12)`} />
      ))}
    </svg>
  );
}

function Authenticating({ signedIn, loaded }: { signedIn: boolean; loaded: boolean }) {
  const [verified, setVerified] = useState(false);
  const [error, setError] = useState("");
  const started = useRef(false);

  useEffect(() => {
    if (!loaded || started.current) return;
    const qs = window.location.search;
    const p = new URLSearchParams(qs);
    if (!p.get("host") || !p.get("project")) {
      // eslint-disable-next-line react-hooks/set-state-in-effect
      setError("This unlock link is missing deployment details.");
      return;
    }
    if (clerkEnabled && !signedIn) {
      // eslint-disable-next-line @next/next/no-location-assign-relative-destination
      window.location.href = `/sign-in?redirect_url=${encodeURIComponent(window.location.pathname + qs)}`;
      return;
    }
    started.current = true;

    fetch(`/api/preview-unlock${qs}`, { cache: "no-store" })
      .then(async (r) => ({ status: r.status, body: await r.json().catch(() => ({})) }))
      .catch(() => ({ status: 0, body: { error: "network error" } as any }))
      .then(({ status, body }) => {
        if (status === 401 && body?.signin) {
          // eslint-disable-next-line @next/next/no-location-assign-relative-destination
          window.location.href = `/sign-in?redirect_url=${encodeURIComponent(window.location.pathname + qs)}`;
          return;
        }
        if (!body?.ok || !body?.url) {
          setError(body?.error || "Could not unlock this preview.");
          return;
        }
        setVerified(true);
        setTimeout(() => (window.location.href = body.url), 650);
      });
  }, [loaded, signedIn]);

  return (
    <div className="fixed inset-0 z-[200] flex flex-col items-center justify-center bg-bg px-6 text-fg">
      {!error && <SpokeSpinner />}
      <h1 className="mt-7 text-4xl font-bold tracking-tight sm:text-5xl">{error ? "Access denied" : "Authenticating"}</h1>

      {error ? (
        <>
          <p className="mt-3 max-w-sm text-center text-sm text-secondary">{error}</p>
          <Link href="/" className="mt-6 rounded-full border border-border px-4 py-2 text-sm hover:bg-subtle">Back to dashboard</Link>
        </>
      ) : (
        <p className="mt-7 max-w-[90vw] text-center font-mono text-[12px] font-semibold leading-relaxed text-green-700 dark:text-[#3dfa7e] sm:text-[13px]">
          {verified ? "Membership verified — access granted. Entering deployment…" : "Verifying membership…"}
        </p>
      )}
    </div>
  );
}
