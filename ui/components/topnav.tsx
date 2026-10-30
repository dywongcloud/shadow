"use client";

import Link from "next/link";
import { usePathname, useSearchParams } from "next/navigation";
import { Suspense, useEffect, useRef, useState } from "react";
import { ChevronsUpDown, ShieldHalf, Check, Plus, User, Settings, Building2, Workflow } from "lucide-react";
import { useOrganization, useOrganizationList, useClerk } from "@clerk/nextjs";
import { cn } from "@/lib/utils";
import { Triangle } from "@/components/ui";
import { ThemeToggle } from "@/components/theme-toggle";
import { Logo } from "@/components/logo";
import { NotificationBell } from "@/components/notifications";
import { RunNodeControl } from "@/components/run-node-control";
import { WithIdentity, type Identity } from "@/components/identity";
import { usePoll, switchTeam, mintSessionToken, type Team } from "@/lib/api";
import { useIsPlatformOwner } from "@/lib/owner";

const clerkOn = !!process.env.NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY;

const teamTabs = [
  { href: "/", label: "Projects" },
  { href: "/regions", label: "Regions" },
  { href: "/workflows", label: "Workflows" },
  { href: "/storage", label: "Storage" },
  { href: "/drive", label: "Drive" },
  { href: "/observability", label: "Observability" },
  { href: "/cdn", label: "CDN" },
  { href: "/integrations", label: "Integrations" },
  { href: "/domains", label: "Domains" },
  { href: "/firewall", label: "Firewall" },
  { href: "/network", label: "Constellation" },
  { href: "/usage", label: "Usage" },
  { href: "/billing", label: "Billing" },
  { href: "/settings", label: "Settings" },
];

/** Drilled-in tab context: once a project or deployment is selected its sub-tabs take the top
 *  tab bar and the team tabs collapse into the breadcrumb. `tabParam` is the reactive `?tab=`. */
type TabItem = { href: string; label: string; key: string };
function contextTabs(pathname: string, tabParam: string | null): { items: TabItem[]; activeKey: string } | null {
  const dep = pathname.match(/^\/deployments\/([^/]+)/);
  if (dep) {
    const id = dep[1];
    const items: TabItem[] = [
      { href: `/deployments/${id}`, label: "Overview", key: "overview" },
      { href: `/deployments/${id}?tab=logs`, label: "Build Logs", key: "logs" },
      { href: `/deployments/${id}?tab=workflows`, label: "Workflows", key: "workflows" },
    ];
    const activeKey = tabParam === "logs" ? "logs" : tabParam === "workflows" ? "workflows" : "overview";
    return { items, activeKey };
  }
  const proj = pathname.match(/^\/projects\/([^/]+)/);
  if (proj) {
    const p = proj[1];
    const base = `/projects/${p}`;
    const items: TabItem[] = [
      { href: `${base}?tab=overview`, label: "Overview", key: "overview" },
      { href: `${base}?tab=graph`, label: "Service Graph", key: "graph" },
      { href: `${base}?tab=workflows`, label: "Workflows", key: "workflows" },
      { href: `${base}?tab=resources`, label: "Resources", key: "resources" },
      { href: `${base}?tab=deployments`, label: "Deployments", key: "deployments" },
      { href: `${base}/logs`, label: "Logs", key: "logs" },
      { href: `${base}/sandboxes`, label: "Sandboxes", key: "sandboxes" },
      { href: `${base}/settings`, label: "Settings", key: "settings" },
    ];
    let activeKey = "overview";
    if (pathname.includes("/settings")) activeKey = "settings";
    else if (pathname.includes("/logs")) activeKey = "logs";
    else if (pathname.includes("/sandboxes")) activeKey = "sandboxes";
    else if (["graph", "workflows", "resources", "deployments"].includes(tabParam ?? "")) activeKey = tabParam!;
    return { items, activeKey };
  }
  return null;
}

/** Thin breadcrumb separator matching the brand slash. */
function Slash() {
  return <span className="px-1 text-2xl font-thin text-border-strong">/</span>;
}

