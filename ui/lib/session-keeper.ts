import { renewSession, sessionMintStatus } from "@/lib/api";
import { STALE_LEFT_FRACTION, fractionLeft, onSessionClock, remintAt, remintPeriodMs, sessionClock, type SessionClock } from "@/lib/session-clock";

/** A timer that fires this close to its mark still mints; a farther mark means the schedule moved, so it re-arms. */
const FIRE_SLACK_MS = 1_000;

interface MintStatus {
  state: string;
  notBeforeMs: number;
  lastAttemptMs: number;
}

/** Milliseconds until this tab's next re-mint attempt, a pure function of the clock, the failure budget and `now` (so re-arming at any moment lands on the
 *  same mark, and every path that mints or fails moves it). Healthy: the re-mint time of the freshest mint any tab made, at about 70% of its lifetime
 *  (already past = 0). After a failure: the end of the budget's backoff window (the ladder `sessionMintStatus` reports), stretched by up to half so tabs
 *  do not retry in step. Once the budget is spent (`failed`, automatic mints suspended): one attempt per period after the last, the recovery path. */
export function nextAttemptDelayMs(clock: SessionClock | null, status: MintStatus, now: number, jitter01: number): number {
  if (status.state === "backoff") return Math.max(0, status.notBeforeMs + 0.5 * jitter01 * (status.notBeforeMs - status.lastAttemptMs) - now);
  if (status.state === "failed") return Math.max(0, status.lastAttemptMs + remintPeriodMs(clock, jitter01) - now);
  return Math.max(0, remintAt(clock, now, jitter01) - now);
}

/** The ONE periodic re-mint of the session cookie, started by `SessionToken`. The mint's reported lifetime sets the clock
 *  (`ui/lib/session-clock.ts`, shared by every tab of the browser): a tab re-mints at about 70% of it with jitter, and a mint in
 *  any tab moves them all, so one tab mints per cycle. A hidden tab never mints; the moment it is visible again it re-mints at
 *  once when under 20% of the lifetime is left, else waits for its time. Mints go through `renewSession`, one at a time per tab,
 *  and a failed one follows the failure budget (`sessionMintStatus`), never a tight loop. */
export function startSessionKeeper(): () => void {
  let timer: ReturnType<typeof setTimeout> | null = null;
  let stopped = false;
  let jitter = Math.random();
  let jitterEpoch = -1;

  const hidden = () => document.visibilityState === "hidden";

  const arm = () => {
    if (stopped) return;
    if (timer !== null) clearTimeout(timer);
    const clock = sessionClock();
    const epoch = clock?.at ?? 0;
    if (epoch !== jitterEpoch) {
      jitterEpoch = epoch;
      jitter = Math.random();
    }
    timer = setTimeout(fire, nextAttemptDelayMs(clock, sessionMintStatus(), Date.now(), jitter));
  };

  const renew = async () => {
    await renewSession();
    arm();
  };

  const fire = () => {
    timer = null;
    if (stopped || hidden()) return;
    if (nextAttemptDelayMs(sessionClock(), sessionMintStatus(), Date.now(), jitter) > FIRE_SLACK_MS) {
      arm();
      return;
    }
    void renew();
  };

  const onVisible = () => {
    if (stopped || hidden()) return;
    const left = fractionLeft(sessionClock(), Date.now());
    const status = sessionMintStatus();
    if (left !== null && left < STALE_LEFT_FRACTION && (status.state === "ok" || status.state === "unattempted")) {
      if (timer !== null) clearTimeout(timer);
      timer = null;
      void renew();
      return;
    }
    arm();
  };

  const offClock = onSessionClock(arm);
  document.addEventListener("visibilitychange", onVisible);
  arm();
  return () => {
    stopped = true;
    if (timer !== null) clearTimeout(timer);
    offClock();
    document.removeEventListener("visibilitychange", onVisible);
  };
}
