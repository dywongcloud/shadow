"use client";

import { useState } from "react";
import { Webhook as WebhookIcon, Plus, Trash2, Check, X } from "lucide-react";
import { Badge, Button, Input, SettingCard } from "@/components/ui";
import { apiSend, usePoll, type Webhook, type Delivery } from "@/lib/api";
import { timeAgo } from "@/lib/utils";

export default function WebhooksSettings({ params }: { params: { project: string } }) {
  const project = decodeURIComponent(params.project);
  const { data: hooks, refresh } = usePoll<Webhook[]>(`/v1/projects/${encodeURIComponent(project)}/webhooks`, 5000);
  const { data: events } = usePoll<string[]>("/v1/webhooks/events", 60000);
  const { data: deliveries } = usePoll<Delivery[]>("/v1/webhooks/deliveries", 5000);

  const [url, setUrl] = useState("");
  const [selected, setSelected] = useState<string[]>([]);

  function toggle(ev: string) {
    setSelected((s) => (s.includes(ev) ? s.filter((x) => x !== ev) : [...s, ev]));
  }
  async function create() {
    if (!url.trim()) return;
    await apiSend("POST", `/v1/projects/${encodeURIComponent(project)}/webhooks`, { url, events: selected });
    setUrl("");
    setSelected([]);
    refresh();
  }
  async function remove(id: string) {
    await apiSend("DELETE", `/v1/webhooks/${id}`);
    refresh();
  }

  return (
    <div className="space-y-6">
      <SettingCard
        title="Webhooks"
        desc="Send a signed HTTP POST to your services when platform events happen — deployment status, database provisioning and more. Verify the X-OpenEdge-Signature (HMAC-SHA256) header with the webhook secret."
        footer="Each delivery includes an X-OpenEdge-Event header and a signed JSON body."
      >
        <div className="space-y-4">
          <div>
            <label className="mb-1 block text-xs font-medium text-secondary">Endpoint URL</label>
            <Input value={url} onChange={(e) => setUrl(e.target.value)} placeholder="https://api.yourapp.com/hooks/hive" />
          </div>
          <div>
            <label className="mb-2 block text-xs font-medium text-secondary">Events {selected.length === 0 && <span className="text-muted">(none selected = all events)</span>}</label>
            <div className="flex flex-wrap gap-1.5">
              {(events ?? []).map((ev) => (
                <button
                  key={ev}
                  onClick={() => toggle(ev)}
                  className={`rounded-full border px-2.5 py-1 text-xs font-mono transition-colors ${selected.includes(ev) ? "border-fg bg-fg text-bg" : "border-border text-secondary hover:bg-subtle"}`}
                >
                  {ev}
                </button>
              ))}
            </div>
          </div>
          <Button onClick={create}><Plus className="h-4 w-4" /> Create Webhook</Button>
        </div>
      </SettingCard>

      {!!hooks?.length && (
        <div className="space-y-3">
          <h3 className="text-sm font-semibold">Active webhooks</h3>
          {hooks.map((h) => (
            <div key={h.id} className="flex items-start justify-between gap-3 rounded-lg border border-border bg-card p-4">
              <div className="min-w-0">
                <div className="flex items-center gap-2 truncate font-mono text-sm"><WebhookIcon className="h-4 w-4 shrink-0 text-muted" /> {h.url}</div>
                <div className="mt-1.5 flex flex-wrap gap-1">
                  {h.events.length ? h.events.map((e) => <Badge key={e}>{e}</Badge>) : <Badge tone="blue">all events</Badge>}
                </div>
                <div className="mt-2 font-mono text-[11px] text-muted">secret: {h.secret}</div>
              </div>
              <button onClick={() => remove(h.id)} className="shrink-0 text-muted hover:text-red-500"><Trash2 className="h-4 w-4" /></button>
            </div>
          ))}
        </div>
      )}

      {!!deliveries?.length && (
        <div className="space-y-2">
          <h3 className="text-sm font-semibold">Recent deliveries</h3>
          <div className="divide-y divide-border rounded-lg border border-border bg-card">
            {deliveries.slice(0, 12).map((d) => (
              <div key={d.id} className="flex items-center gap-3 px-4 py-2 text-sm">
                {d.ok ? <Check className="h-4 w-4 text-green" /> : <X className="h-4 w-4 text-red-500" />}
                <span className="font-mono text-xs">{d.event}</span>
                <span className="min-w-0 flex-1 truncate text-xs text-secondary">{d.url}</span>
                <Badge tone={d.ok ? "green" : "red"}>{d.status || "ERR"}</Badge>
                <span className="text-xs text-muted">{timeAgo(d.ts_ms)} ago</span>
              </div>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}
