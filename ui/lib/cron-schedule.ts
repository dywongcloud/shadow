const MINUTES_PER_HOUR = 60;
const HOURS_PER_DAY = 24;
const LAST_MINUTE = 59;
const LAST_HOUR = 23;

/** The step N of a star-slash-N field when it divides `period` evenly (anything else fires at uneven gaps), else null. */
function evenStep(field: string, period: number): number | null {
  const n = Number(/^\*\/(\d+)$/.exec(field)?.[1]);
  return n >= 1 && n < period && period % n === 0 ? n : null;
}

function plainNumber(field: string, max: number): number | null {
  return /^\d{1,2}$/.test(field) && Number(field) <= max ? Number(field) : null;
}

/** Best-effort English label for the common cron shapes, or null so the raw expression is shown (never lies about a schedule it can't confidently describe). A 6-field
 *  expression is the platform's native form with a leading seconds field: only a seconds field of exactly `0` is dropped before labelling. */
export function describeSchedule(expr: string): string | null {
  const f = expr.trim().split(/\s+/);
  if (f.length !== 5 && f.length !== 6) return null;
  if (f.length === 6 && f[0] !== "0") return null;
  const [min, hour, dom, month, dow] = f.length === 6 ? f.slice(1) : f;
  if (dom !== "*" || month !== "*" || dow !== "*") return null;
  const everyMinutes = evenStep(min, MINUTES_PER_HOUR);
  if (hour === "*") {
    if (min === "*") return "Every minute";
    return everyMinutes ? `Every ${everyMinutes} minute${everyMinutes === 1 ? "" : "s"}` : null;
  }
  const atMinute = plainNumber(min, LAST_MINUTE);
  if (atMinute === null) return null;
  const everyHours = evenStep(hour, HOURS_PER_DAY);
  if (everyHours) return `Every ${everyHours} hours, at minute ${atMinute}`;
  const atHour = plainNumber(hour, LAST_HOUR);
  return atHour === null ? null : `Daily at ${String(atHour).padStart(2, "0")}:${String(atMinute).padStart(2, "0")} UTC`;
}
