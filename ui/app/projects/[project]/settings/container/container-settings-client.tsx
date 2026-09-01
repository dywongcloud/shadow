"use client";

import { useCallback, useEffect, useState, use } from "react";
import { Button, Input, SettingCard } from "@/components/ui";
import { apiGet, apiSend, type ContainerSettings, type ProjectSettings } from "@/lib/api";

/**
 * Container config for a project running the `container` runtime — resource
 * ceilings (memory/CPU/max-PIDs) and the persistent volume's mount path.
 * Saving PUTs `/v1/projects/:project/container`: this is a DASHBOARD-MANAGED
 * override that applies going forward, on the project's NEXT build/redeploy
 * (git.rs's produce_manifest merges it per-field, an explicit fluid.json
 * `container` block always wins) — it does not touch anything already
 * running. Port/protocol live on the separate Network settings page instead,
 * which edits the CURRENT deployment immediately with no redeploy needed —
 * deliberately not duplicated here to avoid two disagreeing places to set
 * the same thing.
 */
export function ContainerSettings({ paramsPromise }: { paramsPromise: Promise<{ project: string }> }) {
  const params = use(paramsPromise);
  const project = decodeURIComponent(params.project);
  const [c, setC] = useState<ContainerSettings | null>(null);
  const [saving, setSaving] = useState(false);
  const [saved, setSaved] = useState(false);
  const [err, setErr] = useState("");

  const load = useCallback(async () => {
    const s = await apiGet<ProjectSettings>(`/v1/projects/${encodeURIComponent(project)}/settings`);
    setC(s.container ?? {});
  }, [project]);
  useEffect(() => {
    // Fetch-on-mount/project-change: load() sets state only after its
    // internal await resolves, syncing external (server) settings into React.
    // eslint-disable-next-line react-hooks/set-state-in-effect -- fetch effect; state is set only after an internal await, not synchronously
    load();
  }, [load]);

  async function save() {
    if (!c) return;
    setErr("");
    setSaving(true);
    try {
      await apiSend("PUT", `/v1/projects/${encodeURIComponent(project)}/container`, c);
      setSaved(true);
      setTimeout(() => setSaved(false), 2000);
    } catch (e) {
      setErr(String(e instanceof Error ? e.message : e));
    } finally {
      setSaving(false);
    }
  }

  if (!c) return <div className="text-sm text-secondary">Loading…</div>;

  const num = (v: string) => (v.trim() === "" ? null : parseInt(v, 10));

  return (
    <div className="flex flex-col gap-6">
      <SettingCard
        title="Resources"
        desc="Ceilings for this project's container — memory, CPU quota, and a fork-bomb guard. Leave a field blank to use the node's generous default. An explicit fluid.json `container` block always overrides these per field."
        footer="Applies on the next build/redeploy — nothing already running changes until then."
        footerAction={
          <Button onClick={save} disabled={saving}>
            {saving ? "Saving…" : saved ? "Saved" : "Save"}
          </Button>
        }
      >
        <div className="flex flex-col gap-4">
          <Field label="Memory" hint='e.g. "4g", "2048m" — blank uses the node default.'>
            <Input
              value={c.memory ?? ""}
              onChange={(e) => setC({ ...c, memory: e.target.value || null })}
              placeholder="e.g. 2g"
              className="max-w-xs font-mono"
            />
          </Field>
          <Field label="CPU quota" hint='e.g. "2.0", "0.5" — blank uses the node default.'>
            <Input
              value={c.cpus ?? ""}
              onChange={(e) => setC({ ...c, cpus: e.target.value || null })}
              placeholder="e.g. 2.0"
              className="max-w-xs font-mono"
            />
          </Field>
          <Field label="Max PIDs" hint="Fork-bomb guard for the container's cgroup — blank uses the node default.">
            <Input
              value={c.pids != null ? String(c.pids) : ""}
              onChange={(e) => setC({ ...c, pids: num(e.target.value.replace(/[^0-9]/g, "")) })}
              placeholder="e.g. 512"
              inputMode="numeric"
              className="max-w-xs font-mono"
            />
          </Field>
        </div>
      </SettingCard>

      <SettingCard
        title="Persistent Volume"
        desc="Every container-runtime project already gets a durable, host-backed volume that survives restarts and redeploys — this only overrides where it's mounted INSIDE the container. Most images that persist data (databases, game servers) document their own expected path."
        footer="Applies on the next build/redeploy."
        footerAction={
          <Button onClick={save} disabled={saving}>
            {saving ? "Saving…" : saved ? "Saved" : "Save"}
          </Button>
        }
      >
        <Field label="Mount Path" hint='Default "/data" if left blank — e.g. Minecraft servers typically also use "/data".'>
          <Input
            value={c.volume_mount_path ?? ""}
            onChange={(e) => setC({ ...c, volume_mount_path: e.target.value || null })}
            placeholder="/data"
            className="max-w-xs font-mono"
          />
        </Field>
      </SettingCard>

      {err && <div className="rounded-lg border border-red-500/30 bg-red-500/5 p-3 text-sm text-red-500">{err}</div>}

      <p className="text-xs text-muted">
        Looking for the listen port or wire protocol (http/tcp/udp/grpc)? That&apos;s on the{" "}
        <a href={`/projects/${encodeURIComponent(project)}/settings/network`} className="underline hover:text-fg">
          Network
        </a>{" "}
        settings page — it edits the current deployment immediately, no redeploy required.
      </p>
    </div>
  );
}

function Field({ label, hint, children }: { label: string; hint?: string; children: React.ReactNode }) {
  return (
    <div>
      <label className="mb-1.5 block text-sm font-medium">{label}</label>
      {children}
      {hint ? <p className="mt-1.5 text-xs text-secondary">{hint}</p> : null}
    </div>
  );
}
