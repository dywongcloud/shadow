"use client";

import Link from "next/link";
import {
  Github, Search, GitBranch, CheckCheck, LayoutGrid, List,
  ChevronDown, ShieldCheck, CircleCheck, EyeOff, Star, Activity, X,
} from "lucide-react";
import { Card, Button, Input } from "@/components/ui";
import { GlobeEmptyState } from "@/components/globe";
import { FrameworkLogo } from "@/components/framework-logo";
import { ProjectMenu } from "@/components/project-menu";
import {
  usePoll, type Deployment, type BillingInfo, type LedgerEntry, type NotificationFeed,
} from "@/lib/api";
import { timeAgo } from "@/lib/utils";
import { deploymentHost, deploymentSelfAlias } from "@/lib/deploy-url";
import { RawPortsBadge } from "@/components/raw-port-connections";
import { useEffect, useMemo, useRef, useState } from "react";
import { Landing } from "@/components/landing";
import { useSettledAuth, resetAuthSettle } from "@/lib/auth-settle";

type View = "grid" | "list";

const usd = (cents: number) => `$${(cents / 100).toFixed(2)}`;

const clerkEnabled = !!process.env.NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY;

// The root route body (client): the public Shadow landing for signed-out
// visitors, the dashboard for signed-in users. In local (no-Clerk) mode it's just
// the dashboard. Rendered by the force-dynamic server shell in page.tsx. Auth is
// resolved CLIENT-side only (the layout's ClerkProvider has no `dynamic` prop,
// so SSR carries no request auth): server HTML and the first client render both
// show NEITHER branch — there is no hydration mismatch, just one client-side
// commit when clerk-js settles.
//
// GUARDED, not raw `<SignedIn>/<SignedOut>`: this route flips the ENTIRE page
// between two different applications, so it renders from `useSettledAuth()` —
// a committed, debounced, flip-BOUNDED view of Clerk's auth state (see
// lib/auth-settle.ts). A flapping auth signal (the mobile/ITP third-party
// cookie handshake failure) therefore cannot thrash landing↔dashboard: fast
// flaps are absorbed with zero paints, slow oscillation is cut off after a
// bounded number of flips by a stable, honest degraded state with a working
// sign-in affordance. The first loaded sample commits with no debounce, so the
// normal case paints as fast as the old control components did.
export function HomeClient() {
  if (!clerkEnabled) return <Dashboard />;
  // Separate subtree so `useSettledAuth` (which needs ClerkProvider) is never
  // called in local/no-Clerk mode — same split the old control components had.
  return <GuardedHome />;
}

function GuardedHome() {
  const view = useSettledAuth();
  if (view === "in") return <Dashboard />;
  if (view === "out") return <Landing />;
  if (view === "degraded") return <AuthDegraded />;
  return <AuthResolving />;
}

/** Neutral third state while auth is genuinely unresolved: deliberately
 *  NEITHER the Landing NOR the Dashboard, so an unsettled auth signal can
 *  never paint the wrong app even once. On the force-dynamic home route
 *  Clerk's SSR resolves auth before first paint, so a normal visitor never
 *  sees this; it exists for the pathological path (slow/blocked clerk-js),
 *  and is time-bounded by the guard (RESOLVE_TIMEOUT_MS → signed-out). */
function AuthResolving() {
  return (
    <div aria-busy="true" className="flex min-h-[70vh] items-center justify-center">
      <span className="h-8 w-8 animate-pulse rounded-full border border-border bg-subtle" />
    </div>
  );
}

/** Terminal state after the guard's flip bound tripped: the auth signal on
 *  this browser is flapping (e.g. iOS blocking the third-party session
 *  handshake), so instead of endlessly re-painting we settle on the stable
 *  signed-out Landing plus an honest, actionable explanation. "Try again" is
 *  the ONLY way out (explicit user action — automatic retry would re-enter
 *  the loop this state exists to end). */
