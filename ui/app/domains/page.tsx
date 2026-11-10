"use client";

import { useEffect, useRef, useState } from "react";
import type { MouseEvent, ReactNode } from "react";
import Link from "next/link";
import {
  Search, ChevronDown, Plus, MoreHorizontal, Filter, RefreshCw, ShieldCheck,
  Globe2, Sparkles, Copy,
} from "lucide-react";
import { Button, Input } from "@/components/ui";
import { usePoll, useCan, type Deployment } from "@/lib/api";
import { deploymentHost } from "@/lib/deploy-url";
import { useProjectNames } from "@/lib/projects";
import { roleHint } from "@/lib/team-role";
import { copyText } from "@/lib/utils";
import { toast } from "@/components/toast";
import { DomainWizard } from "@/components/domain-wizard";

interface DomainEntry { project: string; domain: string; verify_status?: string }

interface Row {
  domain: string;
  project: string;
  kind: "custom" | "managed";
  verify_status: string;
}

type FilterKey = "all" | "pending" | "verified" | "third-party" | "managed";

const FILTERS: { key: FilterKey; label: string }[] = [
  { key: "all", label: "All domains" },
  { key: "pending", label: "Verification pending" },
  { key: "verified", label: "Verified" },
  { key: "third-party", label: "Third Party" },
  { key: "managed", label: "Platform-managed" },
];

const ABSENT = "—";
const NO_REGISTRAR_DATA = "Registration and renewal dates are not reported by this platform; it is not a registrar.";

function statusOf(row: Row): { glyph: ReactNode; text: string; filter: FilterKey } {
  if (row.kind === "managed") {
    return { glyph: <Globe2 className="h-3.5 w-3.5" aria-hidden="true" />, text: "Platform-managed", filter: "managed" };
  }
  if (row.verify_status === "pending") {
    return { glyph: <RefreshCw className="h-3.5 w-3.5" aria-hidden="true" />, text: "Verification pending", filter: "pending" };
  }
  if (row.verify_status === "verified") {
    return { glyph: <ShieldCheck className="h-3.5 w-3.5" aria-hidden="true" />, text: "Verified", filter: "verified" };
  }
  return { glyph: null, text: "Third Party", filter: "third-party" };
}