export function TopNav() {
  const pathname = usePathname();
  const isOwner = useIsPlatformOwner();
  if (pathname.startsWith("/admin") || pathname.startsWith("/sign-in") || pathname.startsWith("/sign-up") || pathname.startsWith("/status") || pathname.startsWith("/docs")) return null;
  const isActive = (href: string) =>
    href === "/" ? pathname === "/" : pathname.startsWith(href);

  const projectSeg = pathname.startsWith("/projects/")
    ? decodeURIComponent(pathname.split("/")[2] ?? "")
    : "";
  const deploymentSeg = pathname.startsWith("/deployments/")
    ? decodeURIComponent(pathname.split("/")[2] ?? "")
    : "";
  const workflowsSeg = pathname === "/workflows" || pathname.startsWith("/workflows/");

  return (
    <header
      className="sticky top-0 z-30 border-b border-border bg-bg/85 backdrop-blur"
      style={{ paddingTop: "env(safe-area-inset-top)" }}
    >
      <div className="mx-auto flex h-[52px] max-w-[1400px] items-center justify-between px-4 sm:px-6">
        <div className="flex min-w-0 items-center gap-2 text-sm">
          <Link href="/" className="flex items-center"><Logo className="h-5 w-auto" /></Link>
          <Slash />
          <WithIdentity>{(id) => (clerkOn ? <ClerkTeamSwitcher identity={id} /> : <TeamSwitcher identity={id} />)}</WithIdentity>
          {projectSeg && (
            <>
              <Slash />
              <Link
                href={`/projects/${encodeURIComponent(projectSeg)}`}
                className="flex min-w-0 items-center gap-2 rounded-md px-1.5 py-1 font-medium hover:bg-subtle"
              >
                <Triangle className="h-5 w-5 shrink-0" />
                <span className="truncate">{projectSeg}</span>
              </Link>
              <Suspense fallback={null}>
                <ProjectWorkflowsCrumb project={projectSeg} />
              </Suspense>
            </>
          )}
          {deploymentSeg && (
            <>
              <Slash />
              <span className="flex min-w-0 items-center gap-2 px-1.5 py-1 font-medium">
                <span className="truncate font-mono text-[13px]">{deploymentSeg}</span>
                <span className="rounded bg-fg px-1.5 py-0.5 text-[10px] font-semibold uppercase text-bg">Deployment</span>
              </span>
            </>
          )}
          {workflowsSeg && (
            <>
              <Slash />
              <Link
                href="/workflows"
                className="flex min-w-0 items-center gap-2 rounded-md px-1.5 py-1 font-medium hover:bg-subtle"
              >
                <Workflow className="h-4 w-4 shrink-0" />
                <span className="truncate">Workflows</span>
              </Link>
            </>
          )}
        </div>
        <div className="flex items-center gap-2">
          {isOwner && (
            <Link
              href="/admin"
              className="hidden items-center gap-1.5 rounded-md border border-border-strong px-2.5 py-1 text-xs font-medium text-secondary hover:bg-subtle hover:text-fg sm:flex"
              title="Owner / operations dashboard"
            >
              <ShieldHalf className="h-3.5 w-3.5" /> Ops
            </Link>
          )}
          <RunNodeControl />
          <NotificationBell />
          <ThemeToggle />
          <Link
            href="/account"
            title="Account & settings"
            className="flex h-8 w-8 items-center justify-center rounded-md text-secondary hover:bg-subtle hover:text-fg"
          >
            <Settings className="h-4 w-4" />
          </Link>
        </div>
      </div>
      <div className="mx-auto max-w-[1400px] px-2 sm:px-4">
        <Suspense fallback={<div className="h-[38px]" />}>
          <SectionTabs isActive={isActive} />
        </Suspense>
      </div>
    </header>
  );
}

/** Breadcrumb crumb after the project when drilled into its Workflows; reads `?tab=`, so it
 *  must live under a Suspense boundary. */
