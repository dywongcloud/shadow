"use client";

import Link from "next/link";
import {
  Github, Search, GitBranch, CheckCheck, LayoutGrid, List,
  ChevronDown, ShieldCheck, CircleCheck, EyeOff, Star,
} from "lucide-react";
import { Card, Button, Input, Triangle } from "@/components/ui";
import { GlobeEmptyState } from "@/components/globe";
import { ProjectMenu } from "@/components/project-menu";
import {
  usePoll, type Deployment, type BillingInfo, type LedgerEntry, type NotificationFeed,
} from "@/lib/api";
import { timeAgo } from "@/lib/utils";
import { deploymentHost } from "@/lib/deploy-url";
import { useEffect, useMemo, useRef, useState } from "react";

type View = "grid" | "list";

const usd = (cents: number) => `$${(cents / 100).toFixed(2)}`;

export default function OverviewPage() {
  const { data: deps, refresh } = usePoll<Deployment[]>("/deployments", 3000);
  const { data: billing } = usePoll<BillingInfo>("/v1/billing", 8000);
  const { data: ledger } = usePoll<LedgerEntry[]>("/v1/billing/ledger", 8000);
  const { data: notifications } = usePoll<NotificationFeed>("/v1/notifications", 8000);
  const [q, setQ] = useState("");
  const [page, setPage] = useState(0);
  const [view, setView] = useState<View>("grid");

  // Restore the saved card/list preference.
  useEffect(() => {
    const v = typeof window !== "undefined" ? localStorage.getItem("oe_projects_view") : null;
    if (v === "grid" || v === "list") setView(v);
  }, []);
  function chooseView(v: View) {
    setView(v);
    if (typeof window !== "undefined") localStorage.setItem("oe_projects_view", v);
  }

  // Favorites (starred projects) — persisted client-side; collapsible section.
  const [favorites, setFavorites] = useState<string[]>([]);
  const [favOpen, setFavOpen] = useState(true);
  useEffect(() => {
    if (typeof window === "undefined") return;
    try { const f = localStorage.getItem("oe_favorites"); if (f) setFavorites(JSON.parse(f)); } catch { /* ignore */ }
    if (localStorage.getItem("oe_fav_open") === "0") setFavOpen(false);
  }, []);
  function toggleFav(project: string) {
    setFavorites((prev) => {
      const next = prev.includes(project) ? prev.filter((p) => p !== project) : [...prev, project];
      if (typeof window !== "undefined") localStorage.setItem("oe_favorites", JSON.stringify(next));
      return next;
    });
  }
  function toggleFavOpen() {
    setFavOpen((o) => {
      const n = !o;
      if (typeof window !== "undefined") localStorage.setItem("oe_fav_open", n ? "1" : "0");
      return n;
    });
  }

  // Group deployments by project (latest per project = the project card).
  const projects = new Map<string, Deployment>();
  for (const d of deps ?? []) if (!projects.has(d.project)) projects.set(d.project, d);
  const list = Array.from(projects.values()).filter((p) =>
    p.project.toLowerCase().includes(q.toLowerCase())
  );

  const favoriteProjects = list.filter((p) => favorites.includes(p.project));

  // Paginate so large fleets stay navigable.
  const PAGE = view === "list" ? 12 : 6;
  const pageCount = Math.max(1, Math.ceil(list.length / PAGE));
  const safePage = Math.min(page, pageCount - 1);
  const shown = list.slice(safePage * PAGE, safePage * PAGE + PAGE);

  return (
    <div className="pb-24">
      {/* Toolbar */}
      <div className="mb-6 flex items-center gap-2">
        <div className="relative flex-1">
          <Search className="absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted" />
          <Input
            placeholder="Search Projects…"
            value={q}
            onChange={(e) => { setQ(e.target.value); setPage(0); }}
            className="pl-9"
          />
        </div>
        <div className="flex items-center rounded-md border border-border-strong p-0.5">
          <ViewButton active={view === "grid"} onClick={() => chooseView("grid")} label="Card view">
            <LayoutGrid className="h-4 w-4" />
          </ViewButton>
          <ViewButton active={view === "list"} onClick={() => chooseView("list")} label="List view">
            <List className="h-4 w-4" />
          </ViewButton>
        </div>
        <AddNewMenu />
      </div>

      {/* Left column slimmed ~15% (340 → 290). */}
      <div className="grid grid-cols-1 gap-6 lg:grid-cols-[290px_1fr]">
        {/* LEFT: Vercel-style dashboard boxes */}
        <div className="order-2 flex flex-col gap-6 lg:order-1">
          <UsageBox billing={billing} ledger={ledger ?? []} />
          <AlertsBox notifications={notifications} />
          <RecentPreviewsBox deps={deps ?? []} />
        </div>

        {/* RIGHT: projects */}
        <div className="order-1 lg:order-2">
          {/* Your Favorites — collapsible, aligned adjacent to Usage. */}
          <button onClick={toggleFavOpen} className="mb-3 flex items-center gap-1.5 text-sm font-semibold">
            <ChevronDown className={`h-4 w-4 transition-transform ${favOpen ? "" : "-rotate-90"}`} /> Your Favorites
          </button>
          {favOpen && (
            favoriteProjects.length ? (
              <div className="mb-6 grid grid-cols-1 gap-4 sm:grid-cols-2">
                {favoriteProjects.map((p) => (
                  <ProjectCard key={`fav-${p.id}`} p={p} onChange={refresh} isFav onToggleFav={() => toggleFav(p.project)} />
                ))}
              </div>
            ) : (
              <Card className="mb-6 p-6 text-center text-sm text-secondary">
                Star a project (the ☆ on a card) to pin it to your favorites.
              </Card>
            )
          )}

          {!list.length ? (
            <Card className="overflow-hidden p-8 text-center">
              <GlobeEmptyState title="Deploy your first project" desc="Import a Git repository or a Dockerfile to deploy across your global mesh. Use “New Project” above to get started." />
            </Card>
          ) : view === "grid" ? (
            <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
              {shown.map((p) => (
                <ProjectCard key={p.id} p={p} onChange={refresh} isFav={favorites.includes(p.project)} onToggleFav={() => toggleFav(p.project)} />
              ))}
            </div>
          ) : (
            <div className="overflow-hidden rounded-xl border border-border bg-card">
              {shown.map((p) => (
                <ProjectRow key={p.id} p={p} onChange={refresh} isFav={favorites.includes(p.project)} onToggleFav={() => toggleFav(p.project)} />
              ))}
            </div>
          )}

          {pageCount > 1 && (
            <div className="mt-5 flex items-center justify-between text-sm">
              <span className="text-muted">
                {list.length} project{list.length === 1 ? "" : "s"} · page {safePage + 1} of {pageCount}
              </span>
              <div className="flex gap-2">
                <Button variant="outline" disabled={safePage === 0} onClick={() => setPage(safePage - 1)}>Previous</Button>
                <Button variant="outline" disabled={safePage >= pageCount - 1} onClick={() => setPage(safePage + 1)}>Next</Button>
              </div>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}

/* ---------- Vercel-style dashboard boxes (left column) ---------- */

function BoxLabel({ children }: { children: React.ReactNode }) {
  return <div className="mb-2 text-sm font-semibold">{children}</div>;
}

/** Usage box — real billing data (included credit, on-demand, line items),
 *  collapsible like Vercel's. */
function UsageBox({ billing, ledger }: { billing: BillingInfo | null; ledger: LedgerEntry[] }) {
  const [expanded, setExpanded] = useState(false);
  const acc = billing?.account;
  const days = acc ? Math.max(0, Math.ceil((acc.period_end_ms - Date.now()) / 86_400_000)) : 0;
  const pct = acc && acc.included_cents ? Math.min(1, acc.used_cents / acc.included_cents) : 0;
  const onDemand = acc ? Math.max(0, -acc.balance_cents) : 0;

  // Real line items: charges grouped by description.
  const items = useMemo(() => {
    const m = new Map<string, number>();
    for (const e of ledger) if (e.kind === "charge") m.set(e.note, (m.get(e.note) || 0) + Math.abs(e.amount_cents));
    return Array.from(m, ([name, cents]) => ({ name, cents })).sort((a, b) => b.cents - a.cents);
  }, [ledger]);
  const shown = expanded ? items : items.slice(0, 2);

  return (
    <div>
      <BoxLabel>Usage</BoxLabel>
      <Card className="p-0">
        <div className="flex items-center justify-between px-4 py-3">
          <span className="text-sm font-semibold">{days} day{days === 1 ? "" : "s"} remaining in cycle</span>
          <Link href="/billing"><Button variant="outline" className="px-2.5 py-1 text-xs">Billing</Button></Link>
        </div>
        <div className="px-4 pb-3">
          <div className="flex items-center justify-between text-xs text-secondary">
            <span>Included Credit</span>
            <span>On-Demand Charges</span>
          </div>
          <div className="mt-0.5 flex items-center justify-between text-sm font-medium">
            <span className="tabular-nums">{usd(acc?.used_cents ?? 0)} / {usd(acc?.included_cents ?? 0)}</span>
            <span className="flex items-center gap-1 tabular-nums"><ShieldCheck className="h-3.5 w-3.5 text-muted" /> {usd(onDemand)}</span>
          </div>
          <div className="mt-2 h-1.5 w-full overflow-hidden rounded-full bg-subtle">
            <div className="h-full rounded-full" style={{ width: `${Math.max(4, pct * 100)}%`, background: "linear-gradient(90deg,#0070f3,#7c3aed)" }} />
          </div>
        </div>
        {items.length > 0 && (
          <div className="border-t border-border">
            {shown.map((it) => (
              <div key={it.name} className="flex items-center justify-between border-b border-border px-4 py-2.5 text-sm last:border-0">
                <span className="truncate">{it.name}</span>
                <span className="tabular-nums text-secondary">{usd(it.cents)}</span>
              </div>
            ))}
            {items.length > 2 && (
              <button onClick={() => setExpanded((e) => !e)} className="flex w-full items-center justify-center border-t border-border py-2 text-muted hover:text-fg">
                <ChevronDown className={`h-4 w-4 transition-transform ${expanded ? "rotate-180" : ""}`} />
              </button>
            )}
          </div>
        )}
        {items.length === 0 && (
          <div className="border-t border-border px-4 py-4 text-center text-xs text-muted">No on-demand usage this cycle yet.</div>
        )}
      </Card>
    </div>
  );
}

/** Alerts box — real anomalies from the notifications feed. */
function AlertsBox({ notifications }: { notifications: NotificationFeed | null }) {
  const anomalies = (notifications?.items ?? []).filter((n) => !n.archived && (n.category === "anomaly" || n.category === "usage"));
  return (
    <div>
      <BoxLabel>Alerts</BoxLabel>
      <Card className="p-5">
        {anomalies.length === 0 ? (
          <div className="flex items-center justify-center gap-2 py-8 text-sm text-secondary">
            <CircleCheck className="h-4 w-4" /> No recent anomalies detected
          </div>
        ) : (
          <div className="flex flex-col gap-3">
            {anomalies.slice(0, 5).map((a) => (
              <div key={a.id} className="flex items-start gap-2 text-sm">
                <span className={`mt-1 h-2 w-2 shrink-0 rounded-full ${a.severity === "error" ? "bg-red-500" : "bg-amber-500"}`} />
                <div className="min-w-0 flex-1">
                  <div className="truncate text-secondary">{a.message}</div>
                  <div className="text-xs text-muted">{timeAgo(a.ts_ms)} ago</div>
                </div>
              </div>
            ))}
          </div>
        )}
      </Card>
    </div>
  );
}

/** Recent Previews box — real preview (non-production) deployments. */
function RecentPreviewsBox({ deps }: { deps: Deployment[] }) {
  const previews = deps
    .filter((d) => !d.production)
    .sort((a, b) => b.created_at_ms - a.created_at_ms)
    .slice(0, 5);
  return (
    <div>
      <BoxLabel>Recent Previews</BoxLabel>
      <Card className="p-5">
        {previews.length === 0 ? (
          <div className="flex flex-col items-center gap-3 py-6 text-center">
            <span className="flex h-9 w-9 items-center justify-center rounded-lg border border-border bg-subtle text-muted"><EyeOff className="h-4 w-4" /></span>
            <p className="text-sm text-secondary">Preview deployments that you have recently visited or created will appear here.</p>
          </div>
        ) : (
          <div className="flex flex-col gap-1">
            {previews.map((d) => (
              <Link key={d.id} href={`/projects/${encodeURIComponent(d.project)}`} className="flex items-center gap-3 rounded-md px-2 py-2 text-sm hover:bg-subtle">
                <Triangle className="h-7 w-7 shrink-0" />
                <div className="min-w-0 flex-1">
                  <div className="truncate font-medium">{d.project}</div>
                  <div className="truncate text-xs text-muted">{deploymentHost(d.alias)}</div>
                </div>
                <span className="shrink-0 text-xs text-muted">{timeAgo(d.created_at_ms)} ago</span>
              </Link>
            ))}
          </div>
        )}
      </Card>
    </div>
  );
}

/** Vercel-style "Add New…" split dropdown. */
function AddNewMenu() {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const f = (e: MouseEvent) => { if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false); };
    document.addEventListener("mousedown", f);
    return () => document.removeEventListener("mousedown", f);
  }, []);
  const items = [
    { label: "Project", href: "/new" },
    { label: "Domain", href: "/domains" },
    { label: "Storage", href: "/storage" },
    { label: "Integration", href: "/integrations" },
    { label: "Team Member", href: "/teams" },
  ];
  return (
    <div className="relative" ref={ref}>
      <button onClick={() => setOpen((o) => !o)} className="flex items-center gap-2 rounded-md bg-accent px-3 py-1.5 text-sm font-medium text-accent-fg hover:opacity-90">
        Add New… <ChevronDown className={`h-3.5 w-3.5 transition-transform ${open ? "rotate-180" : ""}`} />
      </button>
      {open && (
        <div className="absolute right-0 top-full z-40 mt-1.5 w-48 overflow-hidden rounded-lg border border-border bg-card py-1 shadow-pop">
          {items.map((it) => (
            <Link key={it.label} href={it.href} onClick={() => setOpen(false)} className="block px-3 py-2 text-sm hover:bg-subtle">
              {it.label}
            </Link>
          ))}
        </div>
      )}
    </div>
  );
}

