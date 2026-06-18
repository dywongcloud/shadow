"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import { useEffect, useRef, useState } from "react";
import { ChevronsUpDown, ShieldHalf, Check, Plus, User, Settings, Building2 } from "lucide-react";
import { useOrganization, useOrganizationList, useClerk } from "@clerk/nextjs";
import { cn } from "@/lib/utils";
import { ThemeToggle } from "@/components/theme-toggle";
import { NotificationBell } from "@/components/notifications";
import { WithIdentity, type Identity } from "@/components/identity";
import { usePoll, type Team } from "@/lib/api";

const clerkOn = !!process.env.NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY;

const tabs = [
  { href: "/", label: "Projects" },
  { href: "/workflows", label: "Workflows" },
  { href: "/storage", label: "Storage" },
  { href: "/observability", label: "Observability" },
  { href: "/cdn", label: "CDN" },
  { href: "/integrations", label: "Integrations" },
  { href: "/domains", label: "Domains" },
  { href: "/firewall", label: "Firewall" },
  { href: "/network", label: "Network" },
  { href: "/usage", label: "Usage" },
  { href: "/billing", label: "Billing" },
  { href: "/settings", label: "Settings" },
];

/** The Vercel triangle mark — inverts with the theme (black on light, white on dark). */
function VercelMark() {
  return (
    <svg height="20" viewBox="0 0 76 65" fill="none" aria-label="Vercel" className="text-fg">
      <path d="M37.59.25l36.95 64H.64l36.95-64z" fill="currentColor" />
    </svg>
  );
}

export function TopNav() {
  const pathname = usePathname();
  // The owner/ops dashboard + auth pages render their own minimal chrome.
  if (pathname.startsWith("/admin") || pathname.startsWith("/sign-in") || pathname.startsWith("/sign-up")) return null;
  const isActive = (href: string) =>
    href === "/" ? pathname === "/" : pathname.startsWith(href);

  return (
    <header className="sticky top-0 z-30 border-b border-border bg-bg/85 backdrop-blur">
      {/* Row 1: brand + team switcher + account */}
      <div className="mx-auto flex h-[52px] max-w-[1400px] items-center justify-between px-4 sm:px-6">
        <div className="flex items-center gap-2 text-sm">
          <Link href="/" className="flex items-center"><VercelMark /></Link>
          <span className="px-1 text-2xl font-thin text-border-strong">/</span>
          <WithIdentity>{(id) => (clerkOn ? <ClerkTeamSwitcher identity={id} /> : <TeamSwitcher identity={id} />)}</WithIdentity>
        </div>
        <div className="flex items-center gap-2">
          <Link
            href="/admin"
            className="hidden items-center gap-1.5 rounded-md border border-border-strong px-2.5 py-1 text-xs font-medium text-secondary hover:bg-subtle hover:text-fg sm:flex"
            title="Owner / operations dashboard"
          >
            <ShieldHalf className="h-3.5 w-3.5" /> Ops
          </Link>
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
      {/* Row 2: tabs — active underline sits flush on the header's bottom border */}
      <div className="mx-auto max-w-[1400px] px-2 sm:px-4">
        <nav className="-mb-px flex items-center gap-0.5 overflow-x-auto">
          {tabs.map((t) => {
            const active = isActive(t.href);
            return (
              <Link
                key={t.href}
                href={t.href}
                className={cn(
                  "whitespace-nowrap border-b-2 px-3 pb-2.5 pt-1 text-sm transition-colors",
                  active
                    ? "border-fg text-fg"
                    : "border-transparent text-secondary hover:text-fg"
                )}
              >
                {t.label}
              </Link>
            );
          })}
        </nav>
      </div>
    </header>
  );
}

const PERSONAL = "__personal__";

/** Tenant id for the active Clerk org (its slug, falling back to id), or the
 *  personal sentinel. This is what scopes every data request (`x-hive-team`). */
