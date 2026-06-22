Use this prompt for your coding agent:

```md
You are working on our Shadow Cloud / self-hostable Vercel-style deployment platform.

We need to ensure our public ingress architecture supports an ngrok wildcard endpoint pooled across multiple regional nodes, while still routing each deployment/subdomain to the correct owning node/region.

Context:

We may run the same wildcard ngrok endpoint on multiple hosts:

- Node A: Virginia
- Node B: San Francisco
- Node C: Los Angeles

Example wildcard:

*.deployment.shadow.ngrok.pizza

ngrok endpoint pooling may send any matching request to any pooled node. For example:

foobar.deployment.shadow.ngrok.pizza

may hit Virginia, San Francisco, or Los Angeles first. ngrok does not know that `foobar` belongs to Los Angeles. Therefore, every ingress node must run a smart router that inspects the Host header, resolves the deployment/subdomain owner from our control plane, and forwards/proxies the request to the correct workload host.

Goal:

Audit the existing codebase and confirm whether we already support this behavior. If not, implement it cleanly.

Required behavior:

1. Wildcard ingress router

Every regional node running the wildcard ngrok tunnel must expose an HTTP router/proxy that handles all requests for:

*.deployment.shadow.ngrok.pizza

The router must read the original Host header, for example:

foobar.deployment.shadow.ngrok.pizza

and extract:

subdomain = foobar

2. Deployment lookup

The router must resolve the subdomain against our control plane / deployment registry.

The lookup should determine at least:

- deployment ID
- tenant/user ID if applicable
- owning node ID
- owning region
- target internal URL or peer address
- target port/protocol
- whether deployment is public/private/preview
- deployment status: active, sleeping, building, deleted, errored, etc.

Use the existing data model if available. Do not invent a parallel registry if one already exists.

3. Local vs remote routing

If the resolved deployment is hosted locally on the current node:

- proxy the request directly to the local workload/runtime/microVM/container/function server.

If the resolved deployment is hosted on another node:

- forward/proxy the request to the owning node.
- preserve the original Host header.
- preserve method, path, query string, body, cookies, auth headers, and relevant request metadata.
- support streaming responses.
- support WebSocket upgrades if the platform supports them.
- do not redirect the browser to the internal node URL unless that is already the intended design. Prefer transparent proxying.

Example:

Request enters Virginia:

Host: foobar.deployment.shadow.ngrok.pizza

Control plane says:

foobar -> node-c-los-angeles

Virginia should proxy the request to Los Angeles while preserving:

Host: foobar.deployment.shadow.ngrok.pizza

4. Required headers

When forwarding between Shadow Cloud nodes, add internal routing metadata headers such as:

X-Shadow-Original-Host: foobar.deployment.shadow.ngrok.pizza
X-Shadow-Original-Proto: https
X-Shadow-Forwarded-By: <current-node-id>
X-Shadow-Target-Node: <target-node-id>
X-Shadow-Request-ID: <request-id>
X-Forwarded-For: <client-ip-chain>
X-Forwarded-Host: foobar.deployment.shadow.ngrok.pizza
X-Forwarded-Proto: https

Do not expose sensitive internal headers to user workloads unless explicitly intended.

5. Loop prevention

Implement loop protection.

If node A receives a request and forwards it to node C, node C must not forward it back to node A forever.

Use one or more of:

- X-Shadow-Hop-Count
- X-Shadow-Forwarded-By chain
- max hop count, default 3
- direct target verification

If the hop count is exceeded, return a clear 508 Loop Detected or 502 Bad Gateway response.

6. Not found behavior

If the Host/subdomain does not map to a deployment:

Return a clean platform 404 page or JSON response.

Do not leak internal routing data.

7. Deployment unavailable behavior

If the deployment exists but the owning node is unavailable:

Return a clean 502/503 response.

If the platform has failover or replicas, attempt failover using the deployment registry.

If failover is not implemented yet, add TODOs and clean interfaces for future replica-aware routing.

8. Specific endpoint override compatibility

The architecture should still support a future/optional mode where a specific ngrok endpoint exists for:

foobar.deployment.shadow.ngrok.pizza

on the owning node.

But the core design must not require one ngrok tunnel per deployment. The default architecture should be:

*.deployment.shadow.ngrok.pizza -> any regional ingress node -> smart Shadow router -> correct workload owner

9. Security / tenant isolation

Ensure the router does not allow arbitrary Host header abuse.

Validate that the Host matches one of our allowed deployment domains, such as:

*.deployment.shadow.ngrok.pizza

Reject unknown root domains.

Ensure tenant/deployment access rules are enforced before proxying:

- public deployment: allow
- private/preview deployment: validate access token, signed preview URL, ZK proof gate, cookie, or existing access control mechanism
- deleted/suspended deployment: deny

Do not let one tenant access another tenant’s private deployment by guessing subdomains.

10. Observability

Add structured logs for routing decisions:

- request ID
- original host
- subdomain
- selected deployment ID
- current node
- target node
- local vs remote route
- latency
- response status
- error reason if failed

Add metrics if the project already has a metrics system:

- ingress_requests_total
- ingress_proxy_errors_total
- ingress_remote_forward_total
- ingress_local_forward_total
- ingress_unknown_host_total
- ingress_route_latency_ms

11. Config

Add or verify config/env vars for:

SHADOW_NODE_ID
SHADOW_REGION
SHADOW_PUBLIC_INGRESS_DOMAIN=deployment.shadow.ngrok.pizza
SHADOW_ALLOWED_HOST_SUFFIXES=deployment.shadow.ngrok.pizza
SHADOW_INTERNAL_NODE_SECRET or equivalent
SHADOW_CONTROL_PLANE_URL
SHADOW_INTERNAL_PROXY_TIMEOUT_MS
SHADOW_MAX_PROXY_HOPS=3

Reuse existing env/config patterns.

12. Tests

Add tests that prove the behavior.

Minimum required tests:

A. Host parsing

Input:

foobar.deployment.shadow.ngrok.pizza

Expected:

subdomain = foobar

B. Reject invalid host

Input:

foobar.evil.com

Expected:

403 or 404, no proxy

C. Local route

Registry says:

foobar -> current node

Expected:

request proxies to local workload

D. Remote route

Registry says:

foobar -> los-angeles node

Current node is virginia

Expected:

request proxies to los-angeles internal endpoint while preserving original Host

E. Unknown deployment

Registry has no mapping for `missing`

Expected:

404

F. Loop protection

Request has X-Shadow-Hop-Count above limit

Expected:

508 or 502

G. Preserve path/query/body

Request:

POST https://foobar.deployment.shadow.ngrok.pizza/api/test?x=1

Expected target receives:

path = /api/test?x=1
method = POST
body preserved
Host preserved

H. Streaming support

Verify streamed/chunked responses are not buffered incorrectly.

I. WebSocket support, if applicable

Verify upgrade requests proxy correctly.

13. Deliverables

After implementation, provide:

- Summary of whether this already existed
- Files changed
- New routing flow
- Any new env vars
- How to run locally
- How to run with ngrok wildcard pooling on multiple hosts
- Test results

Desired final architecture:

Client request:

https://foobar.deployment.shadow.ngrok.pizza

ngrok pooled wildcard may send it to any node:

Virginia / San Francisco / Los Angeles

The receiving node runs Shadow ingress router:

1. parse Host
2. resolve subdomain in control plane
3. check access policy
4. determine owner node
5. if local, proxy locally
6. if remote, proxy to owner node
7. preserve original Host and request semantics
8. return response transparently to client

Important:

Do not create a unique ngrok tunnel per deployment as the default solution.

Do not assume ngrok will route `foobar` to the Los Angeles node just because the workload is there.

The platform itself must own deployment-aware routing.
```

