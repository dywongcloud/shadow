"use client";

/**
 * Whole-page auth-state guard — the structural fix for the mobile
 * "shadw.cloud redirects back to itself and flickers endlessly" incident.
 *
 * THE BUG CLASS THIS KILLS: the home route flips the ENTIRE page between two
 * different applications (marketing Landing ↔ Dashboard) and the chrome
 * mounts/unmounts (TopNav/Footer/pollers) purely on Clerk's LIVE client auth
 * state. On iOS/WebKit, a Clerk *development* instance keeps the real session
 * on a third-party origin (accounts.dev) and syncs it via a cross-site
 * handshake that ITP can break — leaving `__client_uat` claiming "signed in"
 * while `__session` never persists. Clerk's client state then OSCILLATES, and
 * raw `<SignedIn>/<SignedOut>` rendering faithfully re-paints the whole page on
 * every oscillation: an endless landing↔dashboard thrash that looks exactly
 * like a redirect loop, with the Dashboard's polling loops mounting and
 * unmounting on every flip (a request storm on top of the flicker).
 *
 * MECHANISM (why each piece exists):
 *
 *  1. ONE SHARED COMMIT STORE (module scope). Every flip surface (the home
 *     body, ChromeTop, ChromeBottom) renders from the SAME committed decision,
 *     so the page can never half-flip (dashboard body under landing chrome).
 *
 *  2. FIRST LOADED SAMPLE COMMITS INSTANTLY. The home page is force-dynamic
 *     precisely so Clerk's SSR knows the real request auth and a signed-in
 *     user never sees a landing→dashboard flash on first paint. The guard
 *     derives its first-paint value from the exact same `useAuth()` signal the
 *     old `<SignedIn>/<SignedOut>` used, and commits it with NO debounce — so
 *     first paint is behaviorally identical to the pre-guard code and the
 *     no-flash property is preserved by construction.
 *
 *  3. DEBOUNCED TRANSITIONS (hysteresis). After the initial commit, a
 *     DIFFERENT auth sample must hold stable for FLIP_DEBOUNCE_MS before the
 *     rendered view changes. A signal that flaps faster than the debounce
 *     never paints even once — the flap is absorbed silently and the page
 *     stays on its committed view. A genuine sign-out/sign-in (one clean
 *     transition) still goes through, just ~1.5s later, which keeps the
 *     documented login/logout chrome reactivity (see app-chrome.tsx) intact.
 *
 *  4. BOUNDED FLIPS, THEN AN HONEST TERMINAL STATE. Debounce alone bounds the
 *     RATE, not the COUNT: a slow oscillation (period > debounce) would still
 *     flip forever. So committed transitions are counted — per page load AND
 *     across reloads (sessionStorage, so a reload-loop can't reset the
 *     counter) — and past the bound the guard enters a terminal "degraded"
 *     state: a stable signed-out view plus a banner that tells the user the
 *     truth ("we can't keep you signed in on this browser") with working
 *     sign-in / try-again affordances. Degraded is sticky for the page's life;
 *     only the user's explicit "try again" (which clears the history and
 *     reloads) leaves it. Unbounded retry against a device that will never
 *     persist the cookie IS the loop — this is the bound that ends it.
 *
 *  5. NEUTRAL THIRD STATE. While Clerk hasn't resolved (`isLoaded` false and
 *     nothing committed) consumers render a neutral placeholder — never one of
 *     the two real views — so an unsettled signal cannot paint the wrong app
 *     even once. Resolution itself is bounded: if Clerk never loads
 *     (RESOLVE_TIMEOUT_MS), the guard commits signed-out — the Landing is a
 *     real, actionable state with a working sign-in affordance, unlike an
 *     eternal spinner.
 *
 * SSR SAFETY: module state is only ever mutated on the client (everything
 * funnels through effects or `typeof window` guards). On the server the store
 * is a pure function of the live `useAuth()` sample, so a Node server process
 * shared across requests can never leak one request's auth into another's.
 */

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
/** Set by the sign-in/sign-up pages right before Clerk takes over, so the
 *  guard can tell a DELIBERATE login completing apart from an ambient auth
 *  flap (see `markPendingSignIn` below for the bug this exists to fix). */
const PENDING_SIGNIN_KEY = "hive_pending_signin";

// ---- client-only module state (never touched during SSR) ----
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

