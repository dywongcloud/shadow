# Remaining implementation gaps and security findings

Resolved findings are intentionally omitted. This is an evidence report, not a
request to weaken any boundary.

1. `GET /v1/admin/marketplace` is operator-only, but it is a Hive Admin route,
   not an alternate Marketplace service endpoint.
2. Many Admin handlers return handler-specific JSON rather than a stable
   shared response envelope; the internal spec intentionally leaves those
   schemas broad where code does not declare a stable DTO. Expanding it safely
   requires route-by-route contract work, not guessed schemas.

## Intentionally deferred Marketplace work

Marketplace allocation status/read APIs, callbacks, and DevHub/Hive usage
ingestion are not requirements of the approved integration. `POST
/usage-records` is buyer-authenticated and is not a DevHub/Hive ingestion
surface. If a future integration needs allocation callbacks, usage ingestion,
or node-verifier completion, it requires a separately reviewed Marketplace
contract covering authentication, tenant binding, idempotency and replay
handling, payload sanitization, error semantics, and operational ownership.

## Resolved boundaries

- With `HIVE_JWT_SECRET`, every Admin read except minimal `/healthz` requires a
  verified JWT or tenant API key. Route handlers retain tenant gates and
  platform-wide operations still require the independently-derived
  `platform_admin` claim; tenant `role: owner` is not platform authority.
- The Admin listener accepts loopback by default. A private RFC1918/IPv6-ULA
  management bind requires both `HIVE_ADMIN_PRIVATE_NETWORK=1` and JWT
  enforcement. Public, wildcard, and link-local binds fail startup.
- Public host dispatch now rejects `api.`, `admin.`, `webhook.`, and
  `api-<region>.` rather than forwarding them to Admin. There is no supported
  public Admin, Swagger, Marketplace, or webhook publication topology.
- Marketplace routes are protected as one HMAC-gated router. The middleware
  verifies the exact raw body, five current headers, canonical signature,
  constant-time MAC, timestamp skew, and durable nonce before invoking a
  handler. A nonce is consumed only after the other checks pass.
- `HIVE_AUTH_BYPASS=1` is inert in a production dashboard. Development minting
  also requires non-production mode, the bypass flag, and a loopback
  `HIVE_ADMIN`; `NEXT_PUBLIC_HIVE_DEV_MINT=1` alone cannot mint a token.
