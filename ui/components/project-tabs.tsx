"use client";

import Link from "next/link";

export type ProjectTab =
  | "overview" | "graph" | "workflows" | "resources" | "deployments" | "logs" | "settings";

const IN_PAGE: [ProjectTab, string][] = [
  ["overview", "Overview"],
  ["graph", "Service Graph"],
  ["workflows", "Workflows"],
  ["resources", "Resources"],
  ["deployments", "Deployments"],
];

function Tab({ href, label, isActive }: { href: string; label: string; isActive: boolean }) {
  return (
    <Link
      href={href}
      className={`relative shrink-0 whitespace-nowrap px-3 py-2 text-sm ${isActive ? "text-fg" : "text-secondary hover:text-fg"}`}
    >
      {label}
      {isActive && <span className="absolute inset-x-2 -bottom-px h-0.5 rounded-full bg-fg" />}
    </Link>
  );
}

/** The LINK-driven project sub-tab bar for the sibling routes (Settings, Logs) that live under their own layouts; the main project page renders its own STATE-driven version (instant
 *  in-page switching) and its five in-page tabs deep-link back through `?tab=` (the page reads it on mount). */
export function ProjectTabs({ project, active }: { project: string; active: ProjectTab }) {
  const base = `/projects/${encodeURIComponent(project)}`;
  const hrefFor = (t: ProjectTab) =>
    t === "logs" ? `${base}/logs` : t === "settings" ? `${base}/settings` : `${base}?tab=${t}`;

  return (
    <div className="no-scrollbar mb-6 flex items-center gap-1 overflow-x-auto border-b border-border">
      {IN_PAGE.map(([k, label]) => (
        <Tab key={k} href={hrefFor(k)} label={label} isActive={active === k} />
      ))}
      <Tab href={hrefFor("logs")} label="Logs" isActive={active === "logs"} />
      <Tab href={hrefFor("settings")} label="Settings" isActive={active === "settings"} />
    </div>
  );
}
