"use client";

import { useState } from "react";
import { Badge, Triangle } from "@/components/ui";
import { usePoll, type Event } from "@/lib/api";

function tone(a: string) {
  if (a.includes("deny") || a.includes("block") || a === "throttled" || a === "delete") return "red" as const;
  if (a.includes("cache")) return "blue" as const;
  if (a === "cron" || a === "redirect" || a === "rewrite" || a === "deploy" || a === "domain-add") return "amber" as const;
  return "green" as const;
}

function describe(e: Event): string {
  switch (e.action) {
    case "deploy": return `deployed to ${e.host}`;
    case "domain-add": return `added domain ${e.host} to ${e.detail}`;
    case "cron": return `ran cron ${e.path}`;
    case "waf-deny": return `WAF blocked ${e.method} ${e.path}`;
    case "bot-block": return `blocked a bot at ${e.path}`;
    case "throttled": return `throttled ${e.path} (FUNCTION_THROTTLED)`;
    case "cache-hit": case "cache-stale": case "cache-revalidate": return `served ${e.path} from cache`;
    case "redirect": return `redirected ${e.path} → ${e.detail}`;
    case "rewrite": return `rewrote ${e.path} → ${e.detail}`;
    case "preview-protected": return `blocked an anonymous preview request to ${e.host}`;
    case "delete": return e.detail || `deleted ${e.project}`;
    default: return `${e.method} ${e.path} → ${e.status}`;
  }
}

export function ActivityFeed() {
  const { data: events } = usePoll<Event[]>("/v1/logs?limit=300", 2500);
  const [filter, setFilter] = useState("");

  const list = (events ?? []).filter((e) => !filter || e.action === filter);

  const groups = new Map<string, Event[]>();
  for (const e of list) {
    const d = new Date(e.ts_ms);
    const key = d.toLocaleDateString("en-US", { month: "long", year: "numeric" });
    if (!groups.has(key)) groups.set(key, []);
    groups.get(key)!.push(e);
  }

  return (
    <div>
      <div className="mb-4 flex items-center justify-end">
        <select
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          className="rounded-md border border-border bg-card px-3 py-2 text-sm focus:outline-none"
        >
          <option value="">Filter by Event</option>
          <option value="deploy">Deploys</option>
          <option value="domain-add">Domains</option>
          <option value="waf-deny">WAF blocks</option>
          <option value="bot-block">Bot blocks</option>
          <option value="cron">Cron</option>
          <option value="throttled">Throttled</option>
        </select>
      </div>

      {Array.from(groups.entries()).map(([month, evs]) => (
        <div key={month} className="mb-8">
          <h3 className="mb-3 text-sm font-semibold">{month}</h3>
          <div className="flex flex-col">
            {evs.map((e, i) => (
              <div key={i} className="flex items-start gap-3 border-b border-border py-3 last:border-0">
                <Triangle className="mt-0.5 h-7 w-7 shrink-0" />
                <div className="min-w-0 flex-1">
                  <div className="text-sm">
                    <span className="font-medium">node-{e.region}</span>{" "}
                    <span className="text-secondary">{describe(e)}</span>
                  </div>
                  {e.detail && !["deploy", "domain-add", "redirect", "rewrite"].includes(e.action) ? (
                    <div className="text-xs text-muted">{e.detail}</div>
                  ) : null}
                </div>
                <Badge tone={tone(e.action)}>{e.action}</Badge>
                <span className="w-20 shrink-0 text-right text-xs text-muted">
                  {new Date(e.ts_ms).toLocaleDateString("en-US", { month: "short", day: "numeric" })}
                </span>
              </div>
            ))}
          </div>
        </div>
      ))}
      {!list.length && <div className="text-sm text-secondary">No activity yet.</div>}
    </div>
  );
}
