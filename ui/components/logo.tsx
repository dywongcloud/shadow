export function Logo({ className = "h-6 w-auto" }: { className?: string }) {
  return (
    <span
      className={`inline-flex items-center font-display font-bold tracking-tight text-fg ${className}`}
      aria-label="autheo.dev"
      style={{ lineHeight: 1 }}
    >
      autheo<span className="text-green">.dev</span>
    </span>
  );
}

// The navbar brand mark: a Δ (Greek capital delta). Theme-aware via
// `currentColor` (`text-fg` is black on light backgrounds, white on dark), so
// it inverts with the theme with no image swap or hydration flash. Kept as a
// vector path rather than a text glyph so it renders identically across
// platform fonts and sizes cleanly with a height utility (e.g. `h-5`).
export function VercelMark({ className = "h-5 w-auto" }: { className?: string }) {
  return (
    <svg viewBox="0 0 32 32" className={`${className} text-green`} fill="none" aria-label="Autheo DevHub" role="img">
      <path
        d="M16 2 29 26h-6.8l-1.8-3.9h-8.7L10 26H3L16 2Z"
        fill="currentColor"
      />
      <path
        d="m15.9 10.2-2.9 6.1h5.8l-2.9-6.1Z"
        fill="hsl(var(--background))"
      />
    </svg>
  );
}

// Forced-color variants for surfaces that are always one theme (e.g. the
// always-dark landing page uses <LogoWhite/>).
export function LogoWhite({ className = "h-6 w-auto" }: { className?: string }) {
  return (
    <span className={`inline-flex items-center font-display font-bold tracking-tight text-white ${className}`} aria-label="autheo.dev" style={{ lineHeight: 1 }}>
      autheo<span className="text-emerald-300">.dev</span>
    </span>
  );
}
export function LogoBlack({ className = "h-6 w-auto" }: { className?: string }) {
  return (
    <span className={`inline-flex items-center font-display font-bold tracking-tight text-black ${className}`} aria-label="autheo.dev" style={{ lineHeight: 1 }}>
      autheo<span className="text-emerald-700">.dev</span>
    </span>
  );
}
