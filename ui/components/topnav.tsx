"use client";

import Link from "next/link";
import { usePathname, useSearchParams } from "next/navigation";
import { Suspense, useEffect, useRef, useState } from "react";
import { ChevronsUpDown, ShieldHalf, Check, Plus, User, Settings, Building2, Workflow } from "lucide-react";
import { useOrganization, useOrganizationList, useClerk } from "@clerk/nextjs";
import { cn } from "@/lib/utils";
import { Triangle } from "@/components/ui";
import { ThemeToggle } from "@/components/theme-toggle";
import { VercelMark } from "@/components/logo";
import { NotificationBell } from "@/components/notifications";
import { RunNodeControl } from "@/components/run-node-control";
import { WithIdentity, type Identity } from "@/components/identity";
import { usePoll, switchTeam, mintSessionToken, type Team } from "@/lib/api";
import { useIsPlatformOwner } from "@/lib/owner";

const clerkOn = !!process.env.NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY;

// Team/account-level tabs — shown when NO project/deployment is selected.
const teamTabs = [
  { href: "/", label: "Projects" },
  { href: "/regions", label: "Regions" },
  { href: "/workflows", label: "Workflows" },
  { href: "/storage", label: "Storage" },
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

/** Drilled-in context (Vercel breadcrumb-tabs model): once a project or a
 *  deployment is selected, the TEAM tabs collapse into the breadcrumb and the
 *  SUB-tabs for that level take the top tab-bar position. Returns the tab set +
 *  the active key for the current URL. `tabParam` is the reactive `?tab=` value. */
type TabItem = { href: string; label: string; key: string };
function contextTabs(pathname: string, tabParam: string | null): { items: TabItem[]; activeKey: string } | null {
  // Deployment detail: /deployments/<id>
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
  // Project: /projects/<p>[/logs|/settings] with ?tab= for the in-page tabs.
  const proj = pathname.match(/^\/projects\/([^/]+)/);
  if (proj) {
    const p = proj[1];
    const base = `/projects/${p}`;
    // (The project's Workflows tab now embeds the upstream console, which
    // carries its own Runs/Hooks/Workflows segmented control — no ?wf=
    // sub-tab override in the top bar anymore.)
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
  // (/workflows embeds the upstream console, which carries its own
  // Runs/Hooks/Workflows segmented control — the top bar keeps the normal
  // team tabs there, no sub-tab override.)
  return null;
}

/** The Vercel triangle mark — inverts with the theme (black on light, white on dark). */
/** Thin breadcrumb separator matching the brand slash. */
function Slash() {
  return <span className="px-1 text-2xl font-thin text-border-strong">/</span>;
}

export function TopNav() {
  const pathname = usePathname();
  const isOwner = useIsPlatformOwner();
  // The owner/ops dashboard + auth + public status pages render their own chrome.
  if (pathname.startsWith("/admin") || pathname.startsWith("/sign-in") || pathname.startsWith("/sign-up") || pathname.startsWith("/status") || pathname.startsWith("/docs")) return null;
  const isActive = (href: string) =>
    href === "/" ? pathname === "/" : pathname.startsWith(href);

  // When a project is selected, scope the breadcrumb to it:
  //   LOGO / Team / Project
  const projectSeg = pathname.startsWith("/projects/")
    ? decodeURIComponent(pathname.split("/")[2] ?? "")
    : "";
  // Deployment detail also gets a breadcrumb crumb (LOGO / Team / <id>), and its
  // own sub-tabs take the top bar — matching Vercel's drill-in.
  const deploymentSeg = pathname.startsWith("/deployments/")
    ? decodeURIComponent(pathname.split("/")[2] ?? "")
    : "";
  // Workflows is a drilled-in context: breadcrumb LOGO / Team / Workflows, with the
  // Workflows sub-tabs (Runs / Workflows / Hooks) in the top tab bar below.
  const workflowsSeg = pathname === "/workflows" || pathname.startsWith("/workflows/");

  return (
    <header
      className="sticky top-0 z-30 border-b border-border bg-bg/85 backdrop-blur"
      // bn-ui-mobile-lifecycle: the root layout sets viewport-fit=cover +
      // apple-mobile-web-app-status-bar-style=black-translucent (app/layout.tsx),
      // which is what lets an installed iOS PWA extend content under the
      // notch/status-bar in the first place -- but without THIS, a sticky
      // top-0 header (this one) renders its own content underneath that
      // status bar/notch with nothing pushing it down, exactly the classic
      // "content hidden behind the notch" PWA bug. env() resolves to 0 on
      // any device with no notch/inset (desktop, older phones), so this is a
      // no-op everywhere except where it's actually needed.
      style={{ paddingTop: "env(safe-area-inset-top)" }}
    >
      {/* Row 1: brand + team switcher + account */}
      <div className="mx-auto flex h-[52px] max-w-[1400px] items-center justify-between px-4 sm:px-6">
        <div className="flex min-w-0 items-center gap-2 text-sm">
          <Link href="/" className="flex items-center"><VercelMark className="h-5 w-auto" /></Link>
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
              {/* Deeper crumb (Project / Workflows) when drilled into the project's
                  Workflows. Suspense-bounded: it reads `?tab=` (useSearchParams). */}
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
          {/* Ops entry — platform owner only (middleware enforces; this just hides the link). */}
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
      {/* Row 2: section tabs — ONE horizontally-scrollable underline bar at every
          width (Vercel-style). No dropdown/overlay on mobile: it swipes, the active
          tab auto-centers, and the scrollbar is hidden. */}
      <div className="mx-auto max-w-[1400px] px-2 sm:px-4">
        {/* Suspense-wrapped: SectionTabs reads `?tab=` (useSearchParams) — the
            boundary keeps that from deopting static marketing pages to dynamic. */}
        <Suspense fallback={<div className="h-[38px]" />}>
          <SectionTabs isActive={isActive} />
        </Suspense>
      </div>
    </header>
  );
}

/** Breadcrumb crumb rendered after the project when drilled into its Workflows
 *  (`/projects/<p>?tab=workflows`): `… / Project / Workflows`. Reads `?tab=`, so it
 *  must live under a Suspense boundary (kept out of the static-page render path).
 *  Links back to the project's Workflows root (the Runs sub-tab). */
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

/** The section tab bar: a single underline row that scrolls horizontally on narrow
 *  screens instead of collapsing into a dropdown. Keeps the active tab in view.
 *  Context-aware: team tabs at the top level, or the project/deployment SUB-tabs
 *  once drilled in (Vercel breadcrumb-tabs model). */
function SectionTabs({ isActive }: { isActive: (href: string) => boolean }) {
  const pathname = usePathname();
  const searchParams = useSearchParams();
  const navRef = useRef<HTMLDivElement>(null);
  const ctx = contextTabs(pathname, searchParams.get("tab"));
  // Center the active tab within the scroll container (it may start off-screen on
  // mobile). Only touches the container's scrollLeft — never scrolls the page.
  useEffect(() => {
    const nav = navRef.current;
    const el = nav?.querySelector<HTMLElement>("[data-active='true']");
    if (nav && el) {
      nav.scrollLeft = el.offsetLeft - nav.clientWidth / 2 + el.clientWidth / 2;
    }
  }, [pathname, searchParams]);

  if (ctx) {
    // Drilled-in: render the project/deployment sub-tabs in the top bar position.
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
  const syncedKey = useRef("");

  const memberships = userMemberships?.data ?? [];
  const orgBySlug = (s: string) => memberships.find((m) => (m.organization.slug || m.organization.id) === s)?.organization;

  // Reset per-account view state when a DIFFERENT user signs in (someone logged
  // out, someone else logged in) — so the previous account's active team, the
  // breadcrumb team/org label + dropdown, favorites, onboarding, etc. never leak
  // into the new session. Runs BEFORE init so the cleared state is re-derived
  // from the new user's own Clerk active org.
  useEffect(() => {
    if (typeof window === "undefined") return;
    // IMPORTANT: ignore the transient "local" placeholder identity that appears
    // while Clerk's useUser() is still loading (or signed out). Treating it as a
    // user change would spuriously wipe onboarding/gitops/team state on EVERY load
    // (e.g. re-prompting the GitOps setup modal after it was already completed).
    if (!identity.id || identity.id === "local") return;
    const prev = localStorage.getItem("hive_uid");
    const firstIdentity = prev !== identity.id; // includes unset -> set
    if (prev && firstIdentity) {
      for (const k of ["hive_team", "hive_is_owner", "oe_favorites", "hive_onboarded", "hive_gitops_linked", "hive_notif", "oe_push_dismissed"]) {
        localStorage.removeItem(k);
      }
      initDone.current = false;
      // react-hooks/set-state-in-effect: defer by one microtask (same
      // behavior-preserving fix as use-run-node.ts) rather than remove --
      // this branch genuinely needs to reset the selection when the signed-in
      // identity changes.
      queueMicrotask(() => setSelected(PERSONAL));
    }
    localStorage.setItem("hive_uid", identity.id);
    // When the user id is first established, the personal namespace changes from
    // the pre-auth placeholder to `u_<uid>` — re-mint through switchTeam (never a
    // raw dispatch) so pollers re-fetch under the CORRECT per-account namespace
    // with a matching cookie. A raw dispatch here left the PREVIOUS account's
    // hive_jwt cookie (up to 1h TTL, nothing else ever clears it) fully valid —
    // it kept authenticating this session as the old user/tenant, silently
    // serving their data under the freshly-signed-in account's view.
    if (firstIdentity) void switchTeam(PERSONAL);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [identity.id]);

  // Initialize the selection ONCE: restore the user's saved choice if any,
  // otherwise adopt Clerk's active org the first time it loads. After init, the
  // user's explicit navbar choice always wins — Clerk never overrides it.
  useEffect(() => {
    if (typeof window === "undefined" || initDone.current) return;
    const saved = localStorage.getItem("hive_team");
    if (saved) {
      // Already-persisted selection: SessionToken's mount-time mint (which ran
      // BEFORE this effect, reading this same localStorage value) already
      // minted a matching cookie — nothing to re-mint here.
      // react-hooks/set-state-in-effect: microtask-deferred, same as above.
      queueMicrotask(() => setSelected(saved));
      initDone.current = true;
    } else if (isLoaded) {
      const slug = tenantOf(organization);
      // initDone.current is set FIRST (a ref write, unaffected by the
      // microtask deferral below) so a re-render triggered by switchTeam's
      // own state changes can't re-enter this branch — the guard above reads
      // initDone.current, never `selected`.
      initDone.current = true;
      // react-hooks/set-state-in-effect: microtask-deferred, same as above.
      queueMicrotask(() => setSelected(slug));
      // Route through switchTeam (persists + re-mints + THEN broadcasts) rather
      // than a raw setItem+dispatch — the mount-time mint already ran with NO
      // hive_team set (currentTeam() fell through to the pre-auth placeholder),
      // so the cookie does not yet match this adopted org; broadcasting first
      // told every poller "the view is now `slug`" while the browser kept
      // sending that stale/placeholder-scoped cookie, rendering whatever it
      // authenticated as under the newly-adopted org's view.
      void switchTeam(slug);
    }
  }, [isLoaded, organization?.id]);

  // Resync the navbar's selection with `hive_team` on every `hive-team-changed`
  // — same-tab (a no-op here since `pick`/the effects above already set
  // `selected` directly) AND cross-tab (lib/api.ts re-dispatches this event
  // locally on a `storage` change from another tab). Without this, a second
  // open tab's navbar keeps showing its OLD tenant's label while the shared
  // cookie/localStorage — and therefore every poller's actual data — has
  // already moved to whatever tenant the OTHER tab switched to.
  useEffect(() => {
    if (typeof window === "undefined") return;
    const resync = () => {
      const saved = localStorage.getItem("hive_team");
      if (saved && saved !== selected) setSelected(saved);
    };
    window.addEventListener("hive-team-changed", resync);
    return () => window.removeEventListener("hive-team-changed", resync);
  }, [selected]);

  // Index the user + (resolved) org into the store for the active tenant. Keyed
  // on the selection, not Clerk's org, so it follows what the user picked.
  useEffect(() => {
    if (typeof window === "undefined") return;
    const org = selected === PERSONAL ? null : orgBySlug(selected) ?? null;
    if (selected !== PERSONAL && !org && !isLoaded) return; // wait for memberships
    // Personal scope is per-USER (`u_<uid>`), never the shared literal "personal".
    const team = selected === PERSONAL ? `u_${identity.id}` : selected;
    // De-dup: this effect re-runs on several dep changes that can fire in a <100ms
    // burst, POSTing the same identity 2-4x. Skip when the (team,user,org) tuple is
    // unchanged so we sync once per unique selection.
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
        // The backend authoritatively marks the platform owner (owner_email). The
        // owner keeps the legacy "personal" namespace; everyone else stays isolated
        // under `u_<uid>`. Refresh pollers so personal data loads under the right one.
        const prev = localStorage.getItem("hive_is_owner");
        const now = d?.is_owner ? "1" : "0";
        localStorage.setItem("hive_is_owner", now);
        if (prev === now) return;
        // hive_is_owner flips which tenant currentTeam() resolves to (personal vs
        // u_<uid>) — the FIRST time this resolves (a brand-new session), the
        // hive_jwt cookie was already minted moments earlier under the OLD guess
        // (u_<uid>, since hive_is_owner wasn't known yet — see the sibling
        // hive_uid effect's switchTeam(PERSONAL) call above). The backend derives
        // the tenant SOLELY from that cookie's claim, never from client-side
        // belief, so a raw dispatch here told pollers to re-fetch while they kept
        // authenticating as the WRONG (but validly empty) u_<uid> tenant — a
        // successful 200 for the wrong namespace, never a 401 that would trigger
        // the auth-retry remint, so it silently never self-corrected without a
        // reload. Re-mint for the NOW-correct tenant before telling anyone to
        // re-fetch, mirroring switchTeam's own discipline.
        await mintSessionToken();
        window.dispatchEvent(new Event("hive-team-changed"));
      })
      .catch(() => {
        if (syncedKey.current === key) syncedKey.current = ""; // allow a retry on failure
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

  // Switch tenant: `switchTeam` persists the selection, RE-MINTS the hive_jwt
  // cookie for the new tenant (validated server-side), and only then fires
  // hive-team-changed so pollers re-fetch with the correct cookie. Clerk's active
  // org is aligned best-effort afterwards (no longer drives the tenant — that's
  // the cookie now — so it never blocks or gates the view).
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
      } catch {
        /* best-effort — the cookie already drives the view */
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
          <img src={labelImg} alt="" loading="lazy" decoding="async" width={24} height={24} className={cn("h-6 w-6 object-cover", isPersonal ? "rounded-full" : "rounded-md")} />
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
        <div className="absolute left-0 top-full z-40 mt-1.5 w-64 max-w-[90vw] overflow-hidden rounded-lg border border-border bg-card shadow-pop">
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
  const [open, setOpen] = useState(false);
  // Team membership rarely changes — one fetch to populate the topnav label,
  // then only re-poll while the dropdown is actually open (instead of forever
  // in the background on every page). `choose()` already force-refreshes via
  // `hive-team-changed` on switch, so freshness on interaction is unaffected.
  const { data: teams } = usePoll<Team[]>("/v1/teams", 10000, open);
  const [sel, setSel] = useState<string>(PERSONAL);
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (typeof window === "undefined") return;
    // A `?team=<slug>` deep-link selects (and persists) that team — shareable,
    // team-scoped dashboard URLs.
    const param = new URLSearchParams(window.location.search).get("team");
    if (param) {
      const slug = param === "personal" ? PERSONAL : param;
      // react-hooks/set-state-in-effect: microtask-deferred, same as topnav's
      // other effects above -- switchTeam below still runs synchronously
      // right after, unaffected by when setSel's deferred update lands.
      queueMicrotask(() => setSel(slug));
      // Re-mint the cookie for the deep-linked team (validated) before re-fetch.
      void switchTeam(slug);
      return;
    }
    const saved = localStorage.getItem("hive_team");
    // react-hooks/set-state-in-effect: microtask-deferred, same as above.
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
    // Persist + re-mint the cookie for the new tenant BEFORE pollers re-fetch.
    void switchTeam(slug);
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
        <span className="rounded-full border border-border px-1.5 py-0.5 text-[10px] capitalize text-secondary">{plan}</span>
        <ChevronsUpDown className="h-3.5 w-3.5 text-muted" />
      </button>

      {open && (
        <div className="absolute left-0 top-full z-40 mt-1.5 w-64 max-w-[90vw] overflow-hidden rounded-lg border border-border bg-card shadow-pop">
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
