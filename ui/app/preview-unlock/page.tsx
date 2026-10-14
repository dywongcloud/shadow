"use client";

import { useEffect, useRef, useState } from "react";
import { useUser } from "@clerk/nextjs";
import Link from "next/link";

const clerkEnabled = !!process.env.NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY;
const DEFAULT_RETRY_AFTER_SECS = 5;
const MAX_RETRY_AFTER_SECS = 60;

interface Outcome {
  unavailable: boolean;
  message: string;
}

/** `Retry-After` in seconds (the route sends seconds), bounded so a missing or odd value neither hammers the service nor strands the visitor. */
function retryAfterSecs(header: string | null): number {
  const secs = Number(header);
  return Number.isFinite(secs) && secs >= 1 ? Math.min(secs, MAX_RETRY_AFTER_SECS) : DEFAULT_RETRY_AFTER_SECS;
}

/** Protected-preview unlock interstitial: `/api/preview-unlock` verifies membership and mints the proof, then the page redirects into the preview. A refusal (4xx) is
 *  "Access denied"; an answer the server could not give (5xx or no answer) is "Temporarily unavailable" with a retry that waits out `Retry-After`. */
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
  const [outcome, setOutcome] = useState<Outcome | null>(null);
  const [retryIn, setRetryIn] = useState(0);
  const started = useRef(false);

  useEffect(() => {
    if (!loaded || started.current) return;
    const qs = window.location.search;
    const p = new URLSearchParams(qs);
    if (!p.get("host") || !p.get("project")) {
      // eslint-disable-next-line react-hooks/set-state-in-effect
      setOutcome({ unavailable: false, message: "This unlock link is missing deployment details." });
      return;
    }
    if (clerkEnabled && !signedIn) {
      // eslint-disable-next-line @next/next/no-location-assign-relative-destination
      window.location.href = `/sign-in?redirect_url=${encodeURIComponent(window.location.pathname + qs)}`;
      return;
    }
    started.current = true;

    fetch(`/api/preview-unlock${qs}`, { cache: "no-store" })
      .then(async (r) => ({ status: r.status, retryAfter: r.headers.get("retry-after"), body: await r.json().catch(() => ({})) }))
      .catch(() => ({ status: 0, retryAfter: null, body: { error: "network error" } as any }))
      .then(({ status, retryAfter, body }) => {
        if (status === 401 && body?.signin) {
          // eslint-disable-next-line @next/next/no-location-assign-relative-destination
          window.location.href = `/sign-in?redirect_url=${encodeURIComponent(window.location.pathname + qs)}`;
          return;
        }
        if (!body?.ok || !body?.url) {
          const unavailable = status === 0 || status >= 500;
          setOutcome({
            unavailable,
            message: body?.error || (unavailable ? "The unlock service is temporarily unavailable. Please try again." : "Could not unlock this preview."),
          });
          if (unavailable) setRetryIn(retryAfterSecs(retryAfter));
          return;
        }
        setVerified(true);
        setTimeout(() => (window.location.href = body.url), 650);
      });
  }, [loaded, signedIn]);

  useEffect(() => {
    if (retryIn <= 0) return;
    const timer = setTimeout(() => setRetryIn((secs) => secs - 1), 1000);
    return () => clearTimeout(timer);
  }, [retryIn]);

  const heading = !outcome ? "Authenticating" : outcome.unavailable ? "Temporarily unavailable" : "Access denied";

  return (
    <div className="fixed inset-0 z-[200] flex flex-col items-center justify-center bg-bg px-6 text-fg">
      {!outcome && <SpokeSpinner />}
      <h1 className="mt-7 text-4xl font-bold tracking-tight sm:text-5xl">{heading}</h1>

      {outcome ? (
        <>
          <p className="mt-3 max-w-sm text-center text-sm text-secondary">{outcome.message}</p>
          <div className="mt-6 flex items-center gap-3">
            {outcome.unavailable && (
              <button
                onClick={() => window.location.reload()}
                disabled={retryIn > 0}
                className="rounded-full border border-border px-4 py-2 text-sm hover:bg-subtle disabled:opacity-50 disabled:hover:bg-transparent"
              >
                {retryIn > 0 ? `Try again in ${retryIn}s` : "Try again"}
              </button>
            )}
            <Link href="/" className="rounded-full border border-border px-4 py-2 text-sm hover:bg-subtle">Back to dashboard</Link>
          </div>
        </>
      ) : (
        <p className="mt-7 max-w-[90vw] text-center font-mono text-[12px] font-semibold leading-relaxed text-green-700 dark:text-[#3dfa7e] sm:text-[13px]">
          {verified ? "Membership verified — access granted. Entering deployment…" : "Verifying membership…"}
        </p>
      )}
    </div>
  );
}