/** Timestamps of recent committed flips, shared across reloads. All access is
 *  try/caught: storage can be unavailable (some private modes) and the guard
 *  must degrade to per-load counting, never throw. */
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
    return flips; // storage unavailable → the per-load bound still holds
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
    // First resolved sample: commit immediately (no debounce) so the first
    // paint matches Clerk's SSR answer — the no-flash property (see item 2).
    committed = sample;
    if (resolveTimer !== null) {
      clearTimeout(resolveTimer);
      resolveTimer = null;
    }
    notify();
    return;
  }
  if (sample === committed) {
    // The signal returned to the committed value before the debounce elapsed:
    // a flap, absorbed with ZERO paints. Deliberately not counted — an
    // absorbed flap is invisible and harmless; only committed paints are.
    clearPending();
    return;
  }
  // THE FIRST-LOGIN-DOESN'T-STICK BUG: Clerk's App Router integration
  // redirects post-sign-in via `next/navigation`'s router.push/replace (a
  // CLIENT-SIDE transition — confirmed in @clerk/nextjs's ClerkProvider,
  // which wires `routerPush`/`routerReplace` through `useRouter()`), not a
  // full page reload. `/sign-in` and `/` share this app's root layout, so
  // that transition never remounts this module — `committed` is still
  // whatever it was BEFORE the user signed in (normally "out", set the
  // moment they first loaded the signed-out home page). The genuine
  // false→true flip that sign-in produces therefore fell into the debounced
  // path below like any other sample, sitting on the stale "out" (Landing)
  // view for a full FLIP_DEBOUNCE_MS — long enough that a user reloads or
  // retries, and the SECOND attempt then hits a truly fresh module load
  // (committed === null), which commits instantly and "just works". The
  // marker below is set by the sign-in/sign-up pages the moment Clerk's
  // hosted flow takes over, so THIS transition — and only this one — can be
  // told apart from an ambient flap (iOS/ITP oscillation, the reason the
  // debounce exists) and commit immediately instead of waiting.
  if (sample === "in" && consumePendingSignIn()) {
    clearPending();
    commitFlip(sample);
    return;
  }
  if (pendingValue === sample) return; // countdown for this target already running
  clearPending();
  pendingValue = sample;
  pendingTimer = setTimeout(() => {
    pendingTimer = null;
    pendingValue = null;
    commitFlip(sample);
  }, FLIP_DEBOUNCE_MS);
}

/** Call right before handing off to Clerk's hosted sign-in/sign-up flow (see
 *  the sign-in/sign-up pages) — marks the NEXT observed "signed in" sample as
 *  a deliberate login rather than an ambient flap, so it commits instantly
 *  instead of waiting out the anti-flicker debounce. Self-clears on
 *  consumption; a stale/abandoned marker (never followed by a completed
 *  sign-in) simply never gets read and cannot fast-path an unrelated later
 *  session. */
export function markPendingSignIn(): void {
  if (typeof window === "undefined") return;
  try {
    sessionStorage.setItem(PENDING_SIGNIN_KEY, "1");
  } catch {
    /* storage unavailable — falls back to the ordinary debounced path */
  }
}

/** One-time client init: honor cross-load flap history (a reload loop arrives
 *  here already over-budget and paints the degraded state, not another lap),
 *  and bound how long "resolving" may last. */
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
        committed = "out"; // honest, actionable: Landing has a working sign-in
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

/**
 * The settled auth view every whole-page flip surface must render from
 * (instead of raw `<SignedIn>/<SignedOut>`). All consumers see the same value.
 *
 * MUST NOT be called when Clerk is disabled (no publishable key): `useAuth`
 * requires a ClerkProvider. Callers keep their existing `clerkEnabled` split
 * (a separate component subtree) exactly like the pre-guard code did.
 */
export function useSettledAuth(): SettledAuthView {
  const { isLoaded, isSignedIn } = useAuth();
  // First render (server AND hydration): a pure read — the live sample when
  // nothing is committed yet. On the force-dynamic home route Clerk's SSR
  // resolves auth, so this paints the correct real view immediately, exactly
  // as `<SignedIn>/<SignedOut>` did. No module mutation happens during render.
  const [view, setView] = useState<SettledAuthView>(() =>
    viewFor(isLoaded ? (isSignedIn ? "in" : "out") : null),
  );

  // Subscribe + one-time init. `initClient` may flip us into degraded (from
  // cross-load history) — the immediate sync call right after picks that up.
  useEffect(() => {
    const l = () => setView(viewFor(null));
    listeners.add(l);
    initClient();
    setView(viewFor(isLoaded ? (isSignedIn ? "in" : "out") : null));
    return () => {
      listeners.delete(l);
    };
    // Mount-only: the live sample is re-fed by the effect below on every change.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Feed every live sample through the debounced, bounded commit pipeline.
  useEffect(() => {
    feed(isLoaded === true, isSignedIn === true);
  }, [isLoaded, isSignedIn]);

  return view;
}

/** The degraded banner's "try again": clear the flap history and re-run the
 *  whole resolution from scratch. The ONLY exit from the degraded state — an
 *  explicit user action, never an automatic retry (that would be the loop). */
export function resetAuthSettle(): void {
  if (typeof window === "undefined") return;
  try {
    sessionStorage.removeItem(FLAP_STORE_KEY);
    sessionStorage.removeItem(PENDING_SIGNIN_KEY);
  } catch {
    /* storage unavailable — reload still restarts the per-load state */
  }
  window.location.reload();
}
