"use client";

/** Whole-page auth-state guard: the home route flips the ENTIRE page between marketing Landing and Dashboard on Clerk's live
 *  client auth state, which can oscillate (iOS/WebKit ITP vs a Clerk dev instance), so every flip surface renders from ONE
 *  shared committed decision: the first loaded sample commits instantly (no first-paint flash), later transitions must hold
 *  FLIP_DEBOUNCE_MS, committed flips are counted across reloads and past the bound end in a terminal "degraded" state (a stable
 *  signed-out view plus an honest banner), and an unresolved Clerk (RESOLVE_TIMEOUT_MS) commits signed-out. */

import { useAuth } from "@clerk/nextjs";
import { useEffect, useState } from "react";

export type SettledAuthView = "resolving" | "in" | "out" | "degraded";
type Decision = "in" | "out";

/** How long a NEW auth value must hold before the page is allowed to flip.
 *  Anything oscillating faster than this never paints at all. */
const FLIP_DEBOUNCE_MS = 1500;
/** Committed flips allowed within one page load before degrading (a real user
 *  produces at most one — sign-out; sign-in navigates to a fresh load). */
const MAX_FLIPS_PER_LOAD = 3;
/** Committed flips allowed across recent loads (sessionStorage-backed, so a
 *  reload/redirect loop cannot reset the count) before degrading on sight. */
const MAX_FLIPS_ACROSS_LOADS = 5;
const FLAP_WINDOW_MS = 90_000;
const FLAP_STORE_KEY = "hive_auth_flips";
/** Bounded wait for Clerk to resolve at all; then degrade to the signed-out
 *  view (which carries a working sign-in affordance) instead of spinning. */
const RESOLVE_TIMEOUT_MS = 8_000;
const PENDING_SIGNIN_KEY = "hive_pending_signin";

let committed: Decision | null = null;
let flips = 0;
let degraded = false;
let pendingValue: Decision | null = null;
let pendingTimer: ReturnType<typeof setTimeout> | null = null;
let resolveTimer: ReturnType<typeof setTimeout> | null = null;
let clientInited = false;
const listeners = new Set<() => void>();

function notify(): void {
  for (const l of listeners) l();
}

function clearPending(): void {
  if (pendingTimer !== null) clearTimeout(pendingTimer);
  pendingTimer = null;
  pendingValue = null;
}

/** Timestamps of recent committed flips, shared across reloads; storage may be unavailable, so it degrades to per-load
 *  counting and never throws. */
function readFlipLog(now: number): number[] {
  try {
    const raw = sessionStorage.getItem(FLAP_STORE_KEY);
    if (!raw) return [];
    const arr: unknown = JSON.parse(raw);
    if (!Array.isArray(arr)) return [];
    return arr.filter((t): t is number => typeof t === "number" && now - t < FLAP_WINDOW_MS);
  } catch {
    return [];
  }
}

function recordFlip(now: number): number {
  try {
    const log = readFlipLog(now);
    log.push(now);
    sessionStorage.setItem(FLAP_STORE_KEY, JSON.stringify(log));
    return log.length;
  } catch {
    return flips;
  }
}

function enterDegraded(): void {
  degraded = true;
  clearPending();
  if (resolveTimer !== null) {
    clearTimeout(resolveTimer);
    resolveTimer = null;
  }
  notify();
}

/** Read + clear the pending-sign-in marker in one shot. try/caught like every
 *  other storage access here — a private/blocked storage mode must not throw. */
function consumePendingSignIn(): boolean {
  try {
    const pending = sessionStorage.getItem(PENDING_SIGNIN_KEY) === "1";
    if (pending) sessionStorage.removeItem(PENDING_SIGNIN_KEY);
    return pending;
  } catch {
    return false;
  }
}