function ProjectWorkflowsCrumb({ project }: { project: string }) {
  const tab = useSearchParams().get("tab");
  if (tab !== "workflows") return null;
  return (
    <>
      <Slash />
      <Link
        href={`/projects/${encodeURIComponent(project)}?tab=workflows`}
        className="flex min-w-0 items-center gap-2 rounded-md px-1.5 py-1 font-medium hover:bg-subtle"
      >
        <Workflow className="h-4 w-4 shrink-0" />
        <span className="truncate">Workflows</span>
      </Link>
    </>
  );
}

/** The section tab bar: one horizontally scrolling underline row (team tabs at the top level,
 *  project/deployment sub-tabs once drilled in) that keeps the active tab in view. */
function SectionTabs({ isActive }: { isActive: (href: string) => boolean }) {
  const pathname = usePathname();
  const searchParams = useSearchParams();
  const navRef = useRef<HTMLDivElement>(null);
  const ctx = contextTabs(pathname, searchParams.get("tab"));
  useEffect(() => {
    const nav = navRef.current;
    const el = nav?.querySelector<HTMLElement>("[data-active='true']");
    if (nav && el) {
      nav.scrollLeft = el.offsetLeft - nav.clientWidth / 2 + el.clientWidth / 2;
    }
  }, [pathname, searchParams]);

  if (ctx) {
    return (
      <nav ref={navRef} className="no-scrollbar -mb-px flex items-center gap-0.5 overflow-x-auto">
        {ctx.items.map((t) => {
          const active = t.key === ctx.activeKey;
          return (
            <Link
              key={t.key}
              href={t.href}
              data-active={active}
              className={cn(
                "shrink-0 whitespace-nowrap border-b-2 px-3 pb-2.5 pt-1 text-sm transition-colors",
                active ? "border-fg text-fg" : "border-transparent text-secondary hover:text-fg"
              )}
            >
              {t.label}
            </Link>
          );
        })}
      </nav>
    );
  }

  return (
    <nav ref={navRef} className="no-scrollbar -mb-px flex items-center gap-0.5 overflow-x-auto">
      {teamTabs.map((t) => {
        const active = isActive(t.href);
        return (
          <Link
            key={t.href}
            href={t.href}
            data-active={active}
            className={cn(
              "shrink-0 whitespace-nowrap border-b-2 px-3 pb-2.5 pt-1 text-sm transition-colors",
              active ? "border-fg text-fg" : "border-transparent text-secondary hover:text-fg"
            )}
          >
            {t.label}
          </Link>
        );
      })}
    </nav>
  );
}

const PERSONAL = "__personal__";

/** The ACTIVE tenant's subscription tier from its billing account (cookie-scoped `/v1/billing`,
 *  re-fetched on `hive-team-changed`); `null` until known so a badge renders nothing. */
function useActivePlan(): string | null {
  const { data } = usePoll<{ account?: { plan?: string } }>("/v1/billing", 60000);
  return data?.account?.plan || null;
}

/** Tenant id for the active Clerk org (its slug, falling back to id), or the
 *  personal sentinel. This is what scopes every data request (`x-hive-team`). */
function tenantOf(org: { slug?: string | null; id: string } | null | undefined): string {
  if (!org) return PERSONAL;
  return org.slug || org.id;
}

/** Clerk-driven team switcher: lists the user's organizations + personal account and is the
 *  single source of truth for the active tenant (`setActive` + `hive_team` re-pointing). */
