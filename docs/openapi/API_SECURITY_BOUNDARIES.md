# API security boundaries

| Surface | Exposure | Authentication actually implemented |
| --- | --- | --- |
| Marketplace application | Public HTTPS | Marketplace/user application authentication is outside this router |
| DevHub Marketplace API | Private service-to-service | Five-header HMAC-SHA256, timestamp and durable nonce replay protection |
| Hive Admin `/healthz` | Loopback/private only | **Unauthenticated minimal liveness only** |
| Hive Admin reads and mutations | Loopback/private only | JWT/API key required when `HIVE_JWT_SECRET` is configured; handlers add tenant/operator gates |
| Hive Admin platform-operator handlers | Loopback/private only | Handler checks `platform_admin`, derived from the configured owner/admin identity |
| Hive node mesh/relay transport | Mesh/protocol network | Existing Iroh endpoint identity, peer-trust, and relay controls; not HTTP Admin auth |

## Required separation

Marketplace must never call Hive Admin directly. It uses only the documented
private DevHub HMAC API. DevHub retrieves Marketplace placement policy
server-side with a Clerk JWT minted from the `autheo-marketplace-v1` template,
then validates and snapshots it before invoking Hive. `hive_jwt`,
`HIVE_INTERNAL_TOKEN`, and custom DevHub/Hive M2M headers are never sent to
Marketplace. Publishing documentation does not permit publishing the HTTP
server.

`hive-cloud` defaults Admin to `127.0.0.1:8786`. It rejects public, wildcard,
and link-local Admin binds at startup. An RFC1918 or IPv6 ULA management bind
requires both `HIVE_ADMIN_PRIVATE_NETWORK=1` and `HIVE_JWT_SECRET`.

The public listener explicitly rejects `api.`, `admin.`, `webhook.`, and
`api-<region>.` hostnames. It never dispatches Hive Admin, Swagger,
Marketplace, or webhook routes. No firewall change or public reverse-proxy
configuration can make that an endorsed topology.

`HIVE_AUTH_BYPASS=1` is effective only outside production in the dashboard.
Development minting also requires a loopback `HIVE_ADMIN`; setting
`NEXT_PUBLIC_HIVE_DEV_MINT=1` alone never creates an authentication bypass.
