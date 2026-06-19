/* eslint-disable @next/next/no-img-element */

// The official shadw wordmark — theme-aware. The black logo shows on light
// backgrounds, the white logo on dark ones, toggled purely with CSS (`dark:`)
// so there's no hydration flash. Size it with a height utility (e.g. `h-6`);
// width auto-scales to preserve the ~3:1 aspect ratio.
export function Logo({ className = "h-6 w-auto" }: { className?: string }) {
  return (
    <span className="inline-flex items-center" aria-label="shadw">
      <img src="/shadw-logo-light.png" alt="shadw" className={`${className} block dark:hidden`} />
      <img src="/shadw-logo-dark.png" alt="shadw" className={`${className} hidden dark:block`} />
    </span>
  );
}

// Forced-color variants for surfaces that are always one theme (e.g. the
// always-dark landing page uses <LogoWhite/>).
export function LogoWhite({ className = "h-6 w-auto" }: { className?: string }) {
  return <img src="/shadw-logo-dark.png" alt="shadw" className={className} />;
}
export function LogoBlack({ className = "h-6 w-auto" }: { className?: string }) {
  return <img src="/shadw-logo-light.png" alt="shadw" className={className} />;
}