export default function DomainsPage() {
  const { data: deps } = usePoll<Deployment[]>("/deployments", 4000);
  const { data: custom, refresh } = usePoll<DomainEntry[]>("/v1/domains", 4000);
  const [wizardOpen, setWizardOpen] = useState(false);
  const [q, setQ] = useState("");
  const [projectFilter, setProjectFilter] = useState("");
  const [filterKey, setFilterKey] = useState<FilterKey>("all");
  const [filterOpen, setFilterOpen] = useState(false);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const searchBox = useRef<HTMLDivElement>(null);
  const selectAll = useRef<HTMLInputElement>(null);

  const projects = useProjectNames(4000);
  const mayAttach = useCan()("work");
  const attachHint = mayAttach ? undefined : roleHint("work");
  const deployedProjects = Array.from(new Set((deps ?? []).map((d) => d.project)));
  const managed: Row[] = deployedProjects.map((p) => ({
    domain: deploymentHost(`${p}.localhost`), project: p, kind: "managed", verify_status: "",
  }));
  const customRows: Row[] = (custom ?? []).map((c) => ({
    domain: c.domain, project: c.project, kind: "custom", verify_status: c.verify_status ?? "",
  }));
  const all: Row[] = [...customRows, ...managed];

  const rows = all.filter((d) => {
    if (projectFilter && d.project !== projectFilter) return false;
    if (q && !d.domain.toLowerCase().includes(q.toLowerCase())) return false;
    if (filterKey !== "all" && statusOf(d).filter !== filterKey) return false;
    return true;
  });

  const allSelected = rows.length > 0 && rows.every((r) => selected.has(r.domain));
  const someSelected = rows.some((r) => selected.has(r.domain));
  useEffect(() => {
    if (selectAll.current) selectAll.current.indeterminate = someSelected && !allSelected;
  }, [someSelected, allSelected]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "/" || e.metaKey || e.ctrlKey || e.altKey) return;
      const el = e.target as HTMLElement | null;
      if (el && (el.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(el.tagName))) return;
      e.preventDefault();
      searchBox.current?.querySelector("input")?.focus();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  function toggle(domain: string) {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(domain)) next.delete(domain);
      else next.add(domain);
      return next;
    });
  }

  function toggleAll() {
    setSelected(allSelected ? new Set() : new Set(rows.map((r) => r.domain)));
  }

  async function copy(value: string, e: MouseEvent) {
    if (await copyText(value)) toast("Copied", {});
    else toast("Copy failed", {});
    (e.currentTarget.closest("details") as HTMLDetailsElement | null)?.removeAttribute("open");
  }

  return (
    <div className="min-w-0">
      <div className="flex flex-col gap-3 border-b border-border pb-4 sm:grid sm:grid-cols-[1fr_auto_1fr] sm:items-center sm:gap-4">
        <label className="relative flex min-w-0 items-center self-start sm:w-fit">
          <span className="sr-only">Filter by project</span>
          <select
            value={projectFilter}
            onChange={(e) => setProjectFilter(e.target.value)}
            className="min-w-0 appearance-none rounded-md bg-transparent py-1 pl-1 pr-6 text-sm font-medium text-fg hover:bg-subtle focus:outline-none focus:ring-2 focus:ring-border"
          >
            <option value="">All Projects</option>
            {projects.map((p) => <option key={p} value={p}>{p}</option>)}
          </select>
          <ChevronDown className="pointer-events-none absolute right-1 h-3.5 w-3.5 text-muted" aria-hidden="true" />
        </label>

        <h1 className="text-center text-sm font-semibold tracking-tight text-fg sm:text-base">Team Domains</h1>

        <div className="flex min-w-0 flex-wrap items-center justify-start gap-2 sm:justify-end">
          <Button onClick={() => setWizardOpen(true)} disabled={!mayAttach} title={attachHint}><Plus className="h-4 w-4" /> Create New</Button>
          <button
            type="button"
            disabled
            title="An AI domain agent is not available on this platform; use Create New to attach a domain"
            className="inline-flex items-center gap-2 rounded-md border border-border px-3 py-1.5 text-sm font-medium text-fg opacity-60"
          >
            <Sparkles className="h-4 w-4" aria-hidden="true" /> Agent
          </button>
        </div>
      </div>

      <div className="flex flex-wrap items-center gap-2 py-4">
        <Button
          variant="outline"
          aria-label="Filter domains"
          aria-expanded={filterOpen}
          title="Filter domains"
          onClick={() => setFilterOpen((v) => !v)}
          className="h-10 w-10 shrink-0 p-0"
        >
          <Filter className="h-4 w-4" />
        </Button>
        <div ref={searchBox} className="relative min-w-0 flex-1 basis-64">
          <Search className="pointer-events-none absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted" aria-hidden="true" />
          <Input
            placeholder="Search any domain"
            aria-label="Search any domain"
            value={q}
            onChange={(e) => setQ(e.target.value)}
            className="h-10 pl-9 pr-10"
          />
          <kbd className="pointer-events-none absolute right-2.5 top-1/2 hidden h-5 min-w-5 -translate-y-1/2 items-center justify-center rounded border border-border bg-subtle px-1 font-mono text-[11px] text-muted sm:flex">/</kbd>
        </div>
        <Button variant="outline" className="h-10 shrink-0" disabled={!mayAttach} title={attachHint} onClick={() => setWizardOpen(true)}>Connect External</Button>
        <Button variant="outline" className="h-10 shrink-0" disabled={!mayAttach} title={attachHint} onClick={() => setWizardOpen(true)}>Transfer In</Button>
        <Button
          disabled
          title="Domain purchase is not available: this platform is not a registrar. Use Create New to connect a domain you already own."
          className="h-10 shrink-0 disabled:opacity-100 disabled:cursor-not-allowed"
        >
          Buy
        </Button>
      </div>

      {filterOpen && (
        <div className="mb-4 flex flex-wrap items-center gap-2">
          {FILTERS.map((f) => (
            <button
              key={f.key}
              type="button"
              onClick={() => setFilterKey(f.key)}
              aria-pressed={filterKey === f.key}
              className={`rounded-md border px-2.5 py-1 text-xs font-medium transition-colors ${
                filterKey === f.key ? "border-fg bg-subtle text-fg" : "border-border text-secondary hover:bg-subtle"
              }`}
            >
              {f.label}
            </button>
          ))}
        </div>
      )}

      <div className="min-w-0 overflow-hidden rounded-lg border border-border bg-card">
        <div className="flex min-w-0 items-center justify-between gap-3 border-b border-border bg-subtle/60 px-4 py-3">
          <label className="flex min-w-0 items-center gap-3 text-sm text-secondary">
            <input
              ref={selectAll}
              type="checkbox"
              checked={allSelected}
              onChange={toggleAll}
              aria-label="Select all domains"
              className="h-4 w-4 shrink-0 rounded accent-fg [color-scheme:light]"
            />
            <span className="min-w-0 break-words">{selected.size ? `${selected.size} selected` : "Select all"}</span>
          </label>
          <details className="group relative shrink-0">
            <summary className="tap-44 flex h-8 w-8 cursor-pointer list-none items-center justify-center rounded-md text-muted hover:bg-subtle [&::-webkit-details-marker]:hidden">
              <MoreHorizontal className="h-4 w-4" />
              <span className="sr-only">Bulk actions</span>
            </summary>
            <div className="absolute right-0 top-full z-20 mt-1 w-48 rounded-md border border-border bg-card py-1 shadow-pop">
              <button type="button" onClick={toggleAll} className="block w-full px-3 py-2 text-left text-sm hover:bg-subtle">
                {allSelected ? "Clear selection" : "Select all"}
              </button>
              <button type="button" onClick={(e) => copy(Array.from(selected).join("\n"), e)} className="block w-full px-3 py-2 text-left text-sm hover:bg-subtle">
                Copy selected
              </button>
              <button type="button" onClick={() => { setSelected(new Set()); refresh(); }} className="block w-full px-3 py-2 text-left text-sm hover:bg-subtle">
                Clear and refresh
              </button>
            </div>
          </details>
        </div>

        <div className="divide-y divide-border">
          {rows.map((row) => {
            const status = statusOf(row);
            const detailHref = `/domains/${encodeURIComponent(row.domain)}`;
            return (
              <div key={`${row.kind}:${row.domain}`} className="flex min-w-0 flex-col gap-3 px-4 py-5 transition-colors hover:bg-subtle/40 sm:flex-row sm:items-center sm:justify-between sm:gap-4 sm:py-6">
                <div className="flex min-w-0 items-start gap-3">
                  <input
                    type="checkbox"
                    checked={selected.has(row.domain)}
                    onChange={() => toggle(row.domain)}
                    aria-label={`Select ${row.domain}`}
                    className="mt-1 h-4 w-4 shrink-0 rounded accent-fg [color-scheme:light]"
                  />
                  <div className="min-w-0">
                    <Link href={detailHref} className="block min-w-0 break-all text-[15px] font-semibold leading-tight text-fg hover:underline">
                      {row.domain}
                    </Link>
                    <div className="mt-1.5 flex min-w-0 items-center gap-1.5 text-sm leading-tight text-muted">
                      {status.glyph}
                      <span className="min-w-0 break-words">{status.text}</span>
                      <span className="shrink-0 text-muted" aria-hidden="true">·</span>
                      <span className="min-w-0 break-all">{row.project}</span>
                    </div>
                  </div>
                </div>

                <div className="flex shrink-0 items-center gap-3 self-end sm:gap-4 sm:self-auto">
                  <span className="w-16 text-right text-sm tabular-nums text-muted" title={NO_REGISTRAR_DATA} aria-label="Registration date not reported">
                    {ABSENT}
                  </span>
                  <span
                    className="flex h-7 w-7 shrink-0 items-center justify-center rounded-full border border-border bg-subtle text-[11px] font-semibold uppercase text-secondary"
                    title={`Project: ${row.project}`}
                    aria-hidden="true"
                  >
                    {row.project.slice(0, 1) || "D"}
                  </span>
                  <details className="group relative shrink-0">
                    <summary className="tap-44 flex h-8 w-8 cursor-pointer list-none items-center justify-center rounded-md text-muted hover:bg-subtle [&::-webkit-details-marker]:hidden">
                      <MoreHorizontal className="h-4 w-4" />
                      <span className="sr-only">Actions for {row.domain}</span>
                    </summary>
                    <div className="absolute right-0 top-full z-20 mt-1 w-48 rounded-md border border-border bg-card py-1 shadow-pop">
                      <Link href={detailHref} className="block px-3 py-2 text-sm hover:bg-subtle">Open details</Link>
                      <button type="button" onClick={(e) => copy(row.domain, e)} className="flex w-full items-center gap-2 px-3 py-2 text-left text-sm hover:bg-subtle">
                        <Copy className="h-3.5 w-3.5" aria-hidden="true" /> Copy domain
                      </button>
                      <button type="button" onClick={() => refresh()} className="block w-full px-3 py-2 text-left text-sm hover:bg-subtle">Refresh list</button>
                    </div>
                  </details>
                </div>
              </div>
            );
          })}
          {!rows.length && (
            <div className="px-4 py-10 text-center text-sm text-secondary">
              No domains match — deploy a project, then attach a domain.
            </div>
          )}
        </div>
      </div>

      <p className="mt-3 flex min-w-0 items-start gap-1.5 text-xs text-muted">
        <span className="min-w-0 break-words">{NO_REGISTRAR_DATA}</span>
      </p>

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
