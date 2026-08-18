// Public URL for a deployment.
//
// Deployments are routed by their SUBDOMAIN (the first host label), so the same
// deployment is reachable at both `<sub>.localhost:8787` locally and
// `<sub>.<public-domain>` over a tunnel — the gateway treats them identically.
//
// The domain is resolved per-SESSION at runtime, not just at build time:
//   • When the dashboard itself is opened over localhost (e.g. localhost:3002),
//     deployment links point at the local gateway: `<sub>.localhost:8787`.
//   • When the dashboard is opened on the public platform host (shadw.cloud),
//     links use NEXT_PUBLIC_DEPLOYMENT_DOMAIN so they open from anywhere:
//     `<sub>.shadw.app`.
// This way one build serves both contexts correctly — a localhost session never
// hands out tunnel URLs and vice-versa.

const DEPLOY_DOMAIN = (process.env.NEXT_PUBLIC_DEPLOYMENT_DOMAIN || "").trim().replace(/^\.+|\.+$/g, "");

// Ingress mode (mirrors the node's HIVE_INGRESS): "dns" (default — ngrok is
// retired) emits plain `<sub>.<apps-domain>` — Vercel-DNS wildcards match ONE
// label, and regional steering happens inside the edge after connect, not in
// the hostname. "ngrok"/"dual" remain accepted as explicit break-glass values.
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

/**
 * The deployment domain to use for the CURRENT session. In the browser this is
 * decided from the dashboard's own hostname: a localhost session always targets
 * the local gateway (empty domain), while any other host (a tunnel) uses the
 * configured public domain. During SSR (no `window`) we fall back to the
 * build-time domain; the dashboards load deployment data client-side, so links
 * are produced after hydration when `window` is available.
 */
function activeDeployDomain(): string {
  if (typeof window !== "undefined") {
    const h = window.location.hostname;
    if (isLocalHostname(h)) return "";
    // Tunnel/public session: prefer the configured public domain. If it didn't
    // get inlined into this chunk (NEXT_PUBLIC_* are baked at compile time, and a
    // dev server can serve a chunk compiled before the env was set), derive it
    // from the dashboard's own apex host so links never wrongly fall back to a
    // localhost gateway URL on a public session.
    return DEPLOY_DOMAIN || `deployment.${h}`;
  }
  return DEPLOY_DOMAIN;
}

/** The host label (subdomain) for a deployment alias like "my-app.localhost". */
function subOf(alias: string): string {
  return alias.replace(/\.localhost$/i, "").split(".")[0];
}

/**
 * Region-affinity ingress: when a deployment carries a region code (iad/sin/sfo/lax),
 * its PRIMARY public URL is `<sub>.<code>.ngrok.pizza` so the request enters directly
 * on a node in that region (no cross-region entry hop). Falls back to the legacy
 * region-agnostic zone for empty/unknown codes, and to localhost on a local session.
 */
function publicDeployDomain(regionCode?: string | null): string {
  const domain = activeDeployDomain();
  if (!domain) return ""; // localhost session — never region-encode
  if (INGRESS !== "ngrok") return domain; // real-DNS ingress: single-label alias on the apps domain
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

/** The public host (no port) for a deployment on the CURRENT session — the
 *  same domain resolution `deploymentHost`/`deploymentUrl` use, but without
 *  the HTTP gateway's own `:8787` suffix, since a raw TCP/UDP/gRPC binding
 *  listens on its OWN allocated port (`raw_ports[i].public_port`), never the
 *  shared HTTP gateway port. */
function rawPortHost(alias: string | undefined | null, regionCode?: string | null): string {
  if (!alias) return "";
  const domain = publicDeployDomain(regionCode);
  return domain ? `${subOf(alias)}.${domain}` : `${subOf(alias)}.localhost`;
}

/**
 * A ready-to-copy `host:port` connection address for one raw port binding
 * (`Deployment.raw_ports[i]`) — the literal format every raw client accepts
 * (psql's "host:port", redis-cli's `-h host -p port`, a Minecraft client's
 * "Server Address" field). Every fleet node binds every allocated raw port,
 * so this is reachable through the SAME domain/alias resolution HTTP
 * deployment links already use — no separate DNS story needed.
 */
export function rawPortAddress(alias: string | undefined | null, publicPort: number, regionCode?: string | null): string {
  const host = rawPortHost(alias, regionCode);
  return host ? `${host}:${publicPort}` : "";
}

// ---- EXPERIMENT: anonymous preview access (NEXT_PUBLIC_ZKAUTH=1) ----

/** Whether the anonymous-membership preview flow is enabled in the UI. */
export const zkEnabled = process.env.NEXT_PUBLIC_ZKAUTH === "1";

/**
 * Open a deployment. With the zkauth experiment on, this first asks the home node
 * to mint an anonymous membership proof and routes through the gateway's bootstrap
 * endpoint (which drops a short-lived access cookie) so a protected preview opens
 * without leaking identity to the serving node. Falls back to a plain open if the
 * flag is off, no proof is available, or anything fails.
 */
export async function openDeployment(alias: string | undefined | null, project: string, regionCode?: string | null) {
  const base = deploymentUrl(alias, regionCode).replace(/\/$/, "");
  if (zkEnabled && alias) {
    try {
      // Imported lazily to avoid pulling the api client into pure-URL callers.
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
    } catch {
      /* fall through to a normal open */
    }
  }
  window.open(`${base}/`, "_blank", "noopener");
}

/**
 * The alias a deployment ITSELF answers on — never someone else's URL.
 * Production deployments own the project alias. A preview's own hosts are its
 * commit / branch / per-deployment aliases (commit first: it is minted on
 * every fanout target, so it routes through pooled ingress; the per-node id
 * alias only resolves on its host node). Returns "" when a preview has none
 * of them — callers render a disabled/pending state, because the one thing a
 * preview must NEVER display is the production URL.
 */
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