function ClerkTeamSwitcher({ identity }: { identity: Identity }) {
  const { organization } = useOrganization();
  const { userMemberships, setActive, isLoaded } = useOrganizationList({
    userMemberships: { infinite: true },
  });
  const clerk = useClerk();
  const activePlan = useActivePlan();
  const [open, setOpen] = useState(false);
  const [selected, setSelected] = useState<string>(PERSONAL);
  const ref = useRef<HTMLDivElement>(null);
  const initDone = useRef(false);
  const syncedKey = useRef("");

  const memberships = userMemberships?.data ?? [];
  const orgBySlug = (s: string) => memberships.find((m) => (m.organization.slug || m.organization.id) === s)?.organization;

  useEffect(() => {
    if (typeof window === "undefined") return;
    if (!identity.id || identity.id === "local") return;
    const prev = localStorage.getItem("hive_uid");
    const firstIdentity = prev !== identity.id;
    if (prev && firstIdentity) {
      for (const k of ["hive_team", "hive_is_owner", "hive_platform_admin", "hive_role", "hive_session_clock", "oe_favorites", "hive_onboarded", "hive_gitops_linked", "hive_notif", "oe_push_dismissed", "hive_pending_builds"]) {
        localStorage.removeItem(k);
      }
      initDone.current = false;
      queueMicrotask(() => setSelected(PERSONAL));
    }
    localStorage.setItem("hive_uid", identity.id);
    if (firstIdentity) void switchTeam(PERSONAL);
  }, [identity.id]);

  useEffect(() => {
    if (typeof window === "undefined" || initDone.current) return;
    const saved = localStorage.getItem("hive_team");
    if (saved) {
      queueMicrotask(() => setSelected(saved));
      initDone.current = true;
    } else if (isLoaded) {
      const slug = tenantOf(organization);
      initDone.current = true;
      queueMicrotask(() => setSelected(slug));
      void switchTeam(slug);
    }
  }, [isLoaded, organization]);

  useEffect(() => {
    if (typeof window === "undefined") return;
    const resync = () => {
      const saved = localStorage.getItem("hive_team");
      if (saved && saved !== selected) setSelected(saved);
    };
    window.addEventListener("hive-team-changed", resync);
    return () => window.removeEventListener("hive-team-changed", resync);
  }, [selected]);

  useEffect(() => {
    if (typeof window === "undefined") return;
    const org = selected === PERSONAL ? null : orgBySlug(selected) ?? null;
    if (selected !== PERSONAL && !org && !isLoaded) return;
    const team = selected === PERSONAL ? `u_${identity.id}` : selected;
    const key = `${team}|${identity.id}|${org?.id ?? ""}`;
    if (syncedKey.current === key) return;
    syncedKey.current = key;
    fetch("/cloud/v1/identity/sync", {
      method: "POST",
      headers: { "content-type": "application/json", "x-hive-team": team },
      body: JSON.stringify({
        user: { id: identity.id, email: identity.email, name: identity.name, image_url: identity.imageUrl ?? "" },
        org: org ? { id: org.id, slug: org.slug ?? org.id, name: org.name, image_url: org.imageUrl ?? "" } : null,
      }),
    })
      .then((r) => r.json())
      .then(async (d) => {
        const prev = localStorage.getItem("hive_is_owner");
        const now = d?.is_owner ? "1" : "0";
        localStorage.setItem("hive_is_owner", now);
        if (prev === now) return;
        await mintSessionToken();
        window.dispatchEvent(new Event("hive-team-changed"));
      })
      .catch(() => {
        if (syncedKey.current === key) syncedKey.current = "";
      });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selected, isLoaded, memberships.length, identity.id]);

  useEffect(() => {
    function onClick(e: MouseEvent) {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    }
    document.addEventListener("mousedown", onClick);
    return () => document.removeEventListener("mousedown", onClick);
  }, []);

  async function pick(tenantSlug: string) {
    setOpen(false);
    setSelected(tenantSlug);
    await switchTeam(tenantSlug);
    const org = tenantSlug === PERSONAL ? null : orgBySlug(tenantSlug) ?? null;
    const orgId = org ? org.id : null;
    try {
      await clerk.setActive({ organization: orgId });
    } catch {
      try {
        await setActive?.({ organization: orgId });
      } catch {}
    }
  }

  const isPersonal = selected === PERSONAL;
  const selOrg = isPersonal ? null : orgBySlug(selected);
  const label = isPersonal ? identity.name : selOrg?.name ?? selected;
  const labelImg = isPersonal ? identity.imageUrl : selOrg?.imageUrl;

  return (
    <div className="relative" ref={ref}>
      <button onClick={() => setOpen((o) => !o)} className="flex items-center gap-2 rounded-md px-1.5 py-1 hover:bg-subtle">
        {labelImg ? (
          // eslint-disable-next-line @next/next/no-img-element
          <img src={labelImg} alt="" loading="lazy" decoding="async" width={24} height={24} className={cn("h-6 w-6 object-cover", isPersonal ? "rounded-full" : "rounded-md")} />
        ) : (
          <span className={cn("flex h-6 w-6 items-center justify-center rounded-full text-[11px] font-semibold text-white", isPersonal ? "bg-[#0761d1]" : "bg-fg text-bg")}>
            {(label?.[0] || "?").toUpperCase()}
          </span>
        )}
        <span className="font-medium">{label}</span>
        {activePlan && (
          <span className="rounded-full border border-border px-1.5 py-0.5 text-[10px] capitalize text-secondary">{activePlan}</span>
        )}
        <ChevronsUpDown className="h-3.5 w-3.5 text-muted" />
      </button>

      {open && (
        <div className="absolute left-0 top-full z-40 mt-1.5 w-64 max-w-[90vw] overflow-hidden rounded-lg border border-border bg-card shadow-pop">
          <div className="px-3 py-2 text-[11px] font-medium uppercase tracking-wide text-muted">Personal Account</div>
          <Option
            label={identity.name}
            hint={isPersonal ? activePlan ?? undefined : undefined}
            icon={<User className="h-3.5 w-3.5" />}
            selected={isPersonal}
            onClick={() => pick(PERSONAL)}
          />
          <div className="mt-1 border-t border-border px-3 py-2 text-[11px] font-medium uppercase tracking-wide text-muted">Organizations</div>
          {!isLoaded && <div className="px-3 py-2 text-sm text-muted">Loading…</div>}
          {isLoaded && memberships.length === 0 && (
            <div className="px-3 py-2 text-xs text-muted">No organizations yet.</div>
          )}
          {memberships.map((m) => {
            const slug = m.organization.slug || m.organization.id;
            return (
              <Option
                key={m.organization.id}
                label={m.organization.name}
                icon={
                  m.organization.imageUrl ? (
                    // eslint-disable-next-line @next/next/no-img-element
                    <img src={m.organization.imageUrl} alt="" loading="lazy" decoding="async" width={16} height={16} className="h-4 w-4 rounded object-cover" />
                  ) : (
                    <span className="flex h-4 w-4 items-center justify-center rounded bg-fg text-[9px] font-bold text-bg">{m.organization.name.slice(0, 1).toUpperCase()}</span>
                  )
                }
                selected={selected === slug}
                onClick={() => pick(slug)}
              />
            );
          })}
          <Link href="/teams" onClick={() => setOpen(false)} className="flex items-center gap-2 border-t border-border px-3 py-2.5 text-sm text-secondary hover:bg-subtle hover:text-fg">
            <Building2 className="h-3.5 w-3.5" /> Manage teams
          </Link>
          <button
            onClick={() => {
              setOpen(false);
              clerk.openCreateOrganization?.({ afterCreateOrganizationUrl: "/teams" });
            }}
            className="flex w-full items-center gap-2 border-t border-border px-3 py-2.5 text-sm font-medium text-link hover:bg-subtle"
          >
            <Plus className="h-3.5 w-3.5" /> Create organization
          </button>
        </div>
      )}
    </div>
  );
}

