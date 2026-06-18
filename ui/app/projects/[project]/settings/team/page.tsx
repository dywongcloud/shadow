"use client";

import { useEffect, useState } from "react";
import { Lock } from "lucide-react";
import { Button, Switch, SettingCard } from "@/components/ui";
import { apiGet, apiSend, usePoll, type ProjectSettings, type Team } from "@/lib/api";
import { cn } from "@/lib/utils";

export default function TeamPrivacySettings({ params }: { params: { project: string } }) {
  const project = decodeURIComponent(params.project);
  const { data: teams, refresh: refreshTeams } = usePoll<Team[]>("/v1/teams", 10000);
  const [settings, setSettings] = useState<ProjectSettings | null>(null);
  const [team, setTeam] = useState("personal");
  const [protect, setProtect] = useState(true);
  const [saved, setSaved] = useState(false);
  const [planBusy, setPlanBusy] = useState("");

  const owning = (teams ?? []).find((t) => t.slug === team);
  const plan = owning?.plan ?? "hobby";
  const ssoOn = !!owning?.sso_enabled;

  async function changePlan(p: string) {
    setPlanBusy(p);
    try {
      await apiSend("PUT", `/v1/teams/${encodeURIComponent(team)}/plan`, { plan: p });
      await refreshTeams();
    } catch (e) {
      alert(String(e));
    } finally {
      setPlanBusy("");
    }
  }
  async function toggleSso(v: boolean) {
    try {
      await apiSend("PUT", `/v1/teams/${encodeURIComponent(team)}/sso`, { enabled: v });
      await refreshTeams();
    } catch (e) {
      alert(String(e));
    }
  }

  useEffect(() => {
    apiGet<ProjectSettings>(`/v1/projects/${encodeURIComponent(project)}/settings`).then((s) => {
      setSettings(s);
      setTeam(s.team ?? "personal");
      setProtect(s.preview_protection ?? true);
    });
  }, [project]);

  async function save() {
    await apiSend("PUT", `/v1/projects/${encodeURIComponent(project)}/team`, { team, preview_protection: protect });
    setSaved(true);
    setTimeout(() => setSaved(false), 1500);
  }

  return (
    <div className="space-y-6">
      <SettingCard
        title="Owning Team"
        desc="The team that owns this project. Team members can view its deployments, logs and settings."
        footer="Projects belong to exactly one team."
        footerAction={<Button onClick={save}>{saved ? "Saved" : "Save"}</Button>}
      >
        <select
          value={team}
          onChange={(e) => setTeam(e.target.value)}
          className="w-full max-w-sm rounded-md border border-border bg-card px-3 py-2 text-sm text-fg focus:outline-none"
        >
          {(teams ?? []).map((t) => (
            <option key={t.slug} value={t.slug}>{t.name} ({t.slug})</option>
          ))}
        </select>
      </SettingCard>

      <SettingCard
        title="Preview Deployment Protection"
        desc="When enabled, preview deployments are private — only signed-in members of the owning team can open preview URLs. Production stays public."
        footer={protect ? "Previews require team-member authentication." : "Anyone with the URL can view previews."}
        footerAction={<Button onClick={save}>{saved ? "Saved" : "Save"}</Button>}
      >
        <div className="flex items-center gap-3">
          <Switch checked={protect} onChange={setProtect} label="Preview protection" />
          <span className="flex items-center gap-1.5 text-sm text-secondary"><Lock className="h-4 w-4" /> {protect ? "Protected (team only)" : "Public"}</span>
        </div>
      </SettingCard>

      {/* Plan & Enterprise capabilities for the owning team. */}
      <SettingCard
        title="Plan & Enterprise"
        desc="Your team's tier. Enterprise unlocks automatic region fail-over, 1-hour function runtimes (3600s), and optional Org SSO."
        footer={`Current plan for ${owning?.name ?? team}: ${plan}.`}
      >
        <div className="flex flex-wrap gap-2">
          {(["hobby", "pro", "enterprise"] as const).map((p) => (
            <button
              key={p}
              onClick={() => changePlan(p)}
              disabled={!!planBusy}
              className={cn(
                "rounded-md border px-3 py-1.5 text-sm capitalize transition-colors disabled:opacity-50",
                plan === p ? "border-fg bg-fg text-bg" : "border-border hover:bg-subtle"
              )}
            >
              {planBusy === p ? "…" : p}{plan === p ? " ✓" : ""}
            </button>
          ))}
        </div>
        <div className="mt-4 flex items-center gap-3 border-t border-border pt-4">
          <Switch checked={ssoOn} disabled={plan !== "enterprise"} onChange={toggleSso} label="Org SSO" />
          <span className="text-sm text-secondary">Team / Org SSO (SAML &amp; OIDC) login</span>
          {plan !== "enterprise" && (
            <span className="rounded-full border border-amber-500/30 bg-amber-500/10 px-2 py-0.5 text-[11px] font-medium text-amber-600 dark:text-amber-400">Enterprise</span>
          )}
        </div>
      </SettingCard>
    </div>
  );
}
