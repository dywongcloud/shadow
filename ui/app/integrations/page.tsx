"use client";

import { useEffect, useState } from "react";
import { Github, Search, Plus, Database, Box, Lock, CreditCard } from "lucide-react";
import { Card, Button, Input, Badge, PageHeader, Triangle } from "@/components/ui";

function Logo({ kind }: { kind: string }) {
  const wrap = "flex h-10 w-10 items-center justify-center rounded-lg border border-border bg-subtle";
  if (kind === "github") return <span className={wrap}><Github className="h-5 w-5" /></span>;
  if (kind === "postgres") return <span className={wrap}><Database className="h-5 w-5" /></span>;
  if (kind === "kv") return <span className={wrap}><Box className="h-5 w-5" /></span>;
  if (kind === "blob") return <span className={wrap}><Box className="h-5 w-5" /></span>;
  if (kind === "stripe") return <span className={wrap}><CreditCard className="h-5 w-5" /></span>;
  if (kind === "auth0") return <span className={wrap}><Lock className="h-5 w-5" /></span>;
  return <Triangle className="h-10 w-10" />;
}

export default function IntegrationsPage() {
  const [gh, setGh] = useState<{ configured: boolean; connected: boolean }>({ configured: false, connected: false });
  useEffect(() => {
    fetch("/api/github/status").then((r) => r.json()).then((s) => setGh({ configured: !!s.configured, connected: !!s.connected })).catch(() => {});
  }, []);

  async function connect() {
    const r = await fetch("/api/github/connect", { method: "POST" });
    const d = await r.json();
    if (d.redirectUrl) window.location.href = d.redirectUrl;
  }

  const connected = [
    { kind: "github", name: "GitHub", desc: "Deploy projects automatically with Git integration", active: gh.connected },
    { kind: "postgres", name: "Hive Postgres", desc: "Serverless SQL database built for the edge", active: true },
    { kind: "kv", name: "Hive KV", desc: "Durable Redis database for caching", active: true },
  ];
  const available = [
    { kind: "blob", name: "Hive Blob", desc: "Fast object storage for your files" },
    { kind: "stripe", name: "Stripe", desc: "Accept payments and manage subscriptions" },
    { kind: "auth0", name: "Auth0", desc: "Add authentication to your applications" },
  ];

  return (
    <div>
      <PageHeader
        title="Integrations"
        action={<Button><Plus className="h-4 w-4" /> Browse Marketplace</Button>}
      />
      <div className="relative mb-8 max-w-xl">
        <Search className="absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted" />
        <Input placeholder="Search integrations…" className="pl-9" />
      </div>

      <h2 className="mb-3 text-base font-semibold">Connected</h2>
      <div className="mb-10 grid grid-cols-1 gap-4 md:grid-cols-3">
        {connected.map((i) => (
          <Card key={i.name} className="p-5">
            <div className="mb-3 flex items-start justify-between">
              <div className="flex items-center gap-3">
                <Logo kind={i.kind} />
                <div>
                  <div className="font-semibold">{i.name}</div>
                  <div className="text-xs text-muted">{i.kind === "github" ? (gh.connected ? "connected" : "not connected") : "hive-cloud"}</div>
                </div>
              </div>
              <Badge tone={i.active ? "green" : "default"}>{i.active ? "Active" : "Inactive"}</Badge>
            </div>
            <p className="mb-4 text-sm text-secondary">{i.desc}</p>
            <div className="flex items-center gap-3">
              {i.kind === "github" && !gh.connected && gh.configured ? (
                <Button onClick={connect}>Connect</Button>
              ) : (
                <Button variant="outline">Configure</Button>
              )}
              <button className="text-sm text-secondary hover:text-fg">Disconnect</button>
            </div>
          </Card>
        ))}
      </div>

      <h2 className="mb-3 text-base font-semibold">Available Integrations</h2>
      <div className="grid grid-cols-1 gap-4 md:grid-cols-3">
        {available.map((i) => (
          <Card key={i.name} className="p-5">
            <div className="mb-3 flex items-center gap-3">
              <Logo kind={i.kind} />
              <div>
                <div className="font-semibold">{i.name}</div>
                <div className="text-sm text-secondary">{i.desc}</div>
              </div>
            </div>
            <Button variant="outline" className="w-full"><Plus className="h-4 w-4" /> Add Integration</Button>
          </Card>
        ))}
      </div>
    </div>
  );
}
