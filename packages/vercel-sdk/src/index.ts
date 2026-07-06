/**
 * @openedge/vercel-sdk — a fork of the upstream `@vercel/sdk`, repointed at the
 * OpenEdge platform API and authenticated with an OpenEdge platform key
 * (`hive_…`) instead of a Vercel access token.
 *
 * The public ergonomics mirror `@vercel/sdk`: construct a `Vercel` client with
 * `{ serverURL, bearerToken }` and call namespaced sub-APIs. The key change from
 * upstream is that `serverURL` defaults to (and is meant to point at) YOUR
 * OpenEdge deployment, and `bearerToken` is a `hive_…` platform API key — so
 * every request is automatically scoped to that key's team. A new `integrations`
 * namespace exposes the integrations a team connected via Composio on the
 * Integrations tab, including their credentials and the env vars they inject.
 *
 * Zero runtime dependencies (uses the global `fetch`), so it runs in Node 18+,
 * edge runtimes, Deno, Bun, and the browser.
 */

export interface SDKOptions {
  /**
   * Base URL of the OpenEdge platform API. Defaults to `OPENEDGE_API_URL` /
   * `VERCEL_SDK_SERVER_URL` env, else `http://127.0.0.1:8786`. In production point
   * this at your platform's public API origin.
   */
  serverURL?: string;
  /**
   * Platform API key (`hive_…`). Defaults to `OPENEDGE_API_KEY` /
   * `VERCEL_TOKEN` env. The token's team scopes every request.
   */
  bearerToken?: string;
  /** Optional explicit team override (sent as `x-hive-team`). Usually unnecessary
   *  — the bearer token already determines the team. */
  team?: string;
  /** Override the fetch implementation (tests / custom runtimes). */
  fetch?: typeof fetch;
}

/** A connected integration, redacted view (no secret values). */
export interface Integration {
  id: string;
  team: string;
  /** Composio toolkit slug, e.g. "stripe", "github", "notion". */
  provider: string;
  name: string;
  /** Auth shape: "oauth" | "api_key" | "none". */
  kind: string;
  /** Names of the env vars this integration injects (values are not exposed here). */
  envKeys: string[];
  hasCredentials: boolean;
  createdMs: number;
  updatedMs: number;
}

/** A connected integration with its secret credentials + env values resolved. */
export interface IntegrationCredentials {
  id: string;
  provider: string;
  name: string;
  kind: string;
  /** Raw auth material (access_token / api_key / …). */
  credentials: Record<string, string>;
  /** Env var KEY -> value mapping (the same vars injected into deployments). */
  env: Record<string, string>;
}

export class VercelError extends Error {
  readonly status: number;
  readonly body: string;
  constructor(message: string, status: number, body: string) {
    super(message);
    this.name = "VercelError";
    this.status = status;
    this.body = body;
  }
}

function trimSlash(s: string): string {
  return s.replace(/\/+$/, "");
}

/** Internal HTTP core shared by the namespaced sub-APIs. */
class HttpCore {
  readonly serverURL: string;
  readonly bearerToken: string;
  readonly team?: string;
  private readonly _fetch: typeof fetch;

  constructor(opts: SDKOptions = {}) {
    // Read env without depending on @types/node, so the SDK typechecks standalone
    // and runs in any runtime (Node, edge, Deno, Bun, browser).
    const g = (typeof globalThis !== "undefined" ? (globalThis as Record<string, any>) : {}) as Record<string, any>;
    const env = (k: string): string | undefined => g?.process?.env?.[k];
    this.serverURL = trimSlash(
      opts.serverURL || env("OPENEDGE_API_URL") || env("VERCEL_SDK_SERVER_URL") || "http://127.0.0.1:8786"
    );
    this.bearerToken = opts.bearerToken || env("OPENEDGE_API_KEY") || env("VERCEL_TOKEN") || "";
    this.team = opts.team || env("OPENEDGE_TEAM") || undefined;
    const f = opts.fetch || (typeof fetch !== "undefined" ? fetch : undefined);
    if (!f) throw new Error("No fetch implementation available — pass `fetch` in SDKOptions.");
    this._fetch = f;
  }