function ViewButton({ active, onClick, label, children }: { active: boolean; onClick: () => void; label: string; children: React.ReactNode }) {
  return (
    <button
      onClick={onClick}
      aria-label={label}
      aria-pressed={active}
      className={`flex h-7 w-7 items-center justify-center rounded transition-colors ${active ? "bg-subtle text-fg" : "text-muted hover:text-fg"}`}
    >
      {children}
    </button>
  );
}

function repoLabel(url: string): string {
  return url
    .replace(/^https?:\/\/(www\.)?github\.com\//, "")
    .replace(/^https?:\/\//, "")
    .replace(/\.git$/, "");
}

/** Circular deployment-status ring with a double-check, like Vercel's. */
function StatusRing({ state }: { state: string }) {
  const tone =
    state === "ready" ? "text-green border-green/40"
    : state === "building" ? "text-amber-500 border-amber-500/40"
    : state === "error" ? "text-red-500 border-red-500/40"
    : "text-muted border-border-strong";
  return (
    <span className={`flex h-8 w-8 items-center justify-center rounded-full border-2 ${tone}`}>
      <CheckCheck className="h-3.5 w-3.5" />
    </span>
  );
}

/** Card view — the whole card is a link (stretched-link pattern) while the
 *  status ring + ⋯ menu stay clickable on top. */
function StarBtn({ isFav, onToggle }: { isFav?: boolean; onToggle?: () => void }) {
  if (!onToggle) return null;
  return (
    <button
      onClick={(e) => { e.preventDefault(); e.stopPropagation(); onToggle(); }}
      aria-label={isFav ? "Unfavorite" : "Favorite"}
      className={`pointer-events-auto flex h-7 w-7 items-center justify-center rounded-md hover:bg-subtle ${isFav ? "text-amber-400" : "text-muted hover:text-fg"}`}
    >
      <Star className="h-4 w-4" fill={isFav ? "currentColor" : "none"} />
    </button>
  );
}

function ProjectCard({ p, onChange, isFav, onToggleFav }: { p: Deployment; onChange?: () => void; isFav?: boolean; onToggleFav?: () => void }) {
  const hasGit = !!p.git;
  return (
    <Card className="relative p-5 transition-shadow hover:shadow-pop">
      <Link
        href={`/projects/${encodeURIComponent(p.project)}`}
        aria-label={p.project}
        className="absolute inset-0 z-0 rounded-xl"
      />
      <div className="pointer-events-none relative z-10">
        <div className="flex items-start justify-between gap-2">
          <div className="flex min-w-0 items-center gap-3">
            <Triangle />
            <div className="min-w-0">
              <div className="truncate font-semibold">{p.project}</div>
              <div className="truncate text-sm text-secondary">{deploymentHost(p.alias)}</div>
            </div>
          </div>
          <div className="pointer-events-auto z-20 flex shrink-0 items-center gap-1">
            <StarBtn isFav={isFav} onToggle={onToggleFav} />
            <StatusRing state={p.state} />
            <ProjectMenu project={p.project} alias={p.alias} onChange={onChange} />
          </div>
        </div>

        {hasGit ? (
          <>
            <div className="mt-3">
              <span className="inline-flex max-w-full items-center gap-1.5 truncate rounded-md bg-subtle px-2 py-1 text-xs text-secondary">
                <Github className="h-3.5 w-3.5 shrink-0" />
                <span className="truncate">{repoLabel(p.git!.repo_url)}</span>
              </span>
            </div>
            <p className="mt-3 truncate text-sm">{p.git!.commit_message || "Latest deployment"}</p>
            <div className="mt-3 flex items-center gap-1.5 text-xs text-muted">
              {timeAgo(p.created_at_ms)} ago on
              <GitBranch className="h-3 w-3" />
              {p.git!.branch || "main"}
            </div>
          </>
        ) : (
          <>
            <Link href="/new" className="pointer-events-auto relative z-20 mt-3 inline-block text-sm font-medium text-link hover:underline">
              Connect Git Repository
            </Link>
            <div className="mt-3 text-xs text-muted">{timeAgo(p.created_at_ms)} ago</div>
          </>
        )}
      </div>
    </Card>
  );
}

/** List/row view — a full-width clickable row like Vercel's list layout. */
function ProjectRow({ p, onChange, isFav, onToggleFav }: { p: Deployment; onChange?: () => void; isFav?: boolean; onToggleFav?: () => void }) {
  const hasGit = !!p.git;
  return (
    <div className="relative flex items-center gap-4 border-b border-border px-4 py-3.5 transition-colors last:border-0 hover:bg-subtle/50">
      <Link
        href={`/projects/${encodeURIComponent(p.project)}`}
        aria-label={p.project}
        className="absolute inset-0 z-0"
      />
      <div className="pointer-events-none relative z-10 flex flex-1 items-center gap-4">
        <Triangle />
        <div className="min-w-0 shrink-0 basis-52">
          <div className="truncate font-semibold">{p.project}</div>
          <div className="truncate text-xs text-secondary">{deploymentHost(p.alias)}</div>
        </div>
        <div className="hidden min-w-0 flex-1 md:block">
          {hasGit ? (
            <>
              <div className="truncate text-sm">{p.git!.commit_message || "Latest deployment"}</div>
              <div className="mt-0.5 flex items-center gap-1.5 text-xs text-muted">
                {timeAgo(p.created_at_ms)} ago on
                <GitBranch className="h-3 w-3" />
                {p.git!.branch || "main"}
              </div>
            </>
          ) : (
            <div className="text-sm text-muted">No Production Deployment · {timeAgo(p.created_at_ms)} ago</div>
          )}
        </div>
        {hasGit && (
          <span className="pointer-events-none hidden max-w-[220px] items-center gap-1.5 truncate rounded-md bg-subtle px-2 py-1 text-xs text-secondary lg:inline-flex">
            <Github className="h-3.5 w-3.5 shrink-0" />
            <span className="truncate">{repoLabel(p.git!.repo_url)}</span>
          </span>
        )}
        <div className="pointer-events-auto z-20 flex shrink-0 items-center gap-1">
          <StarBtn isFav={isFav} onToggle={onToggleFav} />
          <StatusRing state={p.state} />
          <ProjectMenu project={p.project} alias={p.alias} onChange={onChange} />
        </div>
      </div>
    </div>
  );
}
