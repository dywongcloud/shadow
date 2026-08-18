import { Triangle } from "@/components/ui";
import { Box, Server } from "lucide-react";

// Per-project framework logos for the dashboard's project grid. Rendered in the
// SAME circular avatar chrome the default `Triangle` uses (bordered circle,
// bg-fg / text-bg) so the grid stays visually uniform — only the glyph inside
// changes. The inner mark is drawn in `currentColor`, which the avatar sets to
// the inverted foreground, so it is legible in both light and dark themes
// without brand-tinted fills that vanish against one background. An unknown or
// empty framework returns the platform's own `Triangle` avatar unchanged, so a
// card is never blank.
//
// Slugs come from `fluid_build::framework` (nextjs, opennext, vinext, vite,
// astro, remix, sveltekit, nuxtjs, vue, gatsby, create-react-app, node,
// static) plus the container/docker kind.

/** The circular avatar wrapper matching `ui.tsx`'s `Triangle`. */
function Avatar({ className, viewBox, children, label }: { className?: string; viewBox: string; children: React.ReactNode; label: string }) {
  return (
    <span
      className={`flex h-9 w-9 shrink-0 items-center justify-center rounded-full border border-border bg-fg text-bg ${className ?? ""}`}
      aria-label={label}
    >
      <svg width="15" height="15" viewBox={viewBox} fill="currentColor" aria-hidden>
        {children}
      </svg>
    </span>
  );
}

const MARKS: Record<string, (c?: string) => React.ReactElement> = {
  nextjs: (c) => (
    <Avatar className={c} viewBox="0 0 128 128" label="Next.js">
      <circle cx="64" cy="64" r="60" fill="none" stroke="currentColor" strokeWidth="6" />
      <path d="M45 40h8v48h-8zM53 40h7l31 46v2h-8L53 47z" />
    </Avatar>
  ),
  opennext: (c) => MARKS.nextjs(c),
  vinext: (c) => MARKS.nextjs(c),
  vite: (c) => (
    <Avatar className={c} viewBox="0 0 24 24" label="Vite">
      <path d="M13.5 2 6 13h4l-1 9 9-13h-4z" />
    </Avatar>
  ),
  astro: (c) => (
    <Avatar className={c} viewBox="0 0 24 24" label="Astro">
      <path d="M12 2l6 18-6-3-6 3z" />
      <ellipse cx="12" cy="19" rx="5" ry="1.6" opacity="0.55" />
    </Avatar>
  ),
  remix: (c) => (
    <Avatar className={c} viewBox="0 0 24 24" label="Remix">
      <path d="M5 3h8a5 5 0 0 1 1 9.9c2 .4 2.6 1.8 2.7 4.1l.3 4H12l-.2-3.3c-.1-1.7-.7-2.4-2.3-2.4H9v5.7H5zM9 6.7v4.1h3.7a2 2 0 0 0 0-4.1z" />
    </Avatar>
  ),
  sveltekit: (c) => (
    <Avatar className={c} viewBox="0 0 24 24" label="Svelte">
      <path d="M18 5.4c-1.7-2.4-5-3.1-7.4-1.6L6.3 6.6A4.8 4.8 0 0 0 4.1 9.9a5 5 0 0 0 .5 3.2 4.8 4.8 0 0 0-.7 1.8 5.1 5.1 0 0 0 .9 3.9c1.7 2.4 5 3.1 7.4 1.6l4.3-2.8a4.8 4.8 0 0 0 2.2-3.3 5 5 0 0 0-.5-3.2 4.8 4.8 0 0 0 .7-1.8 5.1 5.1 0 0 0-.9-3.9M11 18.4a2.7 2.7 0 0 1-2.9-1.1 2.5 2.5 0 0 1-.4-2.2l.1-.3.2.2a4.7 4.7 0 0 0 1.4.7l.2.1v.2a.8.8 0 0 0 .1.5.8.8 0 0 0 .9.3l.2-.1 4-2.5.1-.2a.8.8 0 0 0 0-.8.8.8 0 0 0-.9-.3l-.4.1a4.7 4.7 0 0 1-1.5.1 2.7 2.7 0 0 1-1.8-1.1 2.5 2.5 0 0 1-.4-2.2 2.6 2.6 0 0 1 1.2-1.8l4-2.5a2.8 2.8 0 0 1 1.5-.4 2.7 2.7 0 0 1 2.9 1.1 2.5 2.5 0 0 1 .4 2.2l-.1.3-.2-.2a4.7 4.7 0 0 0-1.4-.7l-.2-.1v-.2a.8.8 0 0 0-.1-.5.8.8 0 0 0-.9-.3l-.2.1-4 2.5-.1.2a.8.8 0 0 0 0 .8.8.8 0 0 0 .9.3l.4-.1a4.7 4.7 0 0 1 1.5-.1 2.7 2.7 0 0 1 1.8 1.1 2.5 2.5 0 0 1 .4 2.2 2.6 2.6 0 0 1-1.2 1.8l-4 2.5a2.8 2.8 0 0 1-1 .3" />
    </Avatar>
  ),
  nuxtjs: (c) => (
    <Avatar className={c} viewBox="0 0 24 24" label="Nuxt">
      <path d="M13 4l7 16h-4L13 12 9 20H2l7-12 2 3-3.5 6h3z" />
    </Avatar>
  ),
  vue: (c) => (
    <Avatar className={c} viewBox="0 0 24 24" label="Vue">
      <path d="M2 3h4l6 10 6-10h4L12 21z" />
    </Avatar>
  ),
  gatsby: (c) => (
    <Avatar className={c} viewBox="0 0 24 24" label="Gatsby">
      <path d="M12 2a10 10 0 1 0 10 10h-2a8 8 0 1 1-8-8v2a6 6 0 0 1 5.7 4.1L12 12.9V16l6-6A10 10 0 0 0 12 2" />
    </Avatar>
  ),
  "create-react-app": (c) => (
    <Avatar className={c} viewBox="0 0 24 24" label="React">
      <circle cx="12" cy="12" r="2" />
      <g fill="none" stroke="currentColor" strokeWidth="1.2">
        <ellipse cx="12" cy="12" rx="10" ry="4" />
        <ellipse cx="12" cy="12" rx="10" ry="4" transform="rotate(60 12 12)" />
        <ellipse cx="12" cy="12" rx="10" ry="4" transform="rotate(120 12 12)" />
      </g>
    </Avatar>
  ),
  node: (c) => (
    <span className={`flex h-9 w-9 shrink-0 items-center justify-center rounded-full border border-border bg-fg text-bg ${c ?? ""}`} aria-label="Node">
      <Server className="h-[15px] w-[15px]" />
    </span>
  ),
};

/** The framework mark for a slug, or the platform Δ fallback (`Triangle`).
 *  Container/image/compose deploys get a box glyph. */
export function FrameworkLogo({ framework, kind, className }: { framework?: string; kind?: string; className?: string }) {
  const slug = (framework || "").trim().toLowerCase();
  const mk = MARKS[slug];
  if (mk) return mk(className);
  if (slug === "docker" || slug === "container" || kind === "container") {
    return (
      <span className={`flex h-9 w-9 shrink-0 items-center justify-center rounded-full border border-border bg-fg text-bg ${className ?? ""}`} aria-label="Container">
        <Box className="h-[15px] w-[15px]" />
      </span>
    );
  }
  // Unknown / static / empty → the platform's own Δ mark, unchanged.
  return <Triangle className={className} />;
}
