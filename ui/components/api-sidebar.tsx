"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import { useEffect, useState } from "react";
import { ChevronLeft, ChevronDown } from "lucide-react";
import { API_CATEGORIES, METHOD_LABEL, METHOD_TONE, endpointHref } from "@/lib/api-catalog";

/** Small fixed-width HTTP method badge (GET / POST / PUT / PATCH / DEL). */
function MethodBadge({ method }: { method: keyof typeof METHOD_LABEL }) {
  return (
    <span className={`inline-flex w-11 shrink-0 justify-center rounded-md px-1 py-0.5 text-[10px] font-semibold ${METHOD_TONE[method]}`}>
      {METHOD_LABEL[method]}
    </span>
  );
}

/** The left sidebar shown on /docs/api-reference/* — Overview + Errors plus a
 *  collapsible tree of categories, each expanding to its endpoints (Vercel-style). */
export function ApiSidebar() {
  const pathname = usePathname();
  // Active category slug from /docs/api-reference/{category}/{endpoint}.
  const activeCat = pathname.startsWith("/docs/api-reference/")
    ? pathname.split("/")[3] ?? ""
    : "";
  const [open, setOpen] = useState<Set<string>>(() => new Set(activeCat ? [activeCat] : []));

  // Keep the active category expanded as you navigate between its endpoints.
  // Resyncs `open` from the route-derived `activeCat` on every navigation —
  // not fixable by a lazy initializer since it must react to later changes.
  useEffect(() => {
    // eslint-disable-next-line react-hooks/set-state-in-effect -- syncs open-set from route changes after mount, not just initial render
    if (activeCat) setOpen((s) => (s.has(activeCat) ? s : new Set(s).add(activeCat)));
  }, [activeCat]);

  const toggle = (slug: string) =>
    setOpen((s) => {
      const n = new Set(s);
      n.has(slug) ? n.delete(slug) : n.add(slug);
      return n;
    });

  const topLink = "flex items-center justify-between rounded-md px-3 py-1.5 text-sm transition-colors";

  return (
    <nav>
      <Link href="/docs" className="mb-4 flex items-center gap-1.5 px-3 text-sm text-secondary hover:text-fg">
        <ChevronLeft className="h-4 w-4" /> autheo REST API
      </Link>

      <Link
        href="/docs/api-reference"
        className={`${topLink} ${pathname === "/docs/api-reference" ? "bg-subtle font-medium text-fg" : "text-secondary hover:bg-subtle/60 hover:text-fg"}`}
      >
        Overview
      </Link>
      <Link href="/docs/api-reference#errors" className={`${topLink} text-secondary hover:bg-subtle/60 hover:text-fg`}>
        Errors
      </Link>

      <div className="mt-3">
        {API_CATEGORIES.map((cat) => {
          const expanded = open.has(cat.slug);
          return (
            <div key={cat.slug}>
              <button
                onClick={() => toggle(cat.slug)}
                className="flex w-full items-center justify-between rounded-md px-3 py-1.5 text-sm text-secondary transition-colors hover:bg-subtle/60 hover:text-fg"
              >
                <span className={activeCat === cat.slug ? "font-medium text-fg" : ""}>{cat.name}</span>
                <ChevronDown className={`h-4 w-4 text-muted transition-transform ${expanded ? "" : "-rotate-90"}`} />
              </button>
              {expanded && (
                <ul className="mb-1 ml-3 border-l border-border pl-2">
                  {cat.endpoints.map((ep) => {
                    const href = endpointHref(cat.slug, ep.slug);
                    const active = pathname === href;
                    return (
                      <li key={ep.slug}>
                        <Link
                          href={href}
                          className={`flex items-start gap-2 rounded-md px-2 py-1.5 text-sm transition-colors ${
                            active ? "bg-subtle font-medium text-fg" : "text-secondary hover:bg-subtle/60 hover:text-fg"
                          }`}
                        >
                          <MethodBadge method={ep.method} />
                          <span className="leading-snug">{ep.name}</span>
                        </Link>
                      </li>
                    );
                  })}
                </ul>
              )}
            </div>
          );
        })}
      </div>
    </nav>
  );
}
