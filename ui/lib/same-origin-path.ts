const PROBE_ORIGIN = "https://same-origin.invalid";

/** `raw` when it is a path on this origin (one leading `/` followed by neither `/` nor `\`, no control characters, and it still resolves to the same
 *  origin), otherwise `fallback`: the only way a caller-supplied return path may become a redirect target. */
export function sameOriginPath(raw: unknown, fallback: string): string {
  if (typeof raw !== "string" || !/^\/(?![/\\])/.test(raw) || /\p{Cc}/u.test(raw)) return fallback;
  try {
    return new URL(raw, PROBE_ORIGIN).origin === PROBE_ORIGIN ? raw : fallback;
  } catch {
    return fallback;
  }
}
