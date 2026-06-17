"use client";

import { useState } from "react";
import { Card, Badge, Button, Input, Switch, PageHeader, Table, Th, Td } from "@/components/ui";
import { apiSend, usePoll, type WafRule } from "@/lib/api";

interface WafConfig { managed: boolean; rules: WafRule[] }
interface BotPolicy { allow_good: boolean; block_bad: boolean }

export default function FirewallPage() {
  const { data: waf, refresh } = usePoll<WafConfig>("/v1/waf", 4000);
  const { data: bot, refresh: refreshBot } = usePoll<BotPolicy>("/v1/bot", 4000);

  const [id, setId] = useState("");
  const [desc, setDesc] = useState("");
  const [contains, setContains] = useState("");
  const [ip, setIp] = useState("");

  async function addRule() {
    if (!id) return;
    const rule: WafRule = {
      id,
      description: desc || id,
      action: "deny",
      enabled: true,
      when: {
        contains: contains || undefined,
        ip_prefix: ip || undefined,
      },
    };
    await apiSend("POST", "/v1/waf/rules", rule);
    setId(""); setDesc(""); setContains(""); setIp("");
    refresh();
  }

  return (
    <div>
      <PageHeader title="Firewall" desc="Web Application Firewall + bot management at the edge" />

      <Card className="mb-4 flex items-center justify-between">
        <div>
          <div className="text-sm font-medium text-fg">Managed ruleset</div>
          <div className="text-xs text-muted">OWASP-style SQLi / XSS / path-traversal signatures</div>
        </div>
        <Switch
          checked={!!waf?.managed}
          onChange={async (v) => { await apiSend("PUT", "/v1/waf/managed", { enabled: v }); refresh(); }}
        />
      </Card>

      <Card className="mb-4">
        <div className="mb-4 text-sm font-medium text-fg">Bot management</div>
        <div className="flex flex-col gap-4 sm:flex-row sm:gap-10">
          <label className="flex cursor-pointer items-center gap-3 text-sm text-secondary">
            <Switch
              label="Allow verified bots"
              checked={!!bot?.allow_good}
              onChange={async (v) => { await apiSend("PUT", "/v1/bot", { ...bot, allow_good: v }); refreshBot(); }}
            />
            Allow verified bots (Googlebot, Slackbot…)
          </label>
          <label className="flex cursor-pointer items-center gap-3 text-sm text-secondary">
            <Switch
              label="Block bad bots"
              checked={!!bot?.block_bad}
              onChange={async (v) => { await apiSend("PUT", "/v1/bot", { ...bot, block_bad: v }); refreshBot(); }}
            />
            Block bad bots (scrapers, scanners)
          </label>
        </div>
      </Card>

      <Card className="mb-4">
        <div className="mb-3 text-sm font-medium text-fg">Add a custom deny rule</div>
        <div className="grid grid-cols-2 gap-3 md:grid-cols-4">
          <Input placeholder="rule id" value={id} onChange={(e) => setId(e.target.value)} />
          <Input placeholder="description" value={desc} onChange={(e) => setDesc(e.target.value)} />
          <Input placeholder="path/query contains…" value={contains} onChange={(e) => setContains(e.target.value)} />
          <Input placeholder="ip prefix e.g. 10.0." value={ip} onChange={(e) => setIp(e.target.value)} />
        </div>
        <div className="mt-3"><Button onClick={addRule}>Add rule</Button></div>
      </Card>

      <div className="mb-2 text-sm font-medium text-fg">Custom rules</div>
      <Table>
        <thead>
          <tr><Th>ID</Th><Th>Action</Th><Th>Match</Th><Th>Enabled</Th><Th></Th></tr>
        </thead>
        <tbody>
          {(waf?.rules ?? []).map((r) => (
            <tr key={r.id}>
              <Td className="font-mono text-xs">{r.id}<div className="text-muted">{r.description}</div></Td>
              <Td><Badge tone={r.action === "deny" ? "red" : r.action === "allow" ? "green" : "amber"}>{r.action}</Badge></Td>
              <Td className="font-mono text-xs text-muted">
                {r.when.contains ? `contains "${r.when.contains}"` : ""}
                {r.when.ip_prefix ? ` ip ${r.when.ip_prefix}*` : ""}
                {r.when.path_regex ? ` path ~ ${r.when.path_regex}` : ""}
              </Td>
              <Td>{r.enabled ? <Badge tone="green">on</Badge> : <Badge>off</Badge>}</Td>
              <Td><Button variant="danger" onClick={async () => { await apiSend("DELETE", `/v1/waf/rules/${r.id}`); refresh(); }}>Delete</Button></Td>
            </tr>
          ))}
          {!waf?.rules?.length && <tr><Td className="text-muted">No custom rules — the managed ruleset is active.</Td></tr>}
        </tbody>
      </Table>
    </div>
  );
}
