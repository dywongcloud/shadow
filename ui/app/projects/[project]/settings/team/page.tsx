"use client";

import { useEffect, useState } from "react";
import { Lock } from "lucide-react";
import { Button, Switch, SettingCard } from "@/components/ui";
import { apiGet, apiSend, usePoll, type ProjectSettings, type Team } from "@/lib/api";

export default function TeamPrivacySettings({ params }: { params: { project: string } }) {
  const project = decodeURIComponent(params.project);
  const { data: teams } = usePoll<Team[]>("/v1/teams", 10000);
  const [settings, setSettings] = useState<ProjectSettings | null>(null);
  const [team, setTeam] = useState("personal");
  const [protect, setProtect] = useState(true);
  const [saved, setSaved] = useState(false);

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
    </div>
  );
}
