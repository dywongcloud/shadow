/**
 * Clerk's frontend-API origin from the publishable key, whose payload IS the frontend host (pk_test_ -> <slug>.clerk.accounts.dev, pk_live_ -> clerk.<our-domain>).
 * Shared by `next.config.mjs` (the CSP origins) and `app/layout.tsx` (the preconnect hint) so the two cannot drift; plain `.mjs` because the config file is ESM
 * JavaScript and cannot import TypeScript. `null` when no key is configured or the payload is not a bare hostname: a malformed env value never becomes a URL we emit.
 * @param {string | undefined} publishableKey
 * @returns {string | null}
 */
export function clerkFrontendOrigin(publishableKey) {
  const pk = publishableKey || "";
  const m = /^pk_(?:test|live)_([A-Za-z0-9+/=]+)$/.exec(pk);
  if (!m) return null;
  try {
    const host = Buffer.from(m[1], "base64").toString("utf8").replace(/\$$/, "").trim();
    if (/^[a-z0-9](?:[a-z0-9.-]*[a-z0-9])?$/i.test(host)) return `https://${host}`;
  } catch {}
  return null;
}
