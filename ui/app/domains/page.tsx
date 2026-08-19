"use client";

import { useState } from "react";
import Link from "next/link";
import { Search, Filter, RotateCw, ChevronRight, Wand2 } from "lucide-react";
import { Card, Button, Input, Badge } from "@/components/ui";
import { usePoll, type Deployment } from "@/lib/api";
import { deploymentHost } from "@/lib/deploy-url";
import { DomainWizard } from "@/components/domain-wizard";

interface DomainEntry { project: string; domain: string; verify_status?: string }

export default function DomainsPage() {
  const { data: deps } = usePoll<Deployment[]>("/deployments", 4000);
  const { data: custom, refresh } = usePoll<DomainEntry[]>("/v1/domains", 4000);
  const [wizardOpen, setWizardOpen] = useState(false);
  const [q, setQ] = useState("");

  const projects = Array.from(new Set((deps ?? []).map((d) => d.project)));
  // managed subdomains (one per project) + custom domains
  const managed = projects.map((p) => ({ domain: deploymentHost(`${p}.localhost`), project: p, kind: "managed" as const }));
  const customList = (custom ?? []).map((c) => ({ ...c, kind: "custom" as const }));
  const all = [...customList, ...managed].filter((d) => d.domain.toLowerCase().includes(q.toLowerCase()));

  return (
    <div>
      <div className="mb-6 flex items-end justify-between">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">Domains</h1>
          <p className="mt-1.5 text-sm text-secondary">The domains you have access to through your account.</p>
        </div>
        <div className="flex gap-2">
          <Button onClick={() => setWizardOpen(true)}><Wand2 className="h-4 w-4" /> Set up a domain</Button>
        </div>
      </div>

      <div className="mb-4 flex items-center gap-2">
        <Button variant="outline"><Filter className="h-4 w-4" /></Button>
        <div className="relative flex-1">
          <Search className="absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted" />
          <Input placeholder="Search for a domain…" value={q} onChange={(e) => setQ(e.target.value)} className="pl-9" />
        </div>
      </div>

      <Card className="p-0">
        <div className="flex items-center gap-2 border-b border-border px-4 py-2.5 text-sm font-medium text-secondary">Select all</div>
        {all.map((d) => (
          <Link
            key={d.domain}
            href={`/domains/${encodeURIComponent(d.domain)}`}
            className="flex items-center justify-between border-b border-border px-4 py-3 transition-colors last:border-0 hover:bg-subtle/50"
          >
            <div>
              <div className="flex items-center gap-2 font-medium">{d.domain} {d.kind === "custom" ? <Badge tone="blue">custom</Badge> : <Badge>managed</Badge>}</div>
              <div className="flex items-center gap-1 text-xs text-secondary"><RotateCw className="h-3 w-3" /> {d.project}{d.kind === "custom" && d.verify_status ? "" : " · auto-renews"}</div>
            </div>
            <div className="flex items-center gap-3">
              {d.kind === "custom" && d.verify_status === "pending"
                ? <Badge tone="amber">Verification pending</Badge>
                : <Badge tone="green">Active</Badge>}
              <ChevronRight className="h-4 w-4 text-muted" />
            </div>
          </Link>
        ))}
        {!all.length && <div className="px-4 py-10 text-center text-sm text-secondary">No domains yet — deploy a project, then attach a domain.</div>}
      </Card>

      {wizardOpen && (
        <DomainWizard
          projects={projects}
          onClose={() => {
            setWizardOpen(false);
            refresh();
          }}
        />
      )}
    </div>
  );
}
