import Image from "next/image";

// The official shadw wordmark — theme-aware. The black logo shows on light
// backgrounds, the white logo on dark ones, toggled purely with CSS (`dark:`)
// so there's no hydration flash. Size it with a height utility (e.g. `h-6`);
// width auto-scales to preserve the ~3:1 aspect ratio.
export function Logo({ className = "h-6 w-auto" }: { className?: string }) {
  return (
    <span className="inline-flex items-center" aria-label="shadw">
      <Image src="/shadw-logo-light.png" alt="shadw" width={274} height={91} className={`${className} block dark:hidden`} />
      <Image src="/shadw-logo-dark.png" alt="shadw" width={274} height={91} className={`${className} hidden dark:block`} />
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
    <svg viewBox="0 0 76 65" className={`${className} text-fg`} fill="currentColor" aria-label="Delta" role="img">
      {/* Uppercase delta: an open (outlined) equilateral triangle, its stroke
          weight built from the difference of the outer and inner triangles so
          the fill (currentColor) forms the letterform. */}
      <path
        fillRule="evenodd"
        clipRule="evenodd"
        d="M38 0 76 65H0L38 0Zm0 13.6L12.9 56.6H63.1L38 13.6Z"
      />
    </svg>
  );
}

// Forced-color variants for surfaces that are always one theme (e.g. the
// always-dark landing page uses <LogoWhite/>).
export function LogoWhite({ className = "h-6 w-auto" }: { className?: string }) {
  return <Image src="/shadw-logo-dark.png" alt="shadw" width={274} height={91} className={className} />;
}
export function LogoBlack({ className = "h-6 w-auto" }: { className?: string }) {
  return <Image src="/shadw-logo-light.png" alt="shadw" width={274} height={91} className={className} />;
}
