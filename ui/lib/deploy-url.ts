// Public URL for a deployment. Deployments are routed by their SUBDOMAIN (the first host label), so one deployment is reachable at
// `<sub>.localhost:8787` locally and `<sub>.<public-domain>` publicly. The domain is resolved per SESSION at runtime: a dashboard opened over
// localhost targets the local gateway, any other host uses NEXT_PUBLIC_DEPLOYMENT_DOMAIN, so one build serves both contexts.

const DEPLOY_DOMAIN = (process.env.NEXT_PUBLIC_DEPLOYMENT_DOMAIN || "").trim().replace(/^\.+|\.+$/g, "");

const INGRESS = (process.env.NEXT_PUBLIC_INGRESS || "dns").trim().toLowerCase();

/** True for any host that is the local machine (localhost, *.localhost, loopback IPs). */
function isLocalHostname(h: string): boolean {
  const host = h.toLowerCase();
  return (
    host === "localhost" ||
    host.endsWith(".localhost") ||
    host === "127.0.0.1" ||
    host === "::1" ||
    host === "[::1]" ||
    host === "0.0.0.0"
  );
}

/** The deployment domain for the CURRENT session: a localhost dashboard targets the local gateway (empty domain), any other host the configured
 *  public domain (SSR falls back to the build-time domain; links are produced after hydration). */
function activeDeployDomain(): string {
  if (typeof window !== "undefined") {
    const h = window.location.hostname;
    if (isLocalHostname(h)) return "";
    return DEPLOY_DOMAIN || `deployment.${h}`;
  }
  return DEPLOY_DOMAIN;
}

/** The host label (subdomain) for a deployment alias like "my-app.localhost". */
function subOf(alias: string): string {
  return alias.replace(/\.localhost$/i, "").split(".")[0];
}

/** Region-affinity ingress: a deployment with a region code (iad/sin/sfo/lax) gets `<sub>.<code>.ngrok.pizza` so the request enters on a node in
 *  that region; empty or unknown codes use the region-agnostic zone, and a localhost session never encodes a region. */
function publicDeployDomain(regionCode?: string | null): string {
  const domain = activeDeployDomain();
  if (!domain) return "";
  if (INGRESS !== "ngrok") return domain;
  const code = (regionCode || "").trim().toLowerCase();
  return code ? `${code}.ngrok.pizza` : domain;
}

/** The public host for a deployment (no scheme), e.g. `my-app.iad.ngrok.pizza` on a
 *  tunnel session (region-encoded), else `my-app.localhost:8787` on a localhost session. */
export function deploymentHost(alias: string | undefined | null, regionCode?: string | null): string {
  if (!alias) return "";
  const domain = publicDeployDomain(regionCode);
  return domain ? `${subOf(alias)}.${domain}` : `${subOf(alias)}.localhost:8787`;
}

/** The full clickable URL for a deployment. */
export function deploymentUrl(alias: string | undefined | null, regionCode?: string | null): string {
  if (!alias) return "#";
  const domain = publicDeployDomain(regionCode);
  return domain ? `https://${subOf(alias)}.${domain}/` : `http://${subOf(alias)}.localhost:8787/`;
}

/** Whether a public deployment domain (tunnel) is configured at build time. */
export const hasPublicDeployDomain = !!DEPLOY_DOMAIN;

/** The public host (no port) for a deployment on the CURRENT session: the same domain resolution as `deploymentHost`/`deploymentUrl` without the
 *  HTTP gateway's `:8787`, because a raw TCP/UDP/gRPC binding listens on its OWN allocated port (`raw_ports[i].public_port`). */
function rawPortHost(alias: string | undefined | null, regionCode?: string | null): string {
  if (!alias) return "";
  const domain = publicDeployDomain(regionCode);
  return domain ? `${subOf(alias)}.${domain}` : `${subOf(alias)}.localhost`;
}

/** A ready-to-copy `host:port` connection address for one raw port binding (`Deployment.raw_ports[i]`), the literal form every raw client
 *  accepts; every fleet node binds every allocated raw port, so the normal domain/alias resolution applies. */
export function rawPortAddress(alias: string | undefined | null, publicPort: number, regionCode?: string | null): string {
  const host = rawPortHost(alias, regionCode);
  return host ? `${host}:${publicPort}` : "";
}

/** Whether the anonymous-membership preview flow is enabled in the UI. */
export const zkEnabled = process.env.NEXT_PUBLIC_ZKAUTH === "1";

/** Opens a deployment. With the zkauth experiment on it first asks the home node for an anonymous membership proof and routes through the
 *  gateway's bootstrap endpoint (which drops a short-lived access cookie) so a protected preview opens without leaking identity to the serving
 *  node; any failure falls back to a plain open. */
export async function openDeployment(alias: string | undefined | null, project: string, regionCode?: string | null) {
  const base = deploymentUrl(alias, regionCode).replace(/\/$/, "");
  if (zkEnabled && alias) {
    try {
      const { apiSend, currentTeam } = await import("@/lib/api");
      const userId = typeof window !== "undefined" ? localStorage.getItem("hive_uid") || "" : "";
      if (userId) {
        const r = await apiSend<{ proof?: string; message?: string }>("POST", "/v1/zkauth/preview-proof", {
          user_id: userId,
          project,
        });
        if (r?.proof) {
          const qs = `p=${r.proof}&m=${encodeURIComponent(r.message || "")}&t=${encodeURIComponent(currentTeam())}`;
          window.open(`${base}/_shadw/zk?${qs}`, "_blank", "noopener");
          return;
        }
      }
    } catch {}
  }
  window.open(`${base}/`, "_blank", "noopener");
}

/** The alias a deployment ITSELF answers on, never someone else's URL: production owns the project alias; a preview uses its commit, branch or
 *  per-deployment alias (commit first: it is minted on every fanout target, while the id alias resolves only on its host node). "" when a
 *  preview has none: callers render a pending state, because a preview must NEVER display the production URL. */
export function deploymentSelfAlias(d: {
  production: boolean;
  alias: string;
  commit_alias?: string;
  branch_alias?: string;
  id_alias?: string;
}): string {
  if (d.production) return d.alias;
  return d.commit_alias || d.branch_alias || d.id_alias || "";
}
