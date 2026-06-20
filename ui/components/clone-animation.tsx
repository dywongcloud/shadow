"use client";

import { ExternalLink, FolderGit2, GitBranch } from "lucide-react";
import { Card } from "@/components/ui";

// Structural shape of a New Project template (matches app/new/page.tsx's Template
// so the page can pass its `selected` template straight through).
export interface CloneTemplate {
  name: string;
  desc: string;
  repo: string;
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
      {/* eslint-disable-next-line @next/next/no-img-element */}
      <img src="/github-mark.png" alt="GitHub" className={`${className} object-contain dark:hidden`} />
      {/* eslint-disable-next-line @next/next/no-img-element */}
      <img src="/github-mark-white.png" alt="GitHub" className={`${className} hidden object-contain dark:block`} />
    </>
  );
}

/** Inline GitHub mark for the "Cloning … to …" caption. */
function GitHubInline() {
  return (
    <>
      {/* eslint-disable-next-line @next/next/no-img-element */}
      <img src="/github-mark.png" alt="" className="inline h-3.5 w-3.5 object-contain align-[-2px] dark:hidden" />
      {/* eslint-disable-next-line @next/next/no-img-element */}
      <img src="/github-mark-white.png" alt="" className="hidden h-3.5 w-3.5 object-contain align-[-2px] dark:inline" />
    </>
  );
}

function Monogram({ t, className = "h-10 w-10" }: { t: CloneTemplate; className?: string }) {
  if (t.icon) {
    return (
      <span className={`flex ${className} shrink-0 items-center justify-center overflow-hidden rounded-lg border border-border bg-white`}>
        {/* eslint-disable-next-line @next/next/no-img-element */}
        <img src={t.icon} alt={t.name} className="h-3/4 w-3/4 object-contain" />
      </span>
    );
  }
  return (
    <span className={`flex ${className} shrink-0 items-center justify-center rounded-lg text-sm font-bold text-white`} style={{ background: t.color }}>
      {t.tag}
    </span>
  );
}

/**
 * The "Preparing Git Repository" step shown after pressing **Create** on /new,
 * just before the build-logs view. Collapses the New Project card into a summary
 * and visualizes the source repository being cloned into the build environment —
 * a GitHub source badge (with a radar ripple) flowing a data packet along a
 * dashed connector into the destination folder. Mirrors Vercel's flow.
 */
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
      {/* Collapsed New Project summary */}
      <Card className="mb-6 p-6 sm:p-8">
        <h1 className="mb-6 text-2xl font-semibold tracking-tight">New Project</h1>
        {template ? (
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

      {/* Deployment — Preparing Git Repository */}
      <Card className="p-6 sm:p-8">
        <h2 className="text-2xl font-semibold tracking-tight">Deployment</h2>
        <p className="mt-1.5 flex items-center gap-2 text-sm text-secondary">
          <span className="h-1.5 w-1.5 animate-pulse rounded-full bg-fg" />
          Preparing Git Repository…
        </p>

        <div className="mt-6 rounded-xl border border-border bg-subtle/30">
          {/* source → build-env flow */}
          <div className="flex items-center justify-center py-12">
            {/* Destination: the build environment (a new repo/folder) */}
            <div className="flex h-14 w-14 items-center justify-center rounded-full border border-border bg-card shadow-sm animate-[clone-breathe_2.4s_ease-in-out_infinite]">
              <FolderGit2 className="h-6 w-6 text-secondary" />
            </div>

            {/* Dashed connector with a traveling data packet (source → dest). */}
            <div className="relative mx-1 h-px w-20 sm:w-28">
              <div className="absolute inset-x-0 top-1/2 -translate-y-1/2 border-t border-dashed border-border-strong" />
              <span className="absolute top-1/2 h-1.5 w-1.5 -translate-y-1/2 rounded-full bg-fg shadow-[0_0_6px_hsl(var(--foreground))] animate-[clone-flow_2s_ease-in-out_infinite]" />
            </div>

            {/* Source: the GitHub repository, with a radar ripple. */}
            <div className="relative flex items-center justify-center">
              <span className="absolute h-14 w-14 rounded-full border border-border-strong animate-[clone-ripple_2s_ease-out_infinite]" />
              <span className="absolute h-14 w-14 rounded-full border border-border-strong animate-[clone-ripple_2s_ease-out_infinite] [animation-delay:1s]" />
              <div className="relative z-10 flex h-14 w-14 items-center justify-center rounded-full border border-border bg-card shadow-sm">
                <GitHubMark className="h-7 w-7" />
              </div>
            </div>
          </div>

          <div className="border-t border-border px-6 py-4 text-center text-sm text-secondary">
            Cloning <GitHubInline /> <span className="font-medium text-fg">{src}</span> to{" "}
            <GitHubInline /> <span className="font-medium text-fg">{dest}</span>
          </div>
        </div>
      </Card>
    </div>
  );
}