  async request<T>(path: string, init?: RequestInit): Promise<T> {
    const headers: Record<string, string> = {
      accept: "application/json",
      ...(init?.headers as Record<string, string> | undefined),
    };
    if (this.bearerToken) headers.authorization = `Bearer ${this.bearerToken}`;
    if (this.team) headers["x-hive-team"] = this.team;
    const res = await this._fetch(`${this.serverURL}${path}`, { ...init, headers });
    const text = await res.text();
    if (!res.ok) {
      throw new VercelError(`${init?.method || "GET"} ${path} → ${res.status}`, res.status, text);
    }
    return (text ? JSON.parse(text) : null) as T;
  }
}

/** Integrations namespace — the team's Composio-linked integrations as resources. */
export class IntegrationsAPI {
  constructor(private readonly core: HttpCore) {}

  /** List the team's connected integrations (redacted — no secret values). */
  async list(): Promise<Integration[]> {
    const rows = await this.core.request<any[]>("/v1/integrations");
    return (rows || []).map(mapIntegration);
  }

  /** Get a single integration by id (redacted). */
  async get(id: string): Promise<Integration> {
    return mapIntegration(await this.core.request<any>(`/v1/integrations/${encodeURIComponent(id)}`));
  }

  /** Resolve an integration's secret credentials + env values. Requires a
   *  platform key with access to the team. */
  async credentials(id: string): Promise<IntegrationCredentials> {
    const r = await this.core.request<any>(`/v1/integrations/${encodeURIComponent(id)}/credentials`);
    return {
      id: r.id,
      provider: r.provider,
      name: r.name,
      kind: r.kind,
      credentials: r.credentials || {},
      env: r.env || {},
    };
  }

  /** Convenience: the merged env vars across ALL connected integrations — the
   *  same set auto-injected into deployments. Handy to hydrate a runtime config. */
  async env(): Promise<Record<string, string>> {
    const list = await this.list();
    const out: Record<string, string> = {};
    for (const i of list) {
      if (!i.hasCredentials) continue;
      const c = await this.credentials(i.id);
      Object.assign(out, c.env);
    }
    return out;
  }

  /** Find a connected integration by provider slug (e.g. "stripe"), or null. */
  async byProvider(provider: string): Promise<Integration | null> {
    const list = await this.list();
    return list.find((i) => i.provider === provider) || null;
  }
}

function mapIntegration(r: any): Integration {
  return {
    id: r.id,
    team: r.team,
    provider: r.provider,
    name: r.name,
    kind: r.kind,
    envKeys: r.env_keys || r.envKeys || [],
    hasCredentials: !!(r.has_credentials ?? r.hasCredentials),
    createdMs: r.created_ms ?? r.createdMs ?? 0,
    updatedMs: r.updated_ms ?? r.updatedMs ?? 0,
  };
}

/**
 * The OpenEdge-flavored Vercel SDK client. Drop-in shape with `@vercel/sdk`:
 *
 * ```ts
 * import { Vercel } from "@openedge/vercel-sdk";
 * const vercel = new Vercel({ serverURL: "https://app.example.com", bearerToken: "hive_…" });
 * const integrations = await vercel.integrations.list();
 * const env = await vercel.integrations.env(); // { STRIPE_API_KEY: "…", … }
 * ```
 */
export class Vercel {
  readonly integrations: IntegrationsAPI;
  private readonly core: HttpCore;

  constructor(opts: SDKOptions = {}) {
    this.core = new HttpCore(opts);
    this.integrations = new IntegrationsAPI(this.core);
  }

  /** The resolved server URL this client talks to. */
  get serverURL(): string {
    return this.core.serverURL;
  }
}

export default Vercel;
