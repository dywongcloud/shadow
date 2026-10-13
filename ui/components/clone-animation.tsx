"use client";

import { Check, ExternalLink, FolderGit2, GitBranch } from "lucide-react";
import { Card } from "@/components/ui";
import { cn } from "@/lib/utils";
import Image from "next/image";

/** Structural shape of a New Project template (a `GitTemplate | ImageTemplate` from app/new/page.tsx passes straight through). */
export interface CloneTemplate {
  name: string;
  desc: string;
  repo?: string;
  root?: string;
  branch?: string;
  tag: string;
  color: string;
  icon?: string;
}

/** Theme-aware GitHub mark — black octocat on light, white octocat on dark. */
function GitHubMark({ className }: { className: string }) {
  return (
    <>
      <Image src="/github-mark.png" alt="GitHub" width={28} height={28} className={`${className} object-contain dark:hidden`} />
      <Image src="/github-mark-white.png" alt="GitHub" width={28} height={28} className={`${className} hidden object-contain dark:block`} />
    </>
  );
}

/** Inline GitHub mark for the "Cloning … to …" caption. */
function GitHubInline() {
  return (
    <>
      <Image src="/github-mark.png" alt="" width={14} height={14} className="inline h-3.5 w-3.5 object-contain align-[-2px] dark:hidden" />
      <Image src="/github-mark-white.png" alt="" width={14} height={14} className="hidden h-3.5 w-3.5 object-contain align-[-2px] dark:inline" />
    </>
  );
}

function Monogram({ t, className = "h-10 w-10" }: { t: CloneTemplate; className?: string }) {
  if (t.icon) {
    return (
      <span className={`flex ${className} shrink-0 items-center justify-center overflow-hidden rounded-lg border border-border bg-white`}>
        <Image src={t.icon} alt={t.name} width={30} height={30} unoptimized className="h-3/4 w-3/4 object-contain" />
      </span>
    );
  }
  return (
    <span className={`flex ${className} shrink-0 items-center justify-center rounded-lg text-sm font-bold text-white`} style={{ background: t.color }}>
      {t.tag}
    </span>
  );
}

/** The "Preparing Git Repository" step shown after Create on /new, just before the build-logs view: it collapses the New Project card into a summary
 *  and visualizes the source repository being cloned into the build environment (a GitHub source badge, a data packet along a dashed connector). */
export function PreparingDeployment({
  template,
  team,
  src,
  dest,
}: {
  template: CloneTemplate | null;
  team: string;
  /** Source path, e.g. `vercel/vercel/examples/nextjs`. */
  src: string;
  /** Destination, e.g. `openedge/my-app`. */
  dest: string;
}) {
  return (
    <div className="mx-auto max-w-2xl">
      <Card className="mb-6 p-6 sm:p-8">
        <h1 className="mb-6 text-2xl font-semibold tracking-tight">New Project</h1>
        {template && template.repo ? (
          <div className="flex items-start gap-4 rounded-xl border border-border bg-subtle/40 p-4">
            <Monogram t={template} />
            <div className="min-w-0 flex-1">
              <div className="flex items-center gap-1.5 font-semibold">
                {template.name}
                <a href={`${template.repo}${template.root ? `/tree/${template.branch || "main"}/${template.root}` : ""}`} target="_blank" rel="noreferrer">
                  <ExternalLink className="h-3.5 w-3.5 text-muted hover:text-fg" />
                </a>
              </div>
              <div className="mt-0.5 text-sm text-secondary">{template.desc}</div>
              <div className="mt-3 text-xs text-muted">Cloning from GitHub</div>
              <div className="mt-1 flex flex-wrap items-center gap-x-4 gap-y-1 text-sm">
                <span className="flex items-center gap-1.5"><GitHubMark className="h-3.5 w-3.5" /> {src.split("/").slice(0, 2).join("/")}</span>
                <span className="flex items-center gap-1.5 text-secondary"><GitBranch className="h-3.5 w-3.5" /> {template.branch || "main"}</span>
                {template.root && <span className="flex items-center gap-1.5 text-secondary"><FolderGit2 className="h-3.5 w-3.5" /> {template.root}</span>}
              </div>
            </div>
          </div>
        ) : template ? (
          <div className="flex items-center gap-3 rounded-xl border border-border bg-subtle/40 p-4">
            <Monogram t={template} />
            <div className="min-w-0 flex-1">
              <div className="font-semibold">{template.name}</div>
              <div className="mt-0.5 text-sm text-secondary">{template.desc}</div>
              <div className="mt-1 font-mono text-xs text-muted">{src}</div>
            </div>
          </div>
        ) : (
          <div className="flex items-center gap-2 rounded-xl border border-border bg-subtle/40 p-4 text-sm">
            <GitHubMark className="h-5 w-5" /> <span className="font-medium">{src}</span>
          </div>
        )}
        <div className="mt-4 flex items-center gap-2 rounded-md border border-border bg-card px-3 py-2.5 text-sm">
          <span className="h-2 w-2 rounded-full bg-green" />
          <span className="font-medium">{team === "personal" ? "Personal Account" : team}</span>
        </div>
      </Card>

      <CloneCard src={src} dest={dest} />
    </div>
  );
}

