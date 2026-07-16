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

// The classic Vercel triangle brand mark — theme-aware via `currentColor`
// (`text-fg` is black on light backgrounds, white on dark), so it inverts with
// the theme without any image swap or hydration flash. Size with a height util.
export function VercelMark({ className = "h-5 w-auto" }: { className?: string }) {
  return (
    <svg viewBox="0 0 76 65" className={`${className} text-fg`} fill="currentColor" aria-label="Vercel" role="img">
      <path d="M37.5274 0L75.0548 65H0L37.5274 0Z" />
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
