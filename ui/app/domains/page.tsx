"use client";

import { Search, Filter, RotateCw } from "lucide-react";
import { Card, Button, Input, Badge, Triangle } from "@/components/ui";
import { usePoll, type Deployment } from "@/lib/api";

export default function DomainsPage() {
  const { data: deps } = usePoll<Deployment[]>("/deployments", 3000);
  const projects = new Map<string, Deployment>();
  for (const d of deps ?? []) if (!projects.has(d.project)) projects.set(d.project, d);
  const domains = Array.from(projects.values()).map((p) => p.alias);

  return (
    <div>
      <div className="mb-6 flex items-center gap-2">
        <Button variant="outline"><Filter className="h-4 w-4" /></Button>
        <div className="relative flex-1">
          <Search className="absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted" />
          <Input placeholder="Search any domain" className="pl-9" />
        </div>
        <Button variant="outline">Use Existing ▾</Button>
        <Button>Buy</Button>
      </div>
      <Card className="p-0">
        <div className="flex items-center gap-2 border-b border-border px-4 py-2.5 text-sm font-medium text-secondary">
          Select all
        </div>
        {domains.map((d) => (
          <div key={d} className="flex items-center justify-between border-b border-border px-4 py-3 last:border-0">
            <div>
              <div className="font-medium">{d}</div>
              <div className="flex items-center gap-1 text-xs text-secondary">
                <RotateCw className="h-3 w-3" /> Auto-renews · managed by Hive
              </div>
            </div>
            <div className="flex items-center gap-3">
              <Badge>Active</Badge>
              <Triangle className="h-6 w-6" />
            </div>
          </div>
        ))}
        {!domains.length && <div className="px-4 py-10 text-center text-sm text-secondary">No domains yet — deploy a project to get a managed subdomain.</div>}
      </Card>
    </div>
  );
}
