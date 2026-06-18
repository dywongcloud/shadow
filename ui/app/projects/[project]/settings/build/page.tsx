"use client";

import { useEffect, useState } from "react";
import { Button, Input, SettingCard } from "@/components/ui";
import { apiGet, apiSend, type BuildConfig, type ProjectSettings } from "@/lib/api";

const FRAMEWORKS = ["Other", "Next.js", "Vite", "Astro", "SvelteKit", "Nuxt", "Remix", "Static", "Python (FastAPI)", "Node (Express)"];

export default function BuildSettings({ params }: { params: { project: string } }) {
  const project = decodeURIComponent(params.project);
  const [b, setB] = useState<BuildConfig | null>(null);

  async function load() {
    const s = await apiGet<ProjectSettings>(`/v1/projects/${project}/settings`);
    setB(s.build);
  }
  useEffect(() => { load(); }, [project]);

  async function save() {
    if (!b) return;
    await apiSend("PUT", `/v1/projects/${project}/build`, b);
  }

  if (!b) return <div className="text-sm text-secondary">Loading…</div>;

  return (
    <div className="flex flex-col gap-6">
      <SettingCard
        title="Framework Settings"
        desc="The framework preset configures the build for your project. Override the commands below if you use a custom setup."
        footer="A new deployment is required for changes to take effect."
        footerAction={<Button onClick={save}>Save</Button>}
      >
        <div className="flex flex-col gap-4">
          <Field label="Framework Preset">
            <select
              value={b.framework}
              onChange={(e) => setB({ ...b, framework: e.target.value })}
              className="w-full rounded-md border border-border bg-card px-3 py-2 text-sm focus:border-border-strong focus:outline-none focus:ring-2 focus:ring-border"
            >
              {FRAMEWORKS.map((f) => <option key={f}>{f}</option>)}
            </select>
          </Field>
          <Field label="Build Command" hint="The command OpenEdge runs to build your app.">
            <Input value={b.build_command} onChange={(e) => setB({ ...b, build_command: e.target.value })} className="font-mono" />
          </Field>
          <Field label="Output Directory" hint="Directory of build output served as static assets.">
            <Input value={b.output_dir} onChange={(e) => setB({ ...b, output_dir: e.target.value })} className="font-mono" />
          </Field>
          <Field label="Install Command">
            <Input value={b.install_command} onChange={(e) => setB({ ...b, install_command: e.target.value })} className="font-mono" />
          </Field>
        </div>
      </SettingCard>

      <SettingCard
        title="Root Directory"
        desc="The directory within your repository that your app is located. Leave as ./ if it is the repository root."
        footer="A new deployment is required for changes to take effect."
        footerAction={<Button onClick={save}>Save</Button>}
      >
        <Input value={b.root_dir} onChange={(e) => setB({ ...b, root_dir: e.target.value })} className="max-w-sm font-mono" />
      </SettingCard>
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