function AuthDegraded() {
  return (
    <>
      <Landing />
      <div
        role="alert"
        className="fixed inset-x-0 bottom-0 z-[200] border-t border-amber-500/40 bg-[#141519]/95 px-4 py-3 text-sm text-zinc-200 backdrop-blur"
      >
        <div className="mx-auto flex max-w-3xl flex-wrap items-center justify-center gap-x-4 gap-y-2 text-center">
          <span>
            We couldn&apos;t keep you signed in on this browser — it may be blocking the
            cookies sign-in needs (common in private browsing on iPhone). You can keep
            browsing signed out, or try again.
          </span>
          <span className="flex shrink-0 items-center gap-4">
            <Link href="/sign-in" className="font-semibold text-white underline underline-offset-2">
              Sign in
            </Link>
            <button
              type="button"
              onClick={resetAuthSettle}
              className="font-semibold text-white underline underline-offset-2"
            >
              Try again
            </button>
          </span>
        </div>
      </div>
    </>
  );
}

function Dashboard() {
  const { data: deps, refresh } = usePoll<Deployment[]>("/deployments", 3000);
  const { data: billing } = usePoll<BillingInfo>("/v1/billing", 8000);
  const { data: ledger } = usePoll<LedgerEntry[]>("/v1/billing/ledger", 8000);
  const { data: notifications } = usePoll<NotificationFeed>("/v1/notifications", 8000);
  const [q, setQ] = useState("");
  const [page, setPage] = useState(0);
  // Lazy initializer: reads the saved card/list preference synchronously on
  // mount. SSR-safe — `typeof window === "undefined"` on the server matches
  // the "grid" default, so no hydration mismatch.
  const [view, setView] = useState<View>(() => {
    const v = typeof window !== "undefined" ? localStorage.getItem("oe_projects_view") : null;
    return v === "grid" || v === "list" ? v : "grid";
  });
  function chooseView(v: View) {
    setView(v);
    if (typeof window !== "undefined") localStorage.setItem("oe_projects_view", v);
  }

  // Favorites (starred projects) — persisted client-side; collapsible section.
  // Lazy initializers: read localStorage synchronously on mount. SSR-safe —
  // `typeof window === "undefined"` on the server matches each default
  // ([] / false), so no hydration mismatch.
  const [favorites, setFavorites] = useState<string[]>(() => {
    if (typeof window === "undefined") return [];
    try { const f = localStorage.getItem("oe_favorites"); return f ? JSON.parse(f) : []; } catch { return []; }
  });
  // COLLAPSED by default for everyone; only an explicit expand (persisted as
  // oe_fav_open="1") re-opens it on later visits. The old polarity (default
  // open, only "0" collapsed) had every end user start with the section
  // expanded whether or not they ever used favorites.
  const [favOpen, setFavOpen] = useState(() => typeof window !== "undefined" && localStorage.getItem("oe_fav_open") === "1");
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
            placeholder="Search Projects…  (⌘K for commands)"
            value={q}
            onChange={(e) => { setQ(e.target.value); setPage(0); }}
            className="pl-9 pr-14"
          />
          {/* Doubles as the platform command bar — open with ⌘K or by clicking. */}
          <button
            type="button"
            onClick={() => window.dispatchEvent(new Event("open-command-bar"))}
            title="Open command bar (⌘K)"
            className="absolute right-2 top-1/2 -translate-y-1/2 rounded border border-border bg-subtle px-1.5 py-0.5 font-mono text-[11px] text-muted hover:text-fg"
          >
            ⌘K
          </button>
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
          {favOpen && deps !== null && (
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

          {deps === null ? (
            // Data not loaded yet (initial mount OR a team/account switch that
            // cleared the previous tenant's data). Show a skeleton, NOT the empty
            // state — otherwise "Deploy your first project" flashes for a frame
            // before the new tenant's cards arrive.
            <div className={view === "grid" ? "grid grid-cols-1 gap-4 sm:grid-cols-2" : "overflow-hidden rounded-xl border border-border bg-card"}>
              {Array.from({ length: view === "grid" ? 4 : 6 }).map((_, i) => (
                <div key={i} className={view === "grid" ? "h-28 animate-pulse rounded-xl border border-border bg-card" : "h-14 animate-pulse border-b border-border bg-card last:border-0"} />
              ))}
            </div>
          ) : !list.length ? (
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
  // Date.now() read directly during render is flagged as impure by the
  // react-compiler-era rule, but this is a "current time for a countdown
  // display" case: the value only needs to be approximately fresh (it
  // re-renders every billing poll, 8s via usePoll) and turning it into real
  // state would require a ticking interval, changing behavior beyond this
  // display. Deliberate, targeted exception.
  // eslint-disable-next-line react-hooks/purity -- Date.now() for an approximate countdown display; see comment above
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
                <FrameworkLogo framework={d.framework} kind={d.kind} className="h-7 w-7 shrink-0" />
                <div className="min-w-0 flex-1">
                  <div className="truncate font-medium">{d.project}</div>
                  <div className="truncate text-xs text-muted">{(() => { const a = deploymentSelfAlias(d); return a ? deploymentHost(a) : "preview URL pending"; })()}</div>
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

/** Absolute short date like Vercel's project list: "May 28" for the current
 *  year, "2/22/25" for a prior year. (Vercel shows absolute dates here, not a
 *  relative "Xh ago".) */
function shortDate(ms: number): string {
  if (!ms) return "";
  const d = new Date(ms);
  const sameYear = d.getFullYear() === new Date().getFullYear();
  return d.toLocaleDateString(
    "en-US",
    sameYear
      ? { month: "short", day: "numeric" }
      : { month: "numeric", day: "numeric", year: "2-digit" }
  );
}

/** Vercel-style deployment-status ring: a light track with a colored progress
 *  arc, and a double-check (or an activity pulse while building / a cross on
 *  error) in the center. */
function StatusRing({ state }: { state: string }) {
  const tone =
    state === "ready" ? "text-blue-500"
    : state === "building" || state === "queued" ? "text-amber-500"
    : state === "error" ? "text-red-500"
    : "text-muted";
  // Arc length on a 0–100 path scale: a small accent for steady states, a
  // longer sweep while building.
  const arc = state === "building" || state === "queued" ? "60 100" : "26 100";
  return (
    <span className={`relative flex h-8 w-8 shrink-0 items-center justify-center ${tone}`} title={state}>
      <svg viewBox="0 0 36 36" className="absolute inset-0 h-full w-full" fill="none" aria-hidden>
        <circle cx="18" cy="18" r="15.5" className="stroke-border" strokeWidth="2.5" />
        <circle
          cx="18" cy="18" r="15.5"
          stroke="currentColor" strokeWidth="2.5" strokeLinecap="round"
          pathLength={100} strokeDasharray={arc} transform="rotate(-90 18 18)"
        />
      </svg>
      {state === "building" || state === "queued" ? (
        <Activity className="h-3.5 w-3.5 text-muted" />
      ) : state === "error" ? (
        <X className="h-3.5 w-3.5 text-red-500" />
      ) : (
        <CheckCheck className="h-3.5 w-3.5 text-secondary" />
      )}
    </span>
  );
}

/** Card view — the whole card is a link (stretched-link pattern) while the
 *  status ring + ⋯ menu stay clickable on top. */
function StarBtn({ isFav, onToggle, className }: { isFav?: boolean; onToggle?: () => void; className?: string }) {
  if (!onToggle) return null;
  return (
    <button
      onClick={(e) => { e.preventDefault(); e.stopPropagation(); onToggle(); }}
      aria-label={isFav ? "Unfavorite" : "Favorite"}
      className={`pointer-events-auto flex h-7 w-7 items-center justify-center rounded-md hover:bg-subtle ${isFav ? "text-amber-400" : "text-muted hover:text-fg"} ${className ?? ""}`}
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
            <FrameworkLogo framework={p.framework} kind={p.kind} />
            <div className="min-w-0">
              <div className="truncate font-semibold">{p.project}</div>
              <div className="flex min-w-0 items-center gap-1.5">
                <div className={`truncate text-sm ${p.production ? "text-secondary" : "text-muted"}`}>
                  {p.production ? deploymentHost(p.alias) : "No Production Deployment"}
                </div>
                <RawPortsBadge deployment={p} />
              </div>
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
              <span>{shortDate(p.created_at_ms)}</span>
              <span>on</span>
              <GitBranch className="h-3 w-3" />
              {p.git!.branch || "main"}
            </div>
          </>
        ) : (
          <>
            <Link href="/new" className="pointer-events-auto relative z-20 mt-3 inline-block text-sm font-medium text-link hover:underline">
              Connect Git Repository
            </Link>
            <div className="mt-3 text-xs text-muted">{shortDate(p.created_at_ms)}</div>
          </>
        )}
      </div>
    </Card>
  );
}

/** List/row view — a full-width clickable row like Vercel's project list. */
function ProjectRow({ p, onChange, isFav, onToggleFav }: { p: Deployment; onChange?: () => void; isFav?: boolean; onToggleFav?: () => void }) {
  const hasGit = !!p.git;
  const isProd = p.production;
  const date = shortDate(p.created_at_ms);
  return (
    <div className="group relative flex items-center gap-4 border-b border-border px-4 py-3.5 transition-colors last:border-0 hover:bg-subtle/50">
      <Link
        href={`/projects/${encodeURIComponent(p.project)}`}
        aria-label={p.project}
        className="absolute inset-0 z-0"
      />
      <div className="pointer-events-none relative z-10 flex flex-1 items-center gap-4">
        <FrameworkLogo framework={p.framework} kind={p.kind} />
        {/* Name + production domain (or "No Production Deployment"). */}
        <div className="min-w-0 shrink-0 basis-56 lg:basis-72">
          <div className="truncate font-semibold">{p.project}</div>
          <div className="flex min-w-0 items-center gap-1.5">
            <div className={`truncate text-xs ${isProd ? "text-secondary" : "text-muted"}`}>
              {isProd ? deploymentHost(p.alias) : "No Production Deployment"}
            </div>
            <RawPortsBadge deployment={p} />
          </div>
        </div>
        {/* Deploy info: commit message / connect-git / status (top) + date (bottom). */}
        <div className="hidden min-w-0 flex-1 flex-col justify-center md:flex">
          {hasGit ? (
            <div className="truncate text-sm">{p.git!.commit_message || "Latest deployment"}</div>
          ) : isProd ? (
            <Link href="/new" className="pointer-events-auto relative z-20 w-fit text-sm font-medium text-link hover:underline">
              Connect Git Repository
            </Link>
          ) : null}
          <div className="mt-0.5 flex items-center gap-1.5 truncate text-xs text-muted">
            <span className="shrink-0">{date}</span>
            {hasGit && (
              <>
                <span className="shrink-0">on</span>
                <GitBranch className="h-3 w-3 shrink-0" />
                <span className="truncate">{p.git!.branch || "main"}</span>
              </>
            )}
          </div>
        </div>
        {/* GitHub repository badge. */}
        {hasGit && (
          <span className="pointer-events-none hidden max-w-[200px] items-center gap-1.5 truncate rounded-md bg-subtle px-2 py-1 text-xs text-secondary lg:inline-flex">
            <Github className="h-3.5 w-3.5 shrink-0" />
            <span className="truncate">{repoLabel(p.git!.repo_url)}</span>
          </span>
        )}
        {/* Actions: star (hover-reveal, like Vercel's hidden default) · status · ⋯ */}
        <div className="pointer-events-auto z-20 ml-auto flex shrink-0 items-center gap-1.5">
          <StarBtn isFav={isFav} onToggle={onToggleFav} className={`transition-opacity focus:opacity-100 group-hover:opacity-100 ${isFav ? "" : "opacity-0"}`} />
          <StatusRing state={p.state} />
          <ProjectMenu project={p.project} alias={p.alias} onChange={onChange} />
        </div>
      </div>
    </div>
  );
}
