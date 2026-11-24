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

/** The one article measure. Every other docs width is a named variant below. */
export const DOCS_MEASURE = "mx-auto max-w-3xl";
/** Index/platform: a hero code pane or full-width figures need the extra room. */
export const DOCS_MEASURE_WIDE = "mx-auto max-w-5xl";
/** API reference: the article shares its row with a table of contents or an example rail. */
export const DOCS_MEASURE_REFERENCE = "mx-auto max-w-6xl";

/** The one h1 rule for /docs. No route declares its own title class. */
export const DOCS_H2 = "scroll-mt-20 text-2xl font-semibold tracking-tight";

export function DocsH1({ children }: { children: ReactNode }) {
  return <h1 className="text-3xl font-semibold tracking-tight sm:text-4xl">{children}</h1>;
}

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
    <div className={wide ? DOCS_MEASURE_WIDE : DOCS_MEASURE}>
      <header className="mb-12">
        <div className="mb-2 font-mono text-xs uppercase tracking-widest text-link">
          {eyebrow}
        </div>
        <DocsH1>{title}</DocsH1>
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
      <h2 className={DOCS_H2}>{title}</h2>
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

/**
 * A lead destination. Two of these at the head of the index replace the
 * uniform card field with a deliberate change of scale.
 */
export function DocsLead({
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
      className="group tap-44 flex flex-col justify-between rounded-lg border border-border bg-card p-6 transition-colors hover:border-border-strong hover:bg-subtle/40"
    >
      <div className="h-7 w-7 text-fg">{icon}</div>
      <div className="mt-8">
        <h3 className="text-xl font-semibold tracking-tight">{title}</h3>
        <p className="mt-2 text-sm leading-relaxed text-secondary">{desc}</p>
      </div>
    </Link>
  );
}

/** The compact index: a destination without another identical frame. */
export function DocsIndexLink({
  title,
  desc,
  href,
}: {
  title: string;
  desc: string;
  href: string;
}) {
  return (
    <Link
      href={href}
      className="tap-44 group flex flex-col gap-0.5 border-b border-border py-3 transition-colors hover:border-border-strong"
    >
      <span className="text-sm font-medium text-fg group-hover:text-link">{title}</span>
      <span className="text-[13px] leading-relaxed text-secondary">{desc}</span>
    </Link>
  );
}
