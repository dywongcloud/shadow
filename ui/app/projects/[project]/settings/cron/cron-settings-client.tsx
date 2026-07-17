"use client";

import { useEffect, useState, use } from "react";
import { ExternalLink } from "lucide-react";
import { Button, SettingCard, Switch } from "@/components/ui";
import { apiGet, apiSend, usePoll, type CronJob, type ProjectSettings } from "@/lib/api";
import { timeAgo } from "@/lib/utils";

/** Best-effort English label for the common cron shapes; falls back to the
 * raw expression (never lies about a schedule it can't confidently describe). */
function describeSchedule(expr: string): string | null {
  const f = expr.trim().split(/\s+/);
  if (f.length !== 5 && f.length !== 6) return null;
  // Normalize to 5 fields (min hour dom month dow) — drop a leading seconds
  // field (this platform's own native format) when present and "0".
  const [min, hour, dom, month, dow] = f.length === 6 ? f.slice(1) : f;
  const every = (field: string) => /^\*\/(\d+)$/.exec(field)?.[1];
  if (dom === "*" && month === "*" && dow === "*") {
    const everyMin = every(min);
    if (everyMin && hour === "*") return `Every ${everyMin} minute${everyMin === "1" ? "" : "s"}`;
    if (min === "*" && hour === "*") return "Every minute";
    const everyHour = every(hour);
    if (min !== "*" && !everyMin && everyHour) return `Every ${everyHour} hours, at minute ${min}`;
    if (min !== "*" && !everyMin && hour !== "*" && !everyHour) {
      return `Daily at ${hour.padStart(2, "0")}:${min.padStart(2, "0")} UTC`;
    }
  }
  return null;
}

export function CronSettings({ paramsPromise }: { paramsPromise: Promise<{ project: string }> }) {
  const params = use(paramsPromise);
  const project = decodeURIComponent(params.project);
  const { data: allJobs, refresh } = usePoll<CronJob[]>("/v1/cron", 5000);
  const [settings, setSettings] = useState<ProjectSettings | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [err, setErr] = useState("");

  useEffect(() => {
    apiGet<ProjectSettings>(`/v1/projects/${encodeURIComponent(project)}/settings`).then(setSettings);
  }, [project]);

  const jobs = (allJobs ?? []).filter((j) => j.deployment === project);
  const enabled = settings?.cron_enabled ?? true;

  async function toggle(next: boolean) {
    setSettings((s) => (s ? { ...s, cron_enabled: next } : s));
    try {
      await apiSend("PUT", `/v1/projects/${encodeURIComponent(project)}/cron-enabled`, { enabled: next });
    } catch (e) {
      setSettings((s) => (s ? { ...s, cron_enabled: !next } : s));
      setErr(String(e));
    }
  }

  async function run(id: string) {
    setBusy(id);
    setErr("");
    try {
      await apiSend("POST", `/v1/cron/${id}/run`, {});
      refresh();
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(null);
    }
  }

  return (
    <div className="flex flex-col gap-6">
      <SettingCard
        title="Cron Jobs"
        desc="Easily monitor and manage your cron jobs."
        footer={
          <a
            href="https://vercel.com/docs/cron-jobs"
            target="_blank"
            rel="noreferrer"
            className="inline-flex items-center gap-1 text-blue-500 hover:underline"
          >
            Learn more about Cron Jobs <ExternalLink className="h-3 w-3" />
          </a>
        }
      >
        <p className="text-sm text-secondary">
          Disabling this feature will prevent all cron jobs from being executed. New cron jobs will still be
          created, updated, and deleted on each production deployment, but they will not run until the feature is
          reactivated.
        </p>
        <div className="mt-4 flex items-center gap-2">
          <Switch checked={enabled} onChange={toggle} label="Cron jobs enabled" />
          <span className="text-sm">{enabled ? "Enabled" : "Disabled"}</span>
        </div>
      </SettingCard>

      {err ? <p className="text-xs text-red-400">{err}</p> : null}

      <div className="overflow-hidden rounded-xl border border-border bg-card shadow-card">
        <div className="border-b border-border px-5 py-3 text-xs text-secondary sm:px-6">
          All scheduled times use the UTC timezone.
        </div>
        {jobs.length === 0 ? (
          <div className="px-5 py-10 text-center text-sm text-secondary sm:px-6">
            No cron jobs are configured for this project. Declare a <code className="font-mono">crons</code> array in{" "}
            <code className="font-mono">vercel.json</code>, or add one from the{" "}
            <a href="/cron" className="text-blue-500 hover:underline">Cron Jobs</a> page.
          </div>
        ) : (
          <div className="divide-y divide-border">
            {jobs.map((j) => (
              <div key={j.id} className="flex flex-col gap-3 px-5 py-4 sm:flex-row sm:items-center sm:justify-between sm:px-6">
                <div className="min-w-0">
                  <div className="font-mono text-sm">{j.path}</div>
                  <div className="mt-1 flex flex-wrap items-center gap-2 text-xs text-secondary">
                    <code className="font-mono">{j.schedule}</code>
                    {describeSchedule(j.schedule) ? <span>{describeSchedule(j.schedule)}</span> : null}
                    {j.last_run_ms ? <span>· last ran {timeAgo(j.last_run_ms)}</span> : null}
                  </div>
                </div>
                <div className="flex shrink-0 items-center gap-2">
                  <Button variant="outline" disabled={!enabled || busy === j.id} onClick={() => run(j.id)}>
                    {busy === j.id ? "Running…" : "Run"}
                  </Button>
                  <Button
                    variant="outline"
                    onClick={() => {
                      window.location.href = `/projects/${encodeURIComponent(project)}/logs?q=${encodeURIComponent(j.path)}`;
                    }}
                  >
                    View Logs
                  </Button>
                </div>
              </div>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}
