"use client";

import { useState } from "react";
import { Users, Plus, X, Trash2, Crown, Shield, User as UserIcon, Eye } from "lucide-react";
import { Card, Badge, Button, Input, PageHeader } from "@/components/ui";
import { apiSend, usePoll, type Team, type Role } from "@/lib/api";

const ROLE_META: Record<Role, { icon: React.ReactNode; tone: "amber" | "blue" | "green" | "default" }> = {
  owner: { icon: <Crown className="h-3 w-3" />, tone: "amber" },
  admin: { icon: <Shield className="h-3 w-3" />, tone: "blue" },
  member: { icon: <UserIcon className="h-3 w-3" />, tone: "green" },
  viewer: { icon: <Eye className="h-3 w-3" />, tone: "default" },
};

export default function TeamsPage() {
  const { data: teams, refresh } = usePoll<Team[]>("/v1/teams", 4000);
  const [creating, setCreating] = useState(false);
  const [newName, setNewName] = useState("");

  async function createTeam() {
    if (!newName.trim()) return;
    await apiSend("POST", "/v1/teams", { name: newName, plan: "pro" });
    setNewName("");
    setCreating(false);
    refresh();
  }

  return (
    <div>
      <PageHeader
        title="Teams"
        desc="Teams own projects, databases and deployments. Members get scoped access — previews are private to the team by default."
        action={<Button onClick={() => setCreating(true)}><Plus className="h-4 w-4" /> Create Team</Button>}
      />

      {creating && (
        <Card className="mb-6 flex items-end gap-3">
          <div className="flex-1">
            <label className="mb-1 block text-xs font-medium text-secondary">Team name</label>
            <Input value={newName} onChange={(e) => setNewName(e.target.value)} placeholder="Acme Inc" autoFocus />
          </div>
          <Button onClick={createTeam}>Create</Button>
          <Button variant="ghost" onClick={() => setCreating(false)}>Cancel</Button>
        </Card>
      )}

      <div className="space-y-6">
        {(teams ?? []).map((t) => (
          <TeamCard key={t.slug} team={t} onChange={refresh} />
        ))}
        {!teams?.length && <Card className="py-12 text-center text-sm text-secondary">No teams yet.</Card>}
      </div>
    </div>
  );
}

function TeamCard({ team, onChange }: { team: Team; onChange: () => void }) {
  const [email, setEmail] = useState("");
  const [role, setRole] = useState<Role>("member");

  async function addMember() {
    if (!email.trim()) return;
    await apiSend("POST", `/v1/teams/${team.slug}/members`, { email, role });
    setEmail("");
    onChange();
  }
  async function removeMember(e: string) {
    await apiSend("DELETE", `/v1/teams/${team.slug}/members/${encodeURIComponent(e)}`);
    onChange();
  }

  return (
    <Card>
      <div className="mb-4 flex items-center justify-between">
        <div className="flex items-center gap-3">
          <span className="flex h-9 w-9 items-center justify-center rounded-lg bg-fg text-bg"><Users className="h-4 w-4" /></span>
          <div>
            <div className="font-semibold">{team.name}</div>
            <div className="font-mono text-xs text-muted">{team.slug}</div>
          </div>
        </div>
        <Badge tone="blue">{team.plan}</Badge>
      </div>

      <div className="divide-y divide-border rounded-lg border border-border">
        {team.members.map((m) => (
          <div key={m.email} className="flex items-center justify-between gap-3 px-4 py-2.5">
            <div className="flex items-center gap-3">
              <span className="flex h-7 w-7 items-center justify-center rounded-full bg-subtle text-xs font-semibold text-secondary">
                {m.email.slice(0, 1).toUpperCase()}
              </span>
              <div className="text-sm">{m.email}</div>
            </div>
            <div className="flex items-center gap-2">
              <Badge tone={ROLE_META[m.role]?.tone ?? "default"}>{ROLE_META[m.role]?.icon} {m.role}</Badge>
              {m.role !== "owner" && (
                <button onClick={() => removeMember(m.email)} className="text-muted hover:text-red-500"><Trash2 className="h-3.5 w-3.5" /></button>
              )}
            </div>
          </div>
        ))}
      </div>

      <div className="mt-4 flex flex-col gap-2 sm:flex-row sm:items-center">
        <Input value={email} onChange={(e) => setEmail(e.target.value)} placeholder="teammate@company.com" className="flex-1" />
        <select
          value={role}
          onChange={(e) => setRole(e.target.value as Role)}
          className="rounded-md border border-border bg-card px-3 py-2 text-sm text-fg focus:outline-none"
        >
          <option value="admin">Admin</option>
          <option value="member">Member</option>
          <option value="viewer">Viewer</option>
        </select>
        <Button onClick={addMember}><Plus className="h-4 w-4" /> Invite</Button>
      </div>
    </Card>
  );
}
