"use client";

import { use } from "react";
import Link from "next/link";
import { usePathname } from "next/navigation";
import { Triangle } from "@/components/ui";
import { cn } from "@/lib/utils";
import { usePoll, type Deployment } from "@/lib/api";

const baseSections = [
  { slug: "", label: "General" },
  { slug: "git", label: "Git" },
  { slug: "environment-variables", label: "Environment Variables" },
  { slug: "functions", label: "Functions" },
  { slug: "network", label: "Network" },
  { slug: "build", label: "Build & Development" },
  { slug: "routing", label: "Routing" },
  { slug: "cron", label: "Cron Jobs" },
  { slug: "microfrontends", label: "Microfrontends" },
  { slug: "webhooks", label: "Webhooks" },
  { slug: "secure-compute", label: "Secure Compute" },
  { slug: "team", label: "Team & Privacy" },
];

/** A function's `browser_ineligible` reason names "container" only for the
 *  `runtime: "container"` case (see git.rs's build pipeline) — the list
 *  endpoint carries no cleaner boolean, so this is the signal the dashboard
 *  already has on hand without an extra request. */
function isContainerDeployment(dep: Deployment | null | undefined): boolean {
  return !!dep?.browser_ineligible?.some((b) => b.reason.includes("container"));
}

export function SettingsLayout(props: {
  children: React.ReactNode;
  paramsPromise: Promise<{ project: string }>;
}) {
  const params = use(props.paramsPromise);

  const { children } = props;

  const pathname = usePathname();
  const name = decodeURIComponent(params.project);
  const base = `/projects/${encodeURIComponent(name)}/settings`;

  // Only meaningful for a container-runtime project — a function/serverless
  // project has no image, no volume, no port/protocol ceiling to configure
  // here (that's already Functions + Network's job).
  const { data: deps } = usePoll<Deployment[]>("/deployments", 10000);
  const mine = (deps ?? []).filter((d) => d.project === name);
  const dep = mine.find((d) => d.production) ?? mine.sort((a, b) => b.created_at_ms - a.created_at_ms)[0] ?? null;
  const sections = isContainerDeployment(dep)
    ? [...baseSections.slice(0, 5), { slug: "container", label: "Container" }, ...baseSections.slice(5)]
    : baseSections;

  return (
    <div>
      <div className="mb-6 flex items-center gap-3">
        <Triangle />
        <div>
          <div className="text-xs text-secondary">Project Settings</div>
          <h1 className="text-xl font-semibold">{name}</h1>
        </div>
      </div>
      {/* Project sub-tabs now live in the top nav (breadcrumb-tabs model) with
          Settings active; the settings-section list below is the inner nav. */}
      <div className="grid grid-cols-1 gap-8 md:grid-cols-[200px_1fr]">
        <nav className="flex flex-row gap-1 overflow-x-auto md:flex-col">
          {sections.map((s) => {
            const href = s.slug ? `${base}/${s.slug}` : base;
            const active = s.slug ? pathname === href : pathname === base;
            return (
              <Link
                key={s.slug}
                href={href}
                className={cn(
                  "whitespace-nowrap rounded-md px-3 py-1.5 text-sm transition-colors",
                  active ? "bg-subtle font-medium text-fg" : "text-secondary hover:bg-subtle hover:text-fg"
                )}
              >
                {s.label}
              </Link>
            );
          })}
        </nav>
        <div className="min-w-0">{children}</div>
      </div>
    </div>
  );
}
