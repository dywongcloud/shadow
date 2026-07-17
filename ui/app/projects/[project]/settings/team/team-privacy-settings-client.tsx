"use client";

import { useEffect, useState, use } from "react";
import { Lock } from "lucide-react";
import { Button, Switch, SettingCard } from "@/components/ui";
import { apiGet, apiSend, usePoll, type ProjectSettings, type Team } from "@/lib/api";
import { cn } from "@/lib/utils";

export function TeamPrivacySettings({ paramsPromise }: { paramsPromise: Promise<{ project: string }> }) {
  const params = use(paramsPromise);
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

  // Enterprise deployment protection (password / scope).
  const [protMode, setProtMode] = useState<"off" | "standard" | "password">("off");
  const [protScope, setProtScope] = useState<"preview" | "all" | "production">("preview");
  const [protPassword, setProtPassword] = useState("");
  const [protHasPw, setProtHasPw] = useState(false);
  const [protSaved, setProtSaved] = useState(false);
  const [protErr, setProtErr] = useState<string | null>(null);

  useEffect(() => {
    apiGet<ProjectSettings>(`/v1/projects/${encodeURIComponent(project)}/settings`).then((s) => {
      setSettings(s);
      setTeam(s.team ?? "personal");
      setProtect(s.preview_protection ?? true);
    });
    apiGet<{ mode: string; scope: string; has_password: boolean }>(`/v1/enterprise/projects/${encodeURIComponent(project)}/protection`)
      .then((p) => {
        setProtMode((p.mode as "off" | "standard" | "password") ?? "off");
        setProtScope((p.scope as "preview" | "all" | "production") ?? "preview");
        setProtHasPw(p.has_password);
      })
      .catch(() => {});
  }, [project]);

  async function save() {
    await apiSend("PUT", `/v1/projects/${encodeURIComponent(project)}/team`, { team, preview_protection: protect });
    setSaved(true);
    setTimeout(() => setSaved(false), 1500);
  }

  async function saveProtection() {
    setProtErr(null);
    try {
      await apiSend("PUT", `/v1/enterprise/projects/${encodeURIComponent(project)}/protection`, {
        mode: protMode,
        scope: protScope,
        password: protPassword || undefined,
      });
      setProtPassword("");
      setProtHasPw(protMode === "password");
      setProtSaved(true);
      setTimeout(() => setProtSaved(false), 1500);
    } catch (e) {
      setProtErr(String(e).replace(/^Error:\s*/, ""));
    }
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

      {/* Enterprise deployment protection — password / scope ("only production"). */}
      <SettingCard
        title="Deployment Protection"
        desc="Require authentication or a shared password to view deployments. Choose which deployments are protected — previews, all, or only production."
        footer={
          protMode === "off"
            ? "Deployments follow the preview-protection setting above."
            : `${protMode === "password" ? "Password" : "Team member"} required for ${protScope === "all" ? "all" : protScope === "production" ? "production" : "preview"} deployments.`
        }
        footerAction={<Button onClick={saveProtection}>{protSaved ? "Saved" : "Save"}</Button>}
      >
        <div className="grid gap-4 sm:grid-cols-2">
          <label className="text-sm">
            <span className="mb-1 block text-secondary">Protection mode</span>
            <select
              value={protMode}
              onChange={(e) => setProtMode(e.target.value as "off" | "standard" | "password")}
              className="w-full rounded-md border border-border bg-card px-3 py-2 text-sm text-fg focus:outline-none"
            >
              <option value="off">Off</option>
              <option value="standard">Standard (team members)</option>
              <option value="password">Password</option>
            </select>
          </label>
          <label className="text-sm">
            <span className="mb-1 block text-secondary">Applies to</span>
            <select
              value={protScope}
              onChange={(e) => setProtScope(e.target.value as "preview" | "all" | "production")}
              disabled={protMode === "off"}
              className="w-full rounded-md border border-border bg-card px-3 py-2 text-sm text-fg focus:outline-none disabled:opacity-50"
            >
              <option value="preview">Preview deployments</option>
              <option value="all">All deployments</option>
              <option value="production">Only production deployments</option>
            </select>
          </label>
        </div>
        {protMode === "password" ? (
          <label className="mt-3 block text-sm">
            <span className="mb-1 block text-secondary">Password{protHasPw ? " — set (leave blank to keep)" : ""}</span>
            <input
              type="password"
              value={protPassword}
              onChange={(e) => setProtPassword(e.target.value)}
              placeholder={protHasPw ? "••••••" : "Enter a password"}
              className="w-full max-w-sm rounded-md border border-border bg-card px-3 py-2 text-sm text-fg placeholder:text-muted focus:outline-none"
            />
          </label>
        ) : null}
        {protErr ? <p className="mt-3 text-sm text-red-500">{protErr}</p> : null}
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
