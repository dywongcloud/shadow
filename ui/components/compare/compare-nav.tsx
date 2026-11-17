import Link from "next/link";
import { Tok } from "@/components/landing/tok";
import { COMPARE_LINKS } from "@/lib/compare-links";

const ALL_LINKS = [
  { label: "All comparisons", href: "/compare", slug: "" },
  ...COMPARE_LINKS.map((c) => ({ label: `shadw vs ${c.rival}`, href: `/compare/${c.slug}`, slug: c.slug })),
];

export function CompareNav({ active }: { active: string }) {
  return (
    <nav aria-label="Comparisons" className="border-b border-white/10">
      <div className="mx-auto flex max-w-6xl flex-wrap items-center gap-2 px-6 py-4">
        <span className="lp-mono mr-1 text-[12px] uppercase tracking-[0.18em] text-zinc-400">shadw vs.</span>
        {ALL_LINKS.map((l) => {
          const isActive = l.slug === active;
          return (
            <Link
              key={l.href}
              href={l.href}
              aria-current={isActive ? "page" : undefined}
              className={`tap-44 inline-flex min-h-[44px] items-center rounded-full border px-4 text-[13px] transition-colors ${
                isActive
                  ? "border-white/30 bg-white/10 text-white"
                  : "border-white/10 text-zinc-400 hover:border-white/25 hover:text-white"
              }`}
            >
              <Tok>{l.label}</Tok>
            </Link>
          );
        })}
      </div>
    </nav>
  );
}
