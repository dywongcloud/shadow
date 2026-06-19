// Public URL for a deployment.
//
// Deployments are routed by their SUBDOMAIN (the first host label), so the same
// deployment is reachable at both `<sub>.localhost:8787` locally and
// `<sub>.<public-domain>` over a tunnel — the gateway treats them identically.
//
// When NEXT_PUBLIC_DEPLOYMENT_DOMAIN is set (e.g. an ngrok wildcard domain like
// `deployment.shadow.ngrok.pizza`), the dashboard links to the public HTTPS URL
// so deployments open from anywhere; otherwise it links to the local gateway.

const DEPLOY_DOMAIN = (process.env.NEXT_PUBLIC_DEPLOYMENT_DOMAIN || "").trim().replace(/^\.+|\.+$/g, "");

/** The host label (subdomain) for a deployment alias like "my-app.localhost". */
function subOf(alias: string): string {
  return alias.replace(/\.localhost$/i, "").split(".")[0];
}

/** The public host for a deployment (no scheme), e.g. `my-app.deployment.shadow.ngrok.pizza`
 *  when a domain is configured, else `my-app.localhost:8787`. */
export function deploymentHost(alias: string | undefined | null): string {
  if (!alias) return "";
  return DEPLOY_DOMAIN ? `${subOf(alias)}.${DEPLOY_DOMAIN}` : `${alias}:8787`;
}

/** The full clickable URL for a deployment. */
export function deploymentUrl(alias: string | undefined | null): string {
  if (!alias) return "#";
  return DEPLOY_DOMAIN ? `https://${subOf(alias)}.${DEPLOY_DOMAIN}/` : `http://${alias}:8787/`;
}

/** Whether a public deployment domain (tunnel) is configured. */
export const hasPublicDeployDomain = !!DEPLOY_DOMAIN;