/** A debounced transition survived its hold window — apply it, bounded. */
function commitFlip(next: Decision): void {
  if (degraded || committed === next) return;
  flips += 1;
  const recent = recordFlip(Date.now());
  if (flips > MAX_FLIPS_PER_LOAD || recent > MAX_FLIPS_ACROSS_LOADS) {
    enterDegraded();
    return;
  }
  committed = next;
  notify();
}

/** Feed one live auth sample into the store. Idempotent (StrictMode-safe). */
function feed(isLoaded: boolean, isSignedIn: boolean): void {
  if (typeof window === "undefined" || degraded || !isLoaded) return;
  const sample: Decision = isSignedIn ? "in" : "out";
  if (committed === null) {
    committed = sample;
    if (resolveTimer !== null) {
      clearTimeout(resolveTimer);
      resolveTimer = null;
    }
    notify();
    return;
  }
  if (sample === committed) {
    clearPending();
    return;
  }
  if (sample === "in" && consumePendingSignIn()) {
    clearPending();
    commitFlip(sample);
    return;
  }
  if (pendingValue === sample) return;
  clearPending();
  pendingValue = sample;
  pendingTimer = setTimeout(() => {
    pendingTimer = null;
    pendingValue = null;
    commitFlip(sample);
  }, FLIP_DEBOUNCE_MS);
}

/** Call right before handing off to Clerk's hosted sign-in/sign-up: marks the NEXT observed "signed in" sample as a deliberate
 *  login (Clerk redirects client-side, so this module is not remounted) and commits it instantly instead of waiting out the
 *  debounce. Self-clears on consumption; an abandoned marker never fast-paths an unrelated session. */
export function markPendingSignIn(): void {
  if (typeof window === "undefined") return;
  try {
    sessionStorage.setItem(PENDING_SIGNIN_KEY, "1");
  } catch {}
}

/** One-time client init: honors cross-load flap history (a reload loop arrives already over budget and paints the degraded
 *  state) and bounds how long "resolving" may last. */
function initClient(): void {
  if (clientInited || typeof window === "undefined") return;
  clientInited = true;
  if (readFlipLog(Date.now()).length > MAX_FLIPS_ACROSS_LOADS) {
    degraded = true;
    return;
  }
  if (committed === null && !degraded && resolveTimer === null) {
    resolveTimer = setTimeout(() => {
      resolveTimer = null;
      if (committed === null && !degraded) {
        committed = "out";
        notify();
      }
    }, RESOLVE_TIMEOUT_MS);
  }
}

function viewFor(sample: Decision | null): SettledAuthView {
  if (degraded) return "degraded";
  if (committed !== null) return committed;
  return sample ?? "resolving";
}

/** The settled auth view every whole-page flip surface must render from, instead of raw `<SignedIn>/<SignedOut>`. Must NOT be
 *  called when Clerk is disabled (no publishable key): `useAuth` requires a ClerkProvider. */
export function useSettledAuth(): SettledAuthView {
  const { isLoaded, isSignedIn } = useAuth();
  const [view, setView] = useState<SettledAuthView>(() =>
    viewFor(isLoaded ? (isSignedIn ? "in" : "out") : null),
  );

  useEffect(() => {
    const l = () => setView(viewFor(null));
    listeners.add(l);
    initClient();
    // eslint-disable-next-line react-hooks/set-state-in-effect
    setView(viewFor(isLoaded ? (isSignedIn ? "in" : "out") : null));
    return () => {
      listeners.delete(l);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    feed(isLoaded === true, isSignedIn === true);
  }, [isLoaded, isSignedIn]);

  return view;
}

/** The degraded banner's "try again": clears the flap history and re-runs resolution; the ONLY exit from the degraded state,
 *  always an explicit user action. */
export function resetAuthSettle(): void {
  if (typeof window === "undefined") return;
  try {
    sessionStorage.removeItem(FLAP_STORE_KEY);
    sessionStorage.removeItem(PENDING_SIGNIN_KEY);
  } catch {}
  window.location.reload();
}
