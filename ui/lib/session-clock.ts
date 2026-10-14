/** The lifetime of the session credential (the httpOnly `hive_jwt` cookie) as this browser knows it. JS cannot read the cookie, so the
 *  mint route echoes its `expires_in` and every mint, in any tab, records `{issued, at, life, team}` here: module state mirrored to
 *  localStorage, so the tabs of one browser share ONE clock and a mint anywhere moves every tab's re-mint time. Pure helpers below
 *  decide when to re-mint and whether a rejection can be the session's fault; `ui/lib/session-keeper.ts` owns the timer. */

const STORE_KEY = "hive_session_clock";

export interface SessionClock {
  /** When the mint request was SENT (ms epoch): the credential lapses no earlier than `issued + life`. */
  issued: number;
  /** When that mint's answer arrived and the cookie was replaced (ms epoch). */
  at: number;
  /** The credential's lifetime in ms; 0 when the mint did not say (an unenforced node, a route that predates `expires_in`). */
  life: number;
  /** The tenant the credential was minted for. */
  team: string;
}

/** Re-mint at about 70% of the lifetime, spread over +-5% of it so tabs and browsers do not renew in step. */
export const REMINT_AT_FRACTION = 0.7;
export const REMINT_JITTER_FRACTION = 0.05;
/** A tab that becomes visible with less than this share of the lifetime left re-mints at once. */
export const STALE_LEFT_FRACTION = 0.2;
/** The lifetime assumed when a mint did not say: the hour every token lived before org roles got 900 s. */
export const FALLBACK_LIFETIME_MS = 3_600_000;
/** A request sent within this margin of the lapse (10% of a short lifetime) waits for a fresh mint first. */
export const LAPSE_MARGIN_MS = 3_000;

/** How long after a mint the next one is due: about 70% of the lifetime (the fallback hour when unknown); `jitter01` (0..1) spreads it over +-5% of the lifetime. */
export function remintPeriodMs(c: SessionClock | null, jitter01: number): number {
  return (c && c.life > 0 ? c.life : FALLBACK_LIFETIME_MS) * (REMINT_AT_FRACTION + REMINT_JITTER_FRACTION * (2 * jitter01 - 1));
}

/** When to re-mint (ms epoch): one period after the mint was sent. */
export function remintAt(c: SessionClock | null, now: number, jitter01: number): number {
  return (c ? c.issued : now) + remintPeriodMs(c, jitter01);
}

/** The share of the lifetime still ahead (1 = just minted, below 0 = lapsed); null when the lifetime is unknown. */
export function fractionLeft(c: SessionClock | null, now: number): number | null {
  return c && c.life > 0 ? (c.issued + c.life - now) / c.life : null;
}

/** Whether a request sent now could meet a lapsed credential: false when the lifetime is unknown (the server decides). */
export function lapsing(c: SessionClock | null, now: number): boolean {
  return !!c && c.life > 0 && now >= c.issued + c.life - Math.min(LAPSE_MARGIN_MS, c.life / 10);
}

/** Whether the credential is KNOWN valid for `team`: a 403 on such a session is an authorization answer (role matrix, operator-only), never a lapse a re-mint would cure. */
export function validFor(c: SessionClock | null, team: string, now: number): boolean {
  return !!c && c.life > 0 && c.team === team && !lapsing(c, now);
}

let clock: SessionClock | null = null;
let attached = false;
const listeners = new Set<() => void>();

function parse(raw: string | null): SessionClock | null {
  if (!raw) return null;
  try {
    const v = JSON.parse(raw) as Partial<SessionClock>;
    if (typeof v.issued !== "number" || typeof v.at !== "number" || typeof v.life !== "number" || typeof v.team !== "string") return null;
    return { issued: v.issued, at: v.at, life: v.life, team: v.team };
  } catch {
    return null;
  }
}

function notify(): void {
  for (const cb of [...listeners]) cb();
}

/** First touch in a browser: adopt the freshest record another tab or an earlier page left, and follow the other tabs' mints from then on. */
function attach(): void {
  if (attached || typeof window === "undefined") return;
  attached = true;
  try {
    const stored = parse(localStorage.getItem(STORE_KEY));
    if (stored && (!clock || stored.at > clock.at)) clock = stored;
  } catch {}
  window.addEventListener("storage", (e) => {
    if (e.key !== STORE_KEY) return;
    const seen = parse(e.newValue);
    if (seen && (!clock || seen.at > clock.at)) {
      clock = seen;
      notify();
    }
  });
}

/** The freshest mint this browser knows of (any tab), or null before the first one. */
export function sessionClock(): SessionClock | null {
  attach();
  return clock;
}

/** Records a mint that just completed here (`issued` = when it was sent, `lifeMs` = the lifetime it reported, 0 if none) and tells every tab. */
export function noteMinted(issued: number, lifeMs: number, team: string): void {
  attach();
  clock = { issued, at: Date.now(), life: lifeMs, team };
  try {
    localStorage.setItem(STORE_KEY, JSON.stringify(clock));
  } catch {}
  notify();
}

/** Tells this tab's listeners that a mint ended WITHOUT moving the clock (it failed), so the keeper re-arms from the failure budget. */
export function announceMintFailure(): void {
  notify();
}

/** Calls `cb` after every mint, this tab's or another's, and after this tab's failed ones; returns the unsubscribe. */
export function onSessionClock(cb: () => void): () => void {
  attach();
  listeners.add(cb);
  return () => {
    listeners.delete(cb);
  };
}
