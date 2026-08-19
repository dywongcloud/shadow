"use client";

import { ExternalLink } from "lucide-react";
import { usePoll } from "@/lib/api";

interface DomainEntry { project: string; domain: string; verify_status?: string }

/**
 * The custom domains attached to a project, as live links. Reads the tenant's
 * attach list (`GET /v1/domains`) and filters to this project — the platform
 * alias URL stays the primary one wherever it's shown; these are the
 * tenant-owned extras. A domain whose ownership verification is still pending
 * is NOT routed, so it renders as plain text with a hint, never a live href.
 */
export function AttachedDomains({ project }: { project: string }) {
  const { data } = usePoll<DomainEntry[]>("/v1/domains", 15000);
  const list = (data ?? []).filter((d) => d.project === project);
  if (list.length === 0) return null;
  return (
    <span className="inline-flex flex-wrap items-center gap-x-3 gap-y-1">
      {list.map((d) =>
        d.verify_status === "pending" ? (
          <span key={d.domain} className="inline-flex items-center gap-1 font-mono text-secondary">
            {d.domain} <span className="text-xs text-amber-500">verification pending</span>
          </span>
        ) : (
          <a
            key={d.domain}
            href={`https://${d.domain}`}
            target="_blank"
            rel="noreferrer"
            className="inline-flex items-center gap-1 font-mono text-link hover:underline"
          >
            {d.domain} <ExternalLink className="h-3 w-3" />
          </a>
        )
      )}
    </span>
  );
}
