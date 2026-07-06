# @openedge/vercel-sdk

A **fork of the upstream [`@vercel/sdk`](https://github.com/vercel/sdk)**, modified to be
configurable/pointable at the **OpenEdge platform API** and to authenticate with an
**OpenEdge platform key (`hive_…`)** instead of a Vercel access token.

It keeps the upstream ergonomics — construct a `Vercel` client with `{ serverURL, bearerToken }`
and call namespaced sub-APIs — so code written against `@vercel/sdk` repoints with a one-line change:

```diff
- import { Vercel } from "@vercel/sdk";
+ import { Vercel } from "@openedge/vercel-sdk";

  const vercel = new Vercel({
-   bearerToken: process.env.VERCEL_TOKEN,
+   serverURL: process.env.OPENEDGE_API_URL,   // your OpenEdge deployment's API
+   bearerToken: process.env.OPENEDGE_API_KEY, // a hive_… platform key (Settings → API Keys)
  });
```

## What's different from upstream

| | upstream `@vercel/sdk` | this fork |
|---|---|---|
| `serverURL` | `https://api.vercel.com` | **your OpenEdge API** (env `OPENEDGE_API_URL`, default `http://127.0.0.1:8786`) |
| `bearerToken` | Vercel token | **`hive_…` platform key** (env `OPENEDGE_API_KEY`) — its team scopes every call |
| team scoping | `teamId`/`slug` query params | derived from the key; optional `team` → `x-hive-team` |
| `integrations` namespace | n/a | lists Composio-linked integrations + their credentials/env |

## Integrations

When you connect an integration on the **Integrations** tab (OAuth via Composio), the platform
registers it as a team-scoped resource and auto-injects its credentials as deployment env vars.
This SDK reads those back:

```ts
import { Vercel } from "@openedge/vercel-sdk";

const vercel = new Vercel({
  serverURL: "https://app.example.com",   // your OpenEdge API origin
  bearerToken: process.env.OPENEDGE_API_KEY, // hive_…
});

// All connected integrations (redacted).
const integrations = await vercel.integrations.list();
// → [{ provider: "stripe", name: "Stripe", envKeys: ["STRIPE_API_KEY"], … }]

// One integration's secret credentials + env values.
const stripe = await vercel.integrations.byProvider("stripe");
if (stripe) {
  const { credentials, env } = await vercel.integrations.credentials(stripe.id);
  // credentials.api_key, env.STRIPE_API_KEY, …
}

// Convenience: every integration's env var, merged (same set injected into deployments).
const env = await vercel.integrations.env();
```

In a deployment you usually don't need the SDK at all — the env vars are already injected
(`STRIPE_API_KEY`, etc.). The SDK is for fetching them on demand, in other environments, or
when you want the live token rather than the snapshot.

## API

- `new Vercel({ serverURL?, bearerToken?, team?, fetch? })`
- `vercel.integrations.list(): Promise<Integration[]>`
- `vercel.integrations.get(id): Promise<Integration>`
- `vercel.integrations.byProvider(slug): Promise<Integration | null>`
- `vercel.integrations.credentials(id): Promise<IntegrationCredentials>`
- `vercel.integrations.env(): Promise<Record<string,string>>`

Zero runtime dependencies (uses global `fetch`). Node 18+, edge runtimes, Deno, Bun, browser.
