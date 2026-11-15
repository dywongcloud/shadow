import "server-only";
import { ownerEmails } from "@/lib/owner-emails";

const REQUEST_TIMEOUT_MS = 10_000;
const TOKEN_TTL_FALLBACK_SECS = 3600;

export type MintedOperator = { token: string; expires_in: number };

/** Reads the UNSIGNED payload of a JWT this server just minted, to CHECK whether the backend
 *  granted the operator claim. This only SELECTS a credential; it authorizes nothing. The
 *  backend re-verifies the signature and the role gate on every request, so a forged, stale or
 *  absent claim grants no access. */
function mintedPlatformAdmin(token: string): boolean {
  try {
    const segment = token.split(".")[1];
    if (!segment) return false;
    const base64 = segment.replace(/-/g, "+").replace(/_/g, "/");
    const padded = base64.padEnd(Math.ceil(base64.length / 4) * 4, "=");
    return (JSON.parse(atob(padded)) as { platform_admin?: unknown }).platform_admin === true;
  } catch {
    return false;
  }
}

/** Mints a platform-operator JWT with `x-hive-internal`. The dashboard's HIVE_OWNER_EMAIL
 *  (`ui/.env.local`, rendered from group_vars) and the node's `admin_emails`
 *  (HIVE_ADMIN_EMAILS + HIVE_OWNER_EMAIL, which a per-node systemd drop-in overrides) come from
 *  DIFFERENT sources. `ownerEmails()[0]` is therefore NOT necessarily a platform admin on this
 *  node: a 200 mint can carry `platform_admin: false`, and every operator-gated ops GET then
 *  answers 403. Try each configured owner and keep the first token the backend actually marked
 *  as an operator. With no operator mint, return the first successful one so an unenforced
 *  (dev) node -- where `require_operator` is open -- keeps its old behaviour, and the caller's
 *  session cookie can take over on an enforced node. An empty owner list mints once email-less. */
export async function mintOperatorBearer(opts: {
  admin: string;
  internal: string;
  sub: string;
  tenant: string;
  role: string;
}): Promise<MintedOperator | null> {
  if (!opts.internal) return null;
  let fallback: MintedOperator | null = null;
  // No configured owner is normal on a local dev node: mint once without an email, so an
  // unenforced node keeps accepting the token exactly as it did before this helper existed.
  const candidates = ownerEmails();
  const emails: Array<string | undefined> = candidates.length ? candidates : [undefined];
  for (const email of emails) {
    try {
      const r = await fetch(`${opts.admin}/v1/token`, {
        method: "POST",
        headers: { "content-type": "application/json", "x-hive-internal": opts.internal },
        body: JSON.stringify({
          sub: opts.sub,
          tenant: opts.tenant,
          role: opts.role,
          ...(email ? { email } : {}),
        }),
        cache: "no-store",
        signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
      });
      if (!r.ok) continue;
      const d = (await r.json()) as { token?: string; expires_in?: number };
      if (!d.token) continue;
      const minted: MintedOperator = {
        token: d.token,
        expires_in: typeof d.expires_in === "number" ? d.expires_in : TOKEN_TTL_FALLBACK_SECS,
      };
      if (mintedPlatformAdmin(d.token)) return minted;
      fallback ??= minted;
    } catch {
      // Try the next configured owner; one unreachable mint is not a failed ops read.
    }
  }
  return fallback;
}
