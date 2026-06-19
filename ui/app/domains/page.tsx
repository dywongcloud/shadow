"use client";

import { useState } from "react";
import Link from "next/link";
import { Search, Filter, RotateCw, Plus, ChevronRight } from "lucide-react";
import { Card, Button, Input, Badge } from "@/components/ui";
import { apiSend, usePoll, type Deployment } from "@/lib/api";
import { deploymentHost } from "@/lib/deploy-url";

interface DomainEntry { project: string; domain: string }

export default function DomainsPage() {
  const { data: deps } = usePoll<Deployment[]>("/deployments", 4000);
  const { data: custom, refresh } = usePoll<DomainEntry[]>("/v1/domains", 4000);
  const [open, setOpen] = useState(false);
  const [domain, setDomain] = useState("");
  const [project, setProject] = useState("");
  const [err, setErr] = useState("");
  const [q, setQ] = useState("");

  const projects = Array.from(new Set((deps ?? []).map((d) => d.project)));
  // managed subdomains (one per project) + custom domains
  const managed = projects.map((p) => ({ domain: deploymentHost(`${p}.localhost`), project: p, kind: "managed" as const }));
  const customList = (custom ?? []).map((c) => ({ ...c, kind: "custom" as const }));
  const all = [...customList, ...managed].filter((d) => d.domain.toLowerCase().includes(q.toLowerCase()));

  async function add() {
    setErr("");
    if (!domain || !project) { setErr("Enter a domain and pick a project."); return; }
    try {
      await apiSend("POST", `/v1/projects/${project}/domains`, { domain });
      setOpen(false); setDomain(""); setProject(""); refresh();
    } catch (e) { setErr(String(e)); }
  }

  return (
    <div>
      <div className="mb-6 flex items-end justify-between">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">Domains</h1>
          <p className="mt-1.5 text-sm text-secondary">The domains you have access to through your account.</p>
        </div>
        <div className="flex gap-2">
          <Button variant="outline" onClick={() => setOpen(true)}>Add to project</Button>
          <Button>Buy</Button>
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
              <div className="flex items-center gap-1 text-xs text-secondary"><RotateCw className="h-3 w-3" /> {d.project} · auto-renews</div>
            </div>
            <div className="flex items-center gap-3"><Badge tone="green">Active</Badge><ChevronRight className="h-4 w-4 text-muted" /></div>
          </Link>
        ))}
        {!all.length && <div className="px-4 py-10 text-center text-sm text-secondary">No domains yet — deploy a project, then attach a domain.</div>}
      </Card>

      {open && (
        <div className="fixed inset-0 z-40 flex items-center justify-center bg-black/40 p-4" onClick={() => setOpen(false)}>
          <Card className="w-full max-w-md" onClick={(e) => e.stopPropagation()}>
            <h3 className="text-lg font-semibold">Add Domain</h3>
            <p className="mt-1 text-sm text-secondary">Enter the domain you want to add to your project.</p>
            <label className="mt-5 block text-sm font-medium">Domain</label>
            <Input className="mt-1.5" placeholder="example.com" value={domain} onChange={(e) => setDomain(e.target.value)} />
            <label className="mt-4 block text-sm font-medium">Project</label>
            <select value={project} onChange={(e) => setProject(e.target.value)} className="mt-1.5 w-full rounded-md border border-border bg-card px-3 py-2 text-sm focus:outline-none">
              <option value="">Select a project…</option>
              {projects.map((p) => <option key={p} value={p}>{p}</option>)}
            </select>
            {err ? <p className="mt-3 text-sm text-red-500">{err}</p> : null}
            <div className="mt-5 flex justify-between">
              <Button variant="outline" onClick={() => setOpen(false)}>Cancel</Button>
              <Button onClick={add}><Plus className="h-4 w-4" /> Continue</Button>
            </div>
          </Card>
        </div>
      )}
    </div>
  );
}
