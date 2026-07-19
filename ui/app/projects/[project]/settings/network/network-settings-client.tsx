"use client";

import { use, useMemo } from "react";
import Link from "next/link";
import { Globe, Server } from "lucide-react";
import { SettingCard } from "@/components/ui";
import { RawPortConnections } from "@/components/raw-port-connections";
import { usePoll, type Deployment } from "@/lib/api";
import { deploymentHost } from "@/lib/deploy-url";

/**
 * Read-only network/connection overview for a project: the current
 * production deployment's HTTP domain plus, for a raw-protocol deployment
 * (Postgres, Minecraft, any container declaring tcp/udp/grpc), its allocated
 * public port(s) as ready-to-copy connection addresses. Protocol/port are
 * set at DEPLOY time (New Project's image-deploy protocol selector, or a
 * Dockerfile project's `fluid.json` `container` block) — the same
 * "changes take effect on the next deployment" model this settings area
 * already uses for CPU/Memory (see the Functions tab's footer note), so
 * changing them here would need its own apply-at-redeploy plumbing this
 * page doesn't (yet) have; it links to Redeploy instead of pretending to
 * edit in place.
 */
export function NetworkSettings({ paramsPromise }: { paramsPromise: Promise<{ project: string }> }) {
  const params = use(paramsPromise);
  const project = decodeURIComponent(params.project);
  const { data: deps } = usePoll<Deployment[]>("/deployments", 5000);

  const dep = useMemo(() => {
    const mine = (deps ?? []).filter((d) => d.project === project);
    return mine.find((d) => d.production) ?? mine.sort((a, b) => b.created_at_ms - a.created_at_ms)[0] ?? null;
  }, [deps, project]);

  const host = dep ? deploymentHost(dep.alias, dep.region_code) : "";
  const hasRawPorts = !!dep?.raw_ports?.length;

  return (
    <div className="flex flex-col gap-6">
      <SettingCard title="Domain" desc="The production HTTP address for this project.">
        {host ? (
          <span className="inline-flex items-center gap-1.5 font-mono text-sm">
            <Globe className="h-3.5 w-3.5 shrink-0 text-muted" /> {host}
          </span>
        ) : (
          <span className="text-sm text-muted">No production deployment yet.</span>
        )}
      </SettingCard>

      <SettingCard
        title="Raw TCP / UDP connections"
        desc="A raw-protocol service (a database, a game server like Minecraft) has no Host header to route on, so it's reached through its own allocated public port instead — connect with a plain host:port address, same as any Postgres/Redis client or a game client's Server Address field."
        footer="Protocol and port are set when the deployment is built (New Project's protocol selector, or fluid.json's container block) — redeploy with a different override to change them."
        footerAction={
          <Link href={`/projects/${encodeURIComponent(project)}`} className="text-sm text-link hover:underline">
            Redeploy with different settings →
          </Link>
        }
      >
        {!dep ? (
          <span className="text-sm text-muted">No deployment yet.</span>
        ) : hasRawPorts ? (
          <RawPortConnections deployment={dep} />
        ) : (
          <span className="inline-flex items-center gap-1.5 text-sm text-muted">
            <Server className="h-3.5 w-3.5 shrink-0" /> This deployment speaks HTTP only — no raw ports allocated.
          </span>
        )}
      </SettingCard>
    </div>
  );
}