/** Toggleable team switcher — a personal ("just my name") view plus any teams. */
function TeamSwitcher({ identity }: { identity: Identity }) {
  const [open, setOpen] = useState(false);
  const { data: teams } = usePoll<Team[]>("/v1/teams", 10000, open);
  const activePlan = useActivePlan();
  const [sel, setSel] = useState<string>(PERSONAL);
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (typeof window === "undefined") return;
    const param = new URLSearchParams(window.location.search).get("team");
    if (param) {
      const slug = param === "personal" ? PERSONAL : param;
      queueMicrotask(() => setSel(slug));
      void switchTeam(slug);
      return;
    }
    const saved = localStorage.getItem("hive_team");
    if (saved) queueMicrotask(() => setSel(saved));
  }, []);
  useEffect(() => {
    function onClick(e: MouseEvent) {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    }
    document.addEventListener("mousedown", onClick);
    return () => document.removeEventListener("mousedown", onClick);
  }, []);

  function choose(slug: string) {
    setSel(slug);
    void switchTeam(slug);
    setOpen(false);
  }

  const current = sel === PERSONAL ? null : (teams ?? []).find((t) => t.slug === sel);
  const label = current ? current.name : identity.name;
  const plan = current ? current.plan : activePlan;

  return (
    <div className="relative" ref={ref}>
      <button
        onClick={() => setOpen((o) => !o)}
        className="flex items-center gap-2 rounded-md px-1.5 py-1 hover:bg-subtle"
      >
        {!current && identity.imageUrl ? (
          // eslint-disable-next-line @next/next/no-img-element
          <img src={identity.imageUrl} alt="" loading="lazy" decoding="async" width={24} height={24} className="h-6 w-6 rounded-full object-cover" />
        ) : (
          <span className={cn(
            "flex h-6 w-6 items-center justify-center rounded-full text-[11px] font-semibold text-white",
            current ? "bg-fg text-bg" : "bg-[#0761d1]"
          )}>
            {current ? label.slice(0, 1).toUpperCase() : identity.initial}
          </span>
        )}
        <span className="font-medium">{label}</span>
        {plan && (
          <span className="rounded-full border border-border px-1.5 py-0.5 text-[10px] capitalize text-secondary">{plan}</span>
        )}
        <ChevronsUpDown className="h-3.5 w-3.5 text-muted" />
      </button>

      {open && (
        <div className="absolute left-0 top-full z-40 mt-1.5 w-64 max-w-[90vw] overflow-hidden rounded-lg border border-border bg-card shadow-pop">
          <div className="px-3 py-2 text-[11px] font-medium uppercase tracking-wide text-muted">Personal Account</div>
          <Option label={identity.name} hint={sel === PERSONAL ? activePlan ?? undefined : undefined} icon={<User className="h-3.5 w-3.5" />} selected={sel === PERSONAL} onClick={() => choose(PERSONAL)} />
          <div className="mt-1 border-t border-border px-3 py-2 text-[11px] font-medium uppercase tracking-wide text-muted">Teams</div>
          {(teams ?? []).filter((t) => t.slug !== "personal").map((t) => (
            <Option
              key={t.slug}
              label={t.name}
              hint={t.plan}
              icon={<span className="flex h-4 w-4 items-center justify-center rounded bg-fg text-[9px] font-bold text-bg">{t.name.slice(0, 1).toUpperCase()}</span>}
              selected={sel === t.slug}
              onClick={() => choose(t.slug)}
            />
          ))}
          <Link href="/teams" onClick={() => setOpen(false)} className="flex items-center gap-2 border-t border-border px-3 py-2.5 text-sm text-secondary hover:bg-subtle hover:text-fg">
            <Plus className="h-3.5 w-3.5" /> Create Team
          </Link>
        </div>
      )}
    </div>
  );
}

function Option({ label, hint, icon, selected, onClick }: { label: string; hint?: string; icon: React.ReactNode; selected: boolean; onClick: () => void }) {
  return (
    <button onClick={onClick} className="flex w-full items-center justify-between gap-2 px-3 py-2 text-sm hover:bg-subtle">
      <span className="flex items-center gap-2">
        <span className="text-muted">{icon}</span>
        <span className="font-medium">{label}</span>
        {hint ? <span className="rounded-full border border-border px-1.5 py-0.5 text-[10px] capitalize text-muted">{hint}</span> : null}
      </span>
      {selected ? <Check className="h-4 w-4 text-fg" /> : null}
    </button>
  );
}
