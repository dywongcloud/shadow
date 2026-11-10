import Image from "next/image";

/** The shadw wordmark: black on light, white on dark, toggled with CSS (`dark:`) so there is no hydration flash. Size it with a height utility; `w-auto` keeps the ~3:1 ratio and `max-w-none` stops Tailwind's `img { max-width: 100% }` preflight from capping the width while the height stays fixed in a squeezed flex row. */
export function Logo({ className = "h-6 w-auto" }: { className?: string }) {
  return (
    <span className="inline-flex shrink-0 items-center" aria-label="shadw">
      <Image src="/shadw-logo-light.png" alt="shadw" width={274} height={91} className={`${className} block max-w-none dark:hidden`} />
      <Image src="/shadw-logo-dark.png" alt="shadw" width={274} height={91} className={`${className} hidden max-w-none dark:block`} />
    </span>
  );
}

/** The navbar brand mark, a Δ vector path in `currentColor`: it inverts with the theme with no image swap or hydration flash. */
export function VercelMark({ className = "h-5 w-auto" }: { className?: string }) {
  return (
    <svg viewBox="0 0 76 65" className={`${className} text-fg`} fill="currentColor" aria-label="Delta" role="img">
      <path
        fillRule="evenodd"
        clipRule="evenodd"
        d="M38 0 76 65H0L38 0Zm0 13.6L12.9 56.6H63.1L38 13.6Z"
      />
    </svg>
  );
}

/** Fixed-theme variants for surfaces that are always one theme (the always-dark landing page uses <LogoWhite/>). */
export function LogoWhite({ className = "h-6 w-auto" }: { className?: string }) {
  return <Image src="/shadw-logo-dark.png" alt="shadw" width={274} height={91} className={className} />;
}
export function LogoBlack({ className = "h-6 w-auto" }: { className?: string }) {
  return <Image src="/shadw-logo-light.png" alt="shadw" width={274} height={91} className={className} />;
}
