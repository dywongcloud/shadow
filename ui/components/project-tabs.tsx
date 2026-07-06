"use client";

import Link from "next/link";

// The project-scope sub-tab bar (Overview · Service Graph · Workflows · Resources
// · Deployments · Logs · Settings). The main project page (`app/projects/[project]/
// page.tsx`) renders its own STATE-driven version (instant in-page switching); this
// LINK-driven version is for the sibling routes (Settings, Logs) that live under
// their own layouts and would otherwise lose the project sub-header entirely. The
// five in-page tabs deep-link back via `?tab=` (the page reads it on mount).
export type ProjectTab =
  | "overview" | "graph" | "workflows" | "resources" | "deployments" | "logs" | "settings";

const IN_PAGE: [ProjectTab, string][] = [
  ["overview", "Overview"],
  ["graph", "Service Graph"],
  ["workflows", "Workflows"],
  ["resources", "Resources"],
  ["deployments", "Deployments"],
];

export function ProjectTabs({ project, active }: { project: string; active: ProjectTab }) {
  const base = `/projects/${encodeURIComponent(project)}`;
  const hrefFor = (t: ProjectTab) =>
    t === "logs" ? `${base}/logs` : t === "settings" ? `${base}/settings` : `${base}?tab=${t}`;

  const Tab = ({ k, label }: { k: ProjectTab; label: string }) => (
    <Link
      href={hrefFor(k)}
      className={`relative shrink-0 whitespace-nowrap px-3 py-2 text-sm ${active === k ? "text-fg" : "text-secondary hover:text-fg"}`}
    >
      {label}
      {active === k && <span className="absolute inset-x-2 -bottom-px h-0.5 rounded-full bg-fg" />}
    </Link>
  );

  return (
    <div className="no-scrollbar mb-6 flex items-center gap-1 overflow-x-auto border-b border-border">
      {IN_PAGE.map(([k, label]) => (
        <Tab key={k} k={k} label={label} />
      ))}
      <Tab k="logs" label="Logs" />
      <Tab k="settings" label="Settings" />
    </div>
  );
}
