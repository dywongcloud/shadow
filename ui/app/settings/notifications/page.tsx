"use client";

import { useEffect, useState } from "react";
import Link from "next/link";
import { Bell, AtSign, Smartphone, Phone, BellOff, ArrowLeft } from "lucide-react";
import { Card, Switch, Badge } from "@/components/ui";
import { WithIdentity } from "@/components/identity";

const CHANNELS = [
  { key: "web", icon: <Bell className="h-4 w-4" />, title: "Web", desc: "Receive notifications in the Hive dashboard." },
  { key: "email", icon: <AtSign className="h-4 w-4" />, title: "Email", desc: "" },
  { key: "push", icon: <Smartphone className="h-4 w-4" />, title: "Push", desc: "Receive notifications on desktop or mobile." },
  { key: "sms", icon: <Phone className="h-4 w-4" />, title: "SMS", desc: "No phone number." },
];

const MATRIX: { group: string; rows: { label: string; cols: string[] }[]; cols: string[] }[] = [
  { group: "Anomaly Alerts", cols: ["Push", "Email", "Web"], rows: [{ label: "Default Alert Rule", cols: ["push", "email", "web"] }] },
  { group: "Usage", cols: ["Push", "Email", "Web"], rows: [
    { label: "75% of included credits", cols: ["push", "email", "web"] },
    { label: "On-Demand Usage Summary", cols: ["push", "email"] },
  ]},
  { group: "Team", cols: ["SMS", "Push", "Email", "Web"], rows: [
    { label: "Feature Requests", cols: ["push", "email", "web"] },
    { label: "Member Joined", cols: ["push", "email", "web"] },
  ]},
];

export default function NotificationsPage() {
  const [chan, setChan] = useState<Record<string, boolean>>({ web: true, email: true, push: false, sms: false });
  const [grid, setGrid] = useState<Record<string, boolean>>({});

  useEffect(() => {
    const s = localStorage.getItem("hive_notif");
    if (s) try { const v = JSON.parse(s); setChan(v.chan ?? chan); setGrid(v.grid ?? {}); } catch {}
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  function persist(nextChan = chan, nextGrid = grid) {
    localStorage.setItem("hive_notif", JSON.stringify({ chan: nextChan, grid: nextGrid }));
  }
  function setChannel(k: string, v: boolean) { const n = { ...chan, [k]: v }; setChan(n); persist(n, grid); }
  function cell(rowKey: string, col: string) {
    const k = `${rowKey}.${col}`;
    return grid[k] ?? true; // default checked, like the screenshot
  }
  function setCell(rowKey: string, col: string, v: boolean) {
    const n = { ...grid, [`${rowKey}.${col}`]: v }; setGrid(n); persist(chan, n);
  }

  return (
    <div>
      <Link href="/settings" className="mb-3 inline-flex items-center gap-1.5 text-sm text-link hover:underline"><ArrowLeft className="h-4 w-4" /> Team Settings</Link>
      <h1 className="text-2xl font-semibold tracking-tight">Notifications</h1>
      <p className="mb-6 mt-1 text-sm text-secondary">Manage your personal notification settings for this team.</p>

      {/* Channels */}
      <Card className="p-0">
        {CHANNELS.map((c, i) => (
          <div key={c.key} className={`flex items-center justify-between px-5 py-4 ${i ? "border-t border-border" : ""}`}>
            <div className="flex items-center gap-3">
              <span className="flex h-9 w-9 items-center justify-center rounded-full bg-subtle text-secondary">{c.icon}</span>
              <div>
                <div className="flex items-center gap-2 text-sm font-medium">
                  {c.title}
                  {c.key === "push" && <Badge tone="amber">Subscribe Device</Badge>}
                </div>
                <div className="text-xs text-secondary">
                  {c.key === "email" ? <WithIdentity>{(id) => <>{id.email || "your email"}</>}</WithIdentity> : c.desc}
                </div>
              </div>
            </div>
            <Switch checked={!!chan[c.key]} onChange={(v) => setChannel(c.key, v)} label={c.title} />
          </div>
        ))}
        <div className="flex items-center justify-between border-t border-border px-5 py-4">
          <div className="flex items-center gap-3">
            <span className="flex h-9 w-9 items-center justify-center rounded-full bg-subtle text-secondary"><BellOff className="h-4 w-4" /></span>
            <div>
              <div className="text-sm font-medium">Mute</div>
              <div className="text-xs text-secondary">Select projects to mute notifications for.</div>
            </div>
          </div>
          <span className="text-sm text-muted">No projects</span>
        </div>
      </Card>

      {/* Matrix */}
      <div className="mt-8 space-y-8">
        {MATRIX.map((m) => (
          <div key={m.group}>
            <div className="mb-2 flex items-center justify-between border-b border-border pb-2">
              <span className="text-sm font-semibold">{m.group}</span>
              <div className="flex gap-6 text-xs font-medium text-muted">{m.cols.map((c) => <span key={c} className="w-10 text-center">{c}</span>)}</div>
            </div>
            {m.rows.map((row) => {
              const rowKey = `${m.group}:${row.label}`;
              return (
                <div key={row.label} className="flex items-center justify-between border-b border-border py-2.5 text-sm last:border-0">
                  <span className="text-secondary">{row.label}</span>
                  <div className="flex gap-6">
                    {m.cols.map((col) => {
                      const lc = col.toLowerCase();
                      const available = row.cols.includes(lc);
                      return (
                        <div key={col} className="flex w-10 justify-center">
                          {available ? (
                            <input type="checkbox" checked={cell(rowKey, lc)} onChange={(e) => setCell(rowKey, lc, e.target.checked)} />
                          ) : (
                            <span className="h-4 w-4 rounded border border-border-strong/40" />
                          )}
                        </div>
                      );
                    })}
                  </div>
                </div>
              );
            })}
          </div>
        ))}
      </div>
    </div>
  );
}
