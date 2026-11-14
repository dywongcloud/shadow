/** Shared visual primitives for the project and storage analytics consoles.
 *  The palette is the dashboard's own neutral tokens plus its existing --link
 *  hue; the luminous landing palette is deliberately not imported here. */
export const ANALYTICS_ROOT_CLASS = "hive-analytics";

export const ANALYTICS_THEME_CSS = [
  ".hive-analytics {",
  "  --analytics-accent: hsl(var(--link));",
  "  --analytics-accent-soft: hsl(var(--link) / 0.10);",
  "  --analytics-accent-softer: hsl(var(--link) / 0.06);",
  "  --analytics-grid: hsl(var(--border));",
  "  --analytics-axis: hsl(var(--muted));",
  "  --analytics-surface: hsl(var(--card));",
  "  --analytics-text: hsl(var(--foreground));",
  "  --analytics-secondary: hsl(var(--secondary));",
  "  --analytics-subtle: hsl(var(--subtle));",
  "  --analytics-underline: hsl(var(--foreground));",
  "  --analytics-neutral-line: hsl(var(--foreground) / 0.36);",
  "  --analytics-bar: hsl(var(--foreground) / 0.055);",
  "  --analytics-bar-fill: hsl(var(--foreground) / 0.10);",
  "  --analytics-positive: hsl(var(--green));",
  "}",
  ".dark .hive-analytics {",
  "  --analytics-accent-soft: hsl(var(--link) / 0.16);",
  "  --analytics-accent-softer: hsl(var(--link) / 0.10);",
  "  --analytics-neutral-line: hsl(var(--foreground) / 0.52);",
  "  --analytics-bar: hsl(var(--foreground) / 0.09);",
  "  --analytics-bar-fill: hsl(var(--foreground) / 0.16);",
  "}",
  "@media (forced-colors: active) {",
  "  .hive-analytics .analytics-bar { outline: 1px solid CanvasText; }",
  "}",
  "@media (prefers-contrast: more) {",
  "  .hive-analytics { --analytics-grid: hsl(var(--border-strong)); --analytics-bar: hsl(var(--foreground) / 0.12); }",
  "}",
].join("\n");

export const ANALYTICS_TOKENS = {
  accent: "var(--analytics-accent)",
  accentSoft: "var(--analytics-accent-soft)",
  accentSofter: "var(--analytics-accent-softer)",
  grid: "var(--analytics-grid)",
  axis: "var(--analytics-axis)",
  surface: "var(--analytics-surface)",
  text: "var(--analytics-text)",
  secondary: "var(--analytics-secondary)",
  subtle: "var(--analytics-subtle)",
  underline: "var(--analytics-underline)",
  neutralLine: "var(--analytics-neutral-line)",
  bar: "var(--analytics-bar)",
  barFill: "var(--analytics-bar-fill)",
  positive: "var(--analytics-positive)",
} as const;

export const ANALYTICS_TEXT = {
  eyebrow: "min-w-0 break-words text-xs font-medium text-muted",
  heading: "mt-1 min-w-0 break-words text-xl font-semibold text-fg sm:text-2xl",
  title: "min-w-0 break-words text-sm font-semibold text-fg",
  caption: "min-w-0 break-words text-xs leading-snug text-secondary",
  axis: "text-[11px] tabular-nums text-muted",
  label: "min-w-0 break-words text-xs font-medium text-muted",
  value: "min-w-0 break-words text-2xl font-semibold leading-tight text-fg sm:text-3xl",
  captionTight: "min-w-0 break-words text-xs text-secondary",
} as const;

export const ANALYTICS_MARK = {
  lineWidth: 2,
  markerRadius: 4,
  gridWidth: 1,
  tailDash: "6 6",
  averageDash: "3 6",
  crosshairDash: "3 4",
} as const;

const SERIES_DASHES: (string | undefined)[] = [
  undefined,
  "2 5",
  "8 5",
  "1 5",
  "10 4 2 4",
  "5 3",
  "12 3 2 3",
  "4 2 1 2",
  "9 3",
  "2 2 8 2",
  "14 4",
  "6 2 2 2 2 2",
];

export function analyticsSeriesDash(index: number): string | undefined {
  return SERIES_DASHES[((index % SERIES_DASHES.length) + SERIES_DASHES.length) % SERIES_DASHES.length];
}

export const ANALYTICS_CARD_CLASS = "min-w-0 w-full max-w-full rounded-md border border-border bg-card p-0 shadow-none";
export const ANALYTICS_TAB_CLASS = "min-h-[44px] min-w-0 rounded-none border-b-2 border-transparent px-3 text-xs font-medium focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-border sm:text-sm";
export const ANALYTICS_PILL_CLASS = "inline-flex min-h-[44px] min-w-0 items-center justify-center gap-2 rounded-full border border-border bg-card px-3 text-xs font-medium text-fg hover:bg-subtle focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-border";
