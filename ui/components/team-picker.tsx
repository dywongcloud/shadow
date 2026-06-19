"use client";

import { useOrganizationList, useUser } from "@clerk/nextjs";
import { ChevronDown } from "lucide-react";
import { usePoll, type Team } from "@/lib/api";

// Team picker that mirrors the navbar breadcrumb's source of truth so the
// selected team always matches what the breadcrumb shows. With Clerk on, the
// options come from the user's Clerk organizations (keyed by slug||id, the same
// `currentTeam()` value the rest of the app uses); in local mode they come from
// the backend `/v1/teams`.
//
// `value` is the `currentTeam()`-style id: "personal" or an org slug.

const clerkEnabled = !!process.env.NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY;

const selectCls =
  "w-full appearance-none rounded-md border border-border bg-card px-3 py-2 text-sm focus:border-border-strong focus:outline-none";

export function TeamSelect({ value, onChange }: { value: string; onChange: (v: string) => void }) {
  return clerkEnabled ? <ClerkTeamSelect value={value} onChange={onChange} /> : <LocalTeamSelect value={value} onChange={onChange} />;
}

function ClerkTeamSelect({ value, onChange }: { value: string; onChange: (v: string) => void }) {
  const { user } = useUser();
  const { userMemberships } = useOrganizationList({ userMemberships: { infinite: true } });
  const orgs = (userMemberships?.data ?? []).map((m) => m.organization);
  const personalLabel = user?.firstName || user?.fullName || user?.username || "Personal";

  // If the current value is an org we don't (yet) see in memberships, still show
  // it so the field reflects the breadcrumb rather than snapping to Personal.
  const known = new Set(orgs.map((o) => o.slug || o.id));
  const orphan = value !== "personal" && !known.has(value) ? value : null;

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
