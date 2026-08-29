# Marketplace project resources integration

Autheo.dev links a project to Marketplace browse pages using:

```text
{NEXT_PUBLIC_MARKETPLACE_URL}/listings?project={project_id}&resource_type={compute|storage}&return_url={encoded_autheo_project_url}
```

`NEXT_PUBLIC_MARKETPLACE_URL` is public configuration only. It defaults to
`http://localhost:3000` in development and `https://marketplace.autheo.dev`
in production. Marketplace owns its own Clerk sign-in flow; Autheo.dev neither
forwards its `hive_jwt` nor exposes Clerk secrets.

## Minimal Marketplace API contract

Marketplace must authorize every endpoint below with a verified
`autheo-marketplace-v1` Clerk session token. The token is obtained by an
Autheo.dev server route from its existing Clerk session. Marketplace must derive
the buyer tenant from that verified token; it must not trust a browser-provided
tenant or project owner.

### List project resources

```text
GET /v1/marketplace/projects/{project_id}/resources
Authorization: Bearer <verified Clerk JWT>
```

Successful response:

```json
{
  "resources": [{
    "order_id": "order_123",
    "listing_name": "GPU compute",
    "provider_name": "Example provider",
    "resource_type": "compute",
    "status": "active",
    "capacity": { "vcpu": 8, "memory_gb": 32 }
  }]
}
```

### Attach a completed order

Marketplace must append `marketplace_order=<order_id>` when redirecting the
buyer to `return_url`. Autheo.dev then calls:

```text
POST /v1/marketplace/orders/{order_id}/project-attachments
Authorization: Bearer <verified Clerk JWT>
Content-Type: application/json

{"project_id":"my-project"}
```

The endpoint must atomically verify that the authenticated buyer owns the
completed order and that the order is attachable, persist the project attachment,
and return the resource object shown above. It must be idempotent for the same
order/project pair. A different tenant or a conflicting project attachment must
be rejected; Autheo.dev renders the failure and never claims success locally.

Until Marketplace implements this contract, the project page shows an
unavailable/error state rather than fabricating attached resources.