function tenantOf(org: { slug?: string | null; id: string } | null | undefined): string {
  if (!org) return PERSONAL;
  return org.slug || org.id;
}

/**
 * Clerk-driven team switcher: the navbar lists the user's actual Clerk
 * organizations + personal account, reflects Clerk's active organization, and
 * is the single source of truth for the active tenant. Switching here calls
 * Clerk's `setActive` and the active-org effect re-points `hive_team`, which
 * every data hook depends on — so data never bleeds across accounts.
 */
function ClerkTeamSwitcher({ identity }: { identity: Identity }) {
  const { organization } = useOrganization();
  const { userMemberships, setActive, isLoaded } = useOrganizationList({
    userMemberships: { infinite: true },
  });
  const clerk = useClerk();
  const [open, setOpen] = useState(false);
  // OUR selection is the source of truth for the dashboard view — persisted in
  // localStorage and authoritative over Clerk's (possibly org-forced) active
  // session. This is what makes "switch to Personal" stick + survive refresh.
  const [selected, setSelected] = useState<string>(PERSONAL);
  const ref = useRef<HTMLDivElement>(null);
  const initDone = useRef(false);

  const memberships = userMemberships?.data ?? [];
  const orgBySlug = (s: string) => memberships.find((m) => (m.organization.slug || m.organization.id) === s)?.organization;

  // Initialize the selection ONCE: restore the user's saved choice if any,
  // otherwise adopt Clerk's active org the first time it loads. After init, the
  // user's explicit navbar choice always wins — Clerk never overrides it.
  useEffect(() => {
    if (typeof window === "undefined" || initDone.current) return;
    const saved = localStorage.getItem("hive_team");
    if (saved) {
      setSelected(saved);
      initDone.current = true;
    } else if (isLoaded) {
      const slug = tenantOf(organization);
      setSelected(slug);
      localStorage.setItem("hive_team", slug);
      window.dispatchEvent(new Event("hive-team-changed"));
      initDone.current = true;
    }
  }, [isLoaded, organization?.id]);

  // Index the user + (resolved) org into the store for the active tenant. Keyed
  // on the selection, not Clerk's org, so it follows what the user picked.
  useEffect(() => {
    if (typeof window === "undefined") return;
    const org = selected === PERSONAL ? null : orgBySlug(selected) ?? null;
    if (selected !== PERSONAL && !org && !isLoaded) return; // wait for memberships
    const team = selected === PERSONAL ? "personal" : selected;
    fetch("/cloud/v1/identity/sync", {
      method: "POST",
      headers: { "content-type": "application/json", "x-hive-team": team },
      body: JSON.stringify({
        user: { id: identity.id, email: identity.email, name: identity.name, image_url: identity.imageUrl ?? "" },
        org: org ? { id: org.id, slug: org.slug ?? org.id, name: org.name, image_url: org.imageUrl ?? "" } : null,
      }),
    }).catch(() => {});
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selected, isLoaded, memberships.length, identity.id]);

  useEffect(() => {
    function onClick(e: MouseEvent) {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    }
    document.addEventListener("mousedown", onClick);
    return () => document.removeEventListener("mousedown", onClick);
  }, []);

  // Switch tenant: our selection + persisted hive_team flip immediately (data +
  // navbar), then Clerk's active org is aligned best-effort (never blocking).
  async function pick(tenantSlug: string) {
    setOpen(false);
    setSelected(tenantSlug);
    if (typeof window !== "undefined") {
      localStorage.setItem("hive_team", tenantSlug);
      window.dispatchEvent(new Event("hive-team-changed"));
    }
    const org = tenantSlug === PERSONAL ? null : orgBySlug(tenantSlug) ?? null;
    const orgId = org ? org.id : null;
    try {
      await clerk.setActive({ organization: orgId });
    } catch {
      try {
        await setActive?.({ organization: orgId });
      } catch {
        /* best-effort — our selection already drives the view */
      }
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
          <img src={labelImg} alt="" className={cn("h-6 w-6 object-cover", isPersonal ? "rounded-full" : "rounded-md")} />
        ) : (
          <span className={cn("flex h-6 w-6 items-center justify-center rounded-full text-[11px] font-semibold text-white", isPersonal ? "bg-[#0761d1]" : "bg-fg text-bg")}>
            {(label?.[0] || "?").toUpperCase()}
          </span>
        )}
        <span className="font-medium">{label}</span>
        <span className="rounded-full border border-border px-1.5 py-0.5 text-[10px] capitalize text-secondary">{isPersonal ? "Hobby" : "Team"}</span>
        <ChevronsUpDown className="h-3.5 w-3.5 text-muted" />
      </button>

      {open && (
        <div className="absolute left-0 top-full z-40 mt-1.5 w-64 overflow-hidden rounded-lg border border-border bg-card shadow-pop">
          <div className="px-3 py-2 text-[11px] font-medium uppercase tracking-wide text-muted">Personal Account</div>
          <Option
            label={identity.name}
            hint="Hobby"
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
                    <img src={m.organization.imageUrl} alt="" className="h-4 w-4 rounded object-cover" />
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
          {/* Create org/team — the very bottom action, opens Clerk's create flow. */}
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
  const { data: teams } = usePoll<Team[]>("/v1/teams", 10000);
  const [open, setOpen] = useState(false);
  const [sel, setSel] = useState<string>(PERSONAL);
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (typeof window === "undefined") return;
    // A `?team=<slug>` deep-link selects (and persists) that team — shareable,
    // team-scoped dashboard URLs.
    const param = new URLSearchParams(window.location.search).get("team");
    if (param) {
      const slug = param === "personal" ? PERSONAL : param;
      setSel(slug);
      localStorage.setItem("hive_team", slug);
      window.dispatchEvent(new Event("hive-team-changed"));
      return;
    }
    const saved = localStorage.getItem("hive_team");
    if (saved) setSel(saved);
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
    if (typeof window !== "undefined") {
      localStorage.setItem("hive_team", slug);
      // Tell every usePoll to re-fetch with the new tenant immediately.
      window.dispatchEvent(new Event("hive-team-changed"));
    }
    setOpen(false);
  }

  const current = sel === PERSONAL ? null : (teams ?? []).find((t) => t.slug === sel);
  const label = current ? current.name : identity.name;
  const plan = current ? current.plan : "Hobby";

  return (
    <div className="relative" ref={ref}>
      <button
        onClick={() => setOpen((o) => !o)}
        className="flex items-center gap-2 rounded-md px-1.5 py-1 hover:bg-subtle"
      >
        {!current && identity.imageUrl ? (
          // eslint-disable-next-line @next/next/no-img-element
          <img src={identity.imageUrl} alt="" className="h-6 w-6 rounded-full object-cover" />
        ) : (
          <span className={cn(
            "flex h-6 w-6 items-center justify-center rounded-full text-[11px] font-semibold text-white",
            current ? "bg-fg text-bg" : "bg-[#0761d1]"
          )}>
            {current ? label.slice(0, 1).toUpperCase() : identity.initial}
          </span>
        )}
        <span className="font-medium">{label}</span>
        <span className="rounded-full border border-border px-1.5 py-0.5 text-[10px] capitalize text-secondary">{plan}</span>
        <ChevronsUpDown className="h-3.5 w-3.5 text-muted" />
      </button>

      {open && (
        <div className="absolute left-0 top-full z-40 mt-1.5 w-64 overflow-hidden rounded-lg border border-border bg-card shadow-pop">
          <div className="px-3 py-2 text-[11px] font-medium uppercase tracking-wide text-muted">Personal Account</div>
          <Option label={identity.name} hint="Hobby" icon={<User className="h-3.5 w-3.5" />} selected={sel === PERSONAL} onClick={() => choose(PERSONAL)} />
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
