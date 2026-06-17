"use client";

import Link from "next/link";
import { Plus, Search, Route as RouteIcon, Trash2, Info, ArrowRight } from "lucide-react";
import { useState } from "react";
import { Badge, Button, Card } from "@/components/ui";
import { apiSend, usePoll } from "@/lib/api";

interface Routing {
  redirects: { source: string; destination: string; status: number }[];
  rewrites: { source: string; destination: string }[];
}

export default function RoutingRulesPage() {
  const { data, refresh } = usePoll<Routing>("/v1/routing", 4000);
  const [q, setQ] = useState("");

  const rules = [
    ...(data?.redirects ?? []).map((r) => ({ kind: "Redirect" as const, ...r })),
    ...(data?.rewrites ?? []).map((r) => ({ kind: "Rewrite" as const, status: 0, ...r })),
  ].filter((r) => !q || r.source.includes(q) || r.destination.includes(q));

  async function remove(kind: string, source: string) {
    const path = kind === "Redirect" ? "/v1/routing/redirects/delete" : "/v1/routing/rewrites/delete";
    await apiSend("POST", path, { source });
    refresh();
  }

  return (
    <div>
      <h1 className="text-2xl font-semibold tracking-tight">Routing Rules</h1>
      <p className="mt-1 text-sm text-secondary">
        Manage CDN routing rules that execute at the edge. These rules override any rules defined in the deployment.
      </p>

      <Card className="mt-6 flex items-center justify-between bg-subtle/60 py-3">
        <span className="flex items-center gap-2 text-sm text-secondary"><Info className="h-4 w-4" /> View the full routing order to understand how routing rules are evaluated.</span>
        <Button variant="outline">View Routing Order</Button>
      </Card>

      <Card className="mt-4 flex items-center gap-2 py-3">
        <span className="flex h-4 w-4 items-center justify-center rounded-full border border-border-strong" />
        <span className="text-sm">Routes usage: <span className="font-semibold">{rules.length}</span> of <span className="font-semibold">100</span></span>
      </Card>

      <div className="mt-4 flex items-center gap-2">
        <div className="relative flex-1">
          <Search className="absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted" />
          <input value={q} onChange={(e) => setQ(e.target.value)} placeholder="Search by name, description, or path…"
            className="w-full rounded-lg border border-border bg-card py-2.5 pl-9 pr-3 text-sm focus:outline-none focus:ring-2 focus:ring-border" />
        </div>
        <Link href="/cdn/routing/new"><Button><Plus className="h-4 w-4" /> Create Route</Button></Link>
      </div>

      {rules.length ? (
        <Card className="mt-4 p-0">
          {rules.map((r, i) => (
            <div key={i} className="flex items-center justify-between border-b border-border px-4 py-3 text-sm last:border-0">
              <div className="flex items-center gap-3">
                <Badge tone={r.kind === "Redirect" ? "amber" : "blue"}>{r.kind}{r.kind === "Redirect" ? ` ${r.status}` : ""}</Badge>
                <span className="font-mono text-xs">{r.source}</span>
                <ArrowRight className="h-3.5 w-3.5 text-muted" />
                <span className="font-mono text-xs text-secondary">{r.destination}</span>
              </div>
              <button onClick={() => remove(r.kind, r.source)} className="text-muted hover:text-red-500"><Trash2 className="h-4 w-4" /></button>
            </div>
          ))}
        </Card>
      ) : (
        <Card className="mt-4 flex flex-col items-center gap-3 py-16 text-center">
          <span className="flex h-12 w-12 items-center justify-center rounded-xl border border-border"><RouteIcon className="h-5 w-5 text-muted" /></span>
          <div className="text-base font-semibold">No Routing Rules</div>
          <p className="max-w-sm text-sm text-secondary">There are no routes defined yet for this project. Get started by creating a route and defining your traffic rules.</p>
          <Link href="/cdn/routing/new"><Button>Create Route</Button></Link>
        </Card>
      )}
    </div>
  );
}
