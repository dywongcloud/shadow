"use client";

import { useEffect, useState, use } from "react";
import Link from "next/link";
import { Lock } from "lucide-react";
import { Button, Switch, SettingCard } from "@/components/ui";
import { RoleGate } from "@/components/role-gate";
import { apiGet, apiSend, useCan, usePlatformAdmin, usePoll, type ProjectSettings, type Team } from "@/lib/api";
import { PLAN_TIERS, followCheckout, planAction, planRefusal, startPlanCheckout } from "@/lib/plan-change";
import { roleHint } from "@/lib/team-role";
import { cn } from "@/lib/utils";

export function TeamPrivacySettings({ paramsPromise }: { paramsPromise: Promise<{ project: string }> }) {
  const params = use(paramsPromise);
  const project = decodeURIComponent(params.project);
  const { data: teams, error: teamsError, refresh: refreshTeams } = usePoll<Team[]>("/v1/teams", 10000);
  const platformAdmin = usePlatformAdmin();
  const can = useCan();
  const [settings, setSettings] = useState<ProjectSettings | null>(null);
  const [team, setTeam] = useState("personal");
  const [protect, setProtect] = useState(true);
  const [saved, setSaved] = useState(false);
  const [saveErr, setSaveErr] = useState<{ card: "team" | "preview"; message: string } | null>(null);
  const [planBusy, setPlanBusy] = useState("");
  const [planErr, setPlanErr] = useState("");

  const owning = (teams ?? []).find((t) => t.slug === team);
  const plan = owning?.plan ?? "hobby";
  const ssoOn = !!owning?.sso_enabled;
  const planScope = !teams || settings === null ? "loading" : owning ? "team" : "none";
  const planFooter =
    planScope === "team"
      ? `Current plan for ${owning?.name ?? team}: ${plan}.`
      : planScope === "none"
        ? `No plan record for ${team}.`
        : teamsError
          ? "The plan could not be loaded."
          : "Loading the plan…";
  const planNote =
    planScope === "team"
      ? platformAdmin === true
        ? "Platform operator: a plan change applies immediately, without a payment."
        : `Upgrading opens checkout and the plan changes once the payment is confirmed; lowering a plan applies immediately${can("team.plan") ? "" : " but needs a team admin"}. Only a platform operator can change a plan without paying.`
      : planScope === "none"
        ? "This project's team has no plan record that can be changed here. The Billing page manages the plan of the team you are working in."
        : "";

  async function changePlan(target: string) {
    const action = planAction(plan, target, platformAdmin);
    if (action === "none") return;
    setPlanErr("");
    setPlanBusy(target);
    let leaving = false;
    try {
      if (action === "checkout") {
        const checkout = await startPlanCheckout(target);
        if (!checkout.applied) {
          leaving = true;
          followCheckout(checkout.url);
          return;
        }
      } else {
        await apiSend("PUT", `/v1/teams/${encodeURIComponent(team)}/plan`, { plan: target });
      }
      await refreshTeams();
    } catch (e) {
      setPlanErr(planRefusal(e));
    } finally {
      if (!leaving) setPlanBusy("");
    }
  }
  async function toggleSso(v: boolean) {
    setPlanErr("");
    try {
      await apiSend("PUT", `/v1/teams/${encodeURIComponent(team)}/sso`, { enabled: v });
      await refreshTeams();
    } catch (e) {
      setPlanErr(planRefusal(e));
    }
  }

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

  async function save(card: "team" | "preview") {
    setSaveErr(null);
    try {
      await apiSend("PUT", `/v1/projects/${encodeURIComponent(project)}/team`, { team, preview_protection: protect });
      setSaved(true);
      setTimeout(() => setSaved(false), 1500);
    } catch (e) {
      setSaveErr({ card, message: e instanceof Error ? e.message : String(e) });
    }
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
      setProtErr(e instanceof Error ? e.message : String(e));
    }
  }

  return (
    <div className="space-y-6">
      <RoleGate action="project.team" note>
      <div className="space-y-6">
      <SettingCard
        title="Owning Team"
        desc="The team that owns this project. Team members can view its deployments, logs and settings."
        footer="Projects belong to exactly one team."
        footerAction={<Button onClick={() => save("team")}>{saved ? "Saved" : "Save"}</Button>}
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
        {saveErr?.card === "team" ? <p className="mt-3 text-sm text-red-500">{saveErr.message}</p> : null}
      </SettingCard>

      <SettingCard
        title="Preview Deployment Protection"
        desc="When enabled, preview deployments are private — only signed-in members of the owning team can open preview URLs. Production stays public."
        footer={protect ? "Previews require team-member authentication." : "Anyone with the URL can view previews."}
        footerAction={<Button onClick={() => save("preview")}>{saved ? "Saved" : "Save"}</Button>}
      >
        <div className="flex items-center gap-3">
          <Switch checked={protect} onChange={setProtect} label="Preview protection" />
          <span className="flex items-center gap-1.5 text-sm text-secondary"><Lock className="h-4 w-4" /> {protect ? "Protected (team only)" : "Public"}</span>
        </div>
        {saveErr?.card === "preview" ? <p className="mt-3 text-sm text-red-500">{saveErr.message}</p> : null}
      </SettingCard>
      </div>
      </RoleGate>

      <RoleGate action="enterprise.config" note>
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
      </RoleGate>

      <SettingCard
        title="Plan & Enterprise"
        desc="Your team's tier. Enterprise unlocks automatic region fail-over, 1-hour function runtimes (3600s), and optional Org SSO."
        footer={planFooter}
      >
        <div className="flex flex-wrap gap-2">
          {PLAN_TIERS.map((p) => {
            const known = planScope === "team";
            const current = known && plan === p;
            const action = known ? planAction(plan, p, platformAdmin) : "none";
            const needs = action === "switch" ? "team.plan" : "work";
            const allowed = action === "none" || can(needs);
            return (
              <button
                key={p}
                onClick={() => changePlan(p)}
                disabled={!!planBusy || !known || !allowed}
                aria-current={current ? "true" : undefined}
                title={!allowed ? roleHint(needs) : action === "checkout" ? "Opens checkout; the plan changes once your payment is confirmed." : undefined}
                className={cn(
                  "rounded-md border px-3 py-1.5 text-sm transition-colors disabled:opacity-50",
                  current ? "border-fg bg-fg text-bg" : "border-border hover:bg-subtle"
                )}
              >
                {planBusy === p ? (
                  "…"
                ) : (
                  <>
                    {action === "checkout" ? "Upgrade to " : ""}
                    <span className="capitalize">{p}</span>
                    {current ? " ✓" : ""}
                  </>
                )}
              </button>
            );
          })}
        </div>
        {planNote ? (
          <p className="mt-3 text-xs text-secondary">
            {planNote}
            {platformAdmin === true ? null : (
              <>
                {" "}
                <Link href="/billing" className="underline hover:text-fg">Open Billing</Link>
              </>
            )}
          </p>
        ) : null}
        {planErr ? (
          <p className="mt-3 text-sm text-red-500">
            {planErr}{" "}
            <Link href="/billing" className="underline">Open Billing</Link>
          </p>
        ) : null}
        <div className="mt-4 flex items-center gap-3 border-t border-border pt-4">
          <span title={can("team.sso") ? undefined : roleHint("team.sso")}>
            <Switch checked={ssoOn} disabled={plan !== "enterprise" || !can("team.sso")} onChange={toggleSso} label="Org SSO" />
          </span>
          <span className="text-sm text-secondary">Team / Org SSO (SAML &amp; OIDC) login</span>
          {plan !== "enterprise" && (
            <span className="rounded-full border border-amber-500/30 bg-amber-500/10 px-2 py-0.5 text-[11px] font-medium text-amber-600 dark:text-amber-400">Enterprise</span>
          )}
        </div>
      </SettingCard>
    </div>
  );
}
