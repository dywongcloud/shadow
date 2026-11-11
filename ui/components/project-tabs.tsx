"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";

export type ProjectTab =
  | "overview" | "graph" | "workflows" | "resources" | "deployments" | "analytics" | "logs" | "settings";

const IN_PAGE: [ProjectTab, string][] = [
  ["overview", "Overview"],
  ["graph", "Service Graph"],
  ["workflows", "Workflows"],
  ["resources", "Resources"],
  ["deployments", "Deployments"],
  ["analytics", "Analytics"],
];

function Tab({ href, label, isActive }: { href: string; label: string; isActive: boolean }) {
  return (
    <Link
      href={href}
      aria-current={isActive ? "page" : undefined}
      className={`relative inline-flex min-h-[44px] min-w-0 shrink-0 items-center whitespace-nowrap px-3 text-sm ${isActive ? "text-fg" : "text-secondary hover:text-fg"}`}
    >
      {label}
      {isActive && <span className="absolute inset-x-2 -bottom-px h-0.5 rounded-full bg-fg" />}
    </Link>
  );
}

/** Link-driven project sub-tabs. Analytics, Logs and Settings are real routes; the in-page tabs deep-link
 *  through `?tab=` (the project page reads it on mount). A route-backed tab wins over the caller's `active`,
 *  so Analytics reads as active on `/projects/<project>/analytics` wherever this bar is rendered. */
export function ProjectTabs({ project, active }: { project: string; active: ProjectTab }) {
  const pathname = usePathname() ?? "";
  const base = `/projects/${encodeURIComponent(project)}`;
  const hrefFor = (t: ProjectTab) =>
    t === "analytics" ? `${base}/analytics`
      : t === "logs" ? `${base}/logs`
        : t === "settings" ? `${base}/settings`
          : `${base}?tab=${t}`;
  const onRoute = (path: string) => pathname === path || pathname.startsWith(`${path}/`);
  const current: ProjectTab =
    onRoute(`${base}/analytics`) ? "analytics"
      : onRoute(`${base}/logs`) ? "logs"
        : onRoute(`${base}/settings`) ? "settings"
          : active;

  return (
    <div className="no-scrollbar mb-6 flex min-w-0 items-center gap-1 overflow-x-auto border-b border-border">
      {IN_PAGE.map(([k, label]) => (
        <Tab key={k} href={hrefFor(k)} label={label} isActive={current === k} />
      ))}
      <Tab href={hrefFor("logs")} label="Logs" isActive={current === "logs"} />
      <Tab href={hrefFor("settings")} label="Settings" isActive={current === "settings"} />
    </div>
  );
}
