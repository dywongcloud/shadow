"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import { Triangle } from "@/components/ui";
import { cn } from "@/lib/utils";

const sections = [
  { slug: "", label: "General" },
  { slug: "environment-variables", label: "Environment Variables" },
  { slug: "functions", label: "Functions" },
  { slug: "build", label: "Build & Development" },
  { slug: "routing", label: "Routing" },
  { slug: "microfrontends", label: "Microfrontends" },
  { slug: "webhooks", label: "Webhooks" },
  { slug: "secure-compute", label: "Secure Compute" },
  { slug: "team", label: "Team & Privacy" },
];

export default function SettingsLayout({
  children,
  params,
}: {
  children: React.ReactNode;
  params: { project: string };
}) {
  const pathname = usePathname();
  const name = decodeURIComponent(params.project);
  const base = `/projects/${encodeURIComponent(name)}/settings`;

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
