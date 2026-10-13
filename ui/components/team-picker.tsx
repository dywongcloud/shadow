"use client";

import { useOrganizationList, useUser } from "@clerk/nextjs";
import { ChevronDown } from "lucide-react";
import { usePoll, type Team } from "@/lib/api";

const clerkEnabled = !!process.env.NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY;

const selectCls =
  "w-full appearance-none rounded-md border border-border bg-card px-3 py-2 text-sm focus:border-border-strong focus:outline-none";

/** Team picker mirroring the navbar breadcrumb's source of truth: with Clerk on the options are the user's Clerk organizations (keyed by slug||id, the `currentTeam()` value), in local
 *  mode the backend `/v1/teams`. `value` is a `currentTeam()`-style id ("personal" or an org slug); one that is not (yet) listed is still shown as an extra option, so the field reflects
 *  the breadcrumb instead of snapping to Personal. */
export function TeamSelect({ value, onChange }: { value: string; onChange: (v: string) => void }) {
  return clerkEnabled ? <ClerkTeamSelect value={value} onChange={onChange} /> : <LocalTeamSelect value={value} onChange={onChange} />;
}

function ClerkTeamSelect({ value, onChange }: { value: string; onChange: (v: string) => void }) {
  const { user } = useUser();
  const { userMemberships } = useOrganizationList({ userMemberships: { infinite: true } });
  const orgs = (userMemberships?.data ?? []).map((m) => m.organization);
  const personalLabel = user?.firstName || user?.fullName || user?.username || "Personal";

  const listedSlugs = new Set(orgs.map((o) => o.slug || o.id));
  const orphan = value !== "personal" && !listedSlugs.has(value) ? value : null;

  return (
    <div className="relative">
      <select value={value} onChange={(e) => onChange(e.target.value)} className={selectCls}>
        <option value="personal">{personalLabel} (Personal)</option>
        {orgs.map((o) => {
          const slug = o.slug || o.id;
          return <option key={slug} value={slug}>{o.name}</option>;
        })}
        {orphan && <option value={orphan}>{orphan}</option>}
      </select>
      <ChevronDown className="pointer-events-none absolute right-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted" />
    </div>
  );
}

function LocalTeamSelect({ value, onChange }: { value: string; onChange: (v: string) => void }) {
  const { data: teams } = usePoll<Team[]>("/v1/teams", 10000);
  const known = new Set((teams ?? []).map((t) => t.slug));
  const orphan = value !== "personal" && !known.has(value) ? value : null;
  return (
    <div className="relative">
      <select value={value} onChange={(e) => onChange(e.target.value)} className={selectCls}>
        <option value="personal">Personal</option>
        {(teams ?? []).filter((t) => t.slug !== "personal").map((t) => (
          <option key={t.slug} value={t.slug}>{t.name} · {t.plan}</option>
        ))}
        {orphan && <option value={orphan}>{orphan}</option>}
      </select>
      <ChevronDown className="pointer-events-none absolute right-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted" />
    </div>
  );
}