/** The source-clone card: a GitHub source flowing a data packet along a dashed connector into the destination build folder. `idle=false` (default)
 *  is the live "Preparing Git Repository…" animation on /new; `idle=true` is a STILL, successful "Git repository cloned" state (no animation, a green
 *  check) stacked under the logs on the build-logs page. */
export function CloneCard({
  src,
  dest,
  idle = false,
  className = "",
}: {
  src: string;
  dest: string;
  idle?: boolean;
  className?: string;
}) {
  return (
    <Card className={cn("p-6 sm:p-8", className)}>
      <div className="flex items-center justify-between gap-3">
        <h2 className="text-2xl font-semibold tracking-tight">{idle ? "Source" : "Deployment"}</h2>
        {idle && (
          <span className="inline-flex items-center gap-1.5 rounded-full border border-green/30 bg-green/10 px-2.5 py-1 text-xs font-medium text-green">
            <Check className="h-3.5 w-3.5" /> Cloned
          </span>
        )}
      </div>
      <p className="mt-1.5 flex items-center gap-2 text-sm text-secondary">
        {idle ? (
          <>
            <Check className="h-4 w-4 text-green" /> Git repository cloned
          </>
        ) : (
          <>
            <span className="h-1.5 w-1.5 animate-pulse rounded-full bg-fg" /> Preparing Git Repository…
          </>
        )}
      </p>

      <div className="mt-6 rounded-xl border border-border bg-subtle/30">
        <div className="flex items-center justify-center py-12">
          <div
            className={cn(
              "relative flex h-14 w-14 items-center justify-center rounded-full border border-border bg-card shadow-sm",
              !idle && "animate-[clone-breathe_2.4s_ease-in-out_infinite]",
            )}
          >
            <FolderGit2 className="h-6 w-6 text-secondary" />
            {idle && (
              <span className="absolute -bottom-1 -right-1 flex h-5 w-5 items-center justify-center rounded-full border-2 border-card bg-green text-white">
                <Check className="h-3 w-3" strokeWidth={3} />
              </span>
            )}
          </div>

          <div className="relative mx-1 h-px w-20 sm:w-28">
            <div
              className={cn(
                "absolute inset-x-0 top-1/2 -translate-y-1/2 border-t border-dashed",
                idle ? "border-green/40" : "border-border-strong",
              )}
            />
            {!idle && (
              <span className="absolute top-1/2 h-1.5 w-1.5 -translate-y-1/2 rounded-full bg-fg shadow-[0_0_6px_hsl(var(--foreground))] animate-[clone-flow_2s_ease-in-out_infinite]" />
            )}
          </div>

          <div className="relative flex items-center justify-center">
            {!idle && (
              <>
                <span className="absolute h-14 w-14 rounded-full border border-border-strong animate-[clone-ripple_2s_ease-out_infinite]" />
                <span className="absolute h-14 w-14 rounded-full border border-border-strong animate-[clone-ripple_2s_ease-out_infinite] [animation-delay:1s]" />
              </>
            )}
            <div className="relative z-10 flex h-14 w-14 items-center justify-center rounded-full border border-border bg-card shadow-sm">
              <GitHubMark className="h-7 w-7" />
            </div>
          </div>
        </div>

        <div className="border-t border-border px-6 py-4 text-center text-sm text-secondary">
          {idle ? "Cloned " : "Cloning "}
          <GitHubInline /> <span className="font-medium text-fg">{src}</span> to{" "}
          <GitHubInline /> <span className="font-medium text-fg">{dest}</span>
        </div>
      </div>
    </Card>
  );
}
