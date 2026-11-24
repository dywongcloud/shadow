import Image from "next/image";
import Link from "next/link";
import type { ReactNode } from "react";

/**
 * One typographic and sectional system for every /docs route.
 *
 * The docs tree previously declared its own Section/H2/Callout/In helpers on
 * four routes, with five different h1 class strings and three different
 * measures. Everything here uses the existing globals.css tokens only; there
 * is no docs.css and no new color.
 */

/** The one article measure. */
export const DOCS_MEASURE = "mx-auto max-w-3xl";

export function DocsPage({
  eyebrow,
  title,
  lead,
  children,
  back,
  wide = false,
}: {
  eyebrow: string;
  title: ReactNode;
  lead?: ReactNode;
  children: ReactNode;
  back?: { href: string; label: string };
  wide?: boolean;
}) {
  return (
    <div className={wide ? "mx-auto max-w-5xl" : DOCS_MEASURE}>
      <header className="mb-12">
        <div className="mb-2 font-mono text-xs uppercase tracking-widest text-link">
          {eyebrow}
        </div>
        <h1 className="text-3xl font-semibold tracking-tight sm:text-4xl">{title}</h1>
        {lead && (
          <p className="mt-4 text-[15px] leading-relaxed text-secondary">{lead}</p>
        )}
      </header>
      <div className="space-y-2">{children}</div>
      {back && (
        <footer className="mt-16 flex items-center justify-between border-t border-border pt-6 text-sm text-muted">
          <span>© 2026 shadw.cloud</span>
          <Link href={back.href} className="tap-44 inline-flex items-center hover:text-fg">
            {back.label}
          </Link>
        </footer>
      )}
    </div>
  );
}

export function DocsSection({
  id,
  eyebrow,
  title,
  children,
}: {
  id: string;
  eyebrow?: string;
  title: ReactNode;
  children: ReactNode;
}) {
  return (
    <section
      id={id}
      className="scroll-mt-20 border-t border-border pt-12 first:border-0 first:pt-0"
    >
      {eyebrow && (
        <div className="mb-2 font-mono text-xs uppercase tracking-widest text-link">
          {eyebrow}
        </div>
      )}
      <h2 className="text-2xl font-semibold tracking-tight">{title}</h2>
      <div className="mt-5 space-y-4 text-[15px] leading-relaxed text-secondary">
        {children}
      </div>
    </section>
  );
}

export function DocsH3({ children }: { children: ReactNode }) {
  return <h3 className="!mt-8 text-lg font-semibold text-fg">{children}</h3>;
}

export function DocsIn({ children }: { children: ReactNode }) {
  return (
    <code className="rounded bg-subtle px-1.5 py-0.5 font-mono text-[0.85em] text-fg">
      {children}
    </code>
  );
}

export function DocsCallout({
  children,
  tone = "info",
}: {
  children: ReactNode;
  tone?: "info" | "warn";
}) {
  const cls =
    tone === "warn"
      ? "border-amber-500/30 bg-amber-500/10 text-amber-700 dark:text-amber-300"
      : "border-link/30 bg-link/5 text-secondary";
  return <div className={`rounded-lg border px-4 py-3 text-sm ${cls}`}>{children}</div>;
}

/**
 * A real, unretouched product capture with visible provenance. `note` is
 * mandatory: a screenshot without a caption is an assertion, not evidence.
 */
export function DocsFigure({
  src,
  alt,
  caption,
  note,
  width,
  height,
}: {
  src: string;
  alt: string;
  caption: string;
  note: string;
  width: number;
  height: number;
}) {
  return (
    <figure className="my-8">
      <div className="overflow-hidden rounded-xl border border-border bg-[#0a0a0a]">
        <Image
          src={src}
          alt={alt}
          width={width}
          height={height}
          sizes="(max-width: 768px) 100vw, 768px"
          className="h-auto w-full"
        />
      </div>
      <figcaption className="mt-3 text-sm font-medium text-fg">{caption}</figcaption>
      <p className="mt-1 text-xs leading-relaxed text-secondary">{note}</p>
    </figure>
  );
}

export function DocsCard({
  icon,
  title,
  desc,
  href,
}: {
  icon: ReactNode;
  title: string;
  desc: string;
  href: string;
}) {
  return (
    <Link
      href={href}
      className="group rounded-xl border border-border bg-card p-5 transition-colors hover:border-border-strong hover:bg-subtle/40"
    >
      <div className="h-6 w-6 text-fg">{icon}</div>
      <h3 className="mt-4 flex items-center gap-1.5 font-semibold">{title}</h3>
      <p className="mt-1.5 text-sm leading-relaxed text-secondary">{desc}</p>
    </Link>
  );
}
