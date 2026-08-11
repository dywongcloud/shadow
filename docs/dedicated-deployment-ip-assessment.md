# Dedicated public IPv4/IPv6 per deployment — feasibility assessment

Requested alongside the Minecraft raw-TCP fix: a way to give a deployment's
hostname a dedicated public IP address, rather than a shared fleet IP
differentiated only by port. This document is the honest scoping this needs
before any code lands — no half-built stub ships as part of this change.

## Current state (verified against source, not assumed)

Every raw-protocol deployment already gets a **dedicated public port**
(`crates/hive-cloud/src/raw_ports.rs`, range `HIVE_RAW_PORT_RANGE`, default
20000-29999) — that's the mechanism this session's Minecraft fix relies on.
It has never been a dedicated **IP**. The raw ingress listener binds:

```rust
// raw_proxy.rs:122
TcpListener::bind(("0.0.0.0", p)).await
// raw_ports.rs:248-249 (the allocator's own bind-probe)
std::net::TcpListener::bind(("0.0.0.0", port)).is_ok()
    && std::net::UdpSocket::bind(("0.0.0.0", port)).is_ok()
```

`0.0.0.0` — every allocation binds on **every IPv4 address the node
already has**, and there is no IPv6 bind anywhere in `raw_proxy.rs` or
`raw_ports.rs` at all. So today: IPv4-only, one shared address per node,
port is the entire routing key.

## What "dedicated IP" actually requires

**IPv4.** Each fleet node has exactly one public IPv4 address from its
cloud provider. Giving one deployment its own address means provisioning
and attaching an *additional* address per deployment — a real
infrastructure purchase (a floating/elastic IP from whichever provider
hosts that node) plus an ansible role to attach and configure it. No
change inside this repository can conjure an address the host doesn't
have. This is an infrastructure/billing decision, not a code change.

**IPv6.** Structurally cheaper *if* the underlying host has a routed
prefix: a single `/64` is enough to hand out a distinct address per
deployment with zero per-address cost, no purchase, no provider API call
per deployment — just software choosing which address to bind. This is
the only path that's genuinely a code problem rather than a procurement
one.

## What's verified — resolved 2026-08-05, with live evidence

The previous version of this document left "does any fleet node have a
routed IPv6 prefix" **unverified**, for lack of SSH/API access. That gap is
now closed, at every layer the doc's own next-step section named, with
real (not assumed) evidence:

- **OS layer (fc-bangkok, live SSH):** `ip -6 addr show scope global` →
  empty, no global IPv6 address configured on any interface. `ip -6 route
  show` → only auto-configured link-local (`fe80::/64`) routes; no global
  route present.
- **Instance layer (Tencent Cloud API, `cvm.tencentcloudapi.com`
  `DescribeInstances`, signed TC3-HMAC-SHA256, no SDK):** fc-bangkok's
  instance (`ins-ldme28ac`) reports `IPv6Addresses: []`. Swept with no
  filter across every Tencent region the fleet occupies —
  `na-siliconvalley` (16 instances / 5 VPCs, covers sj/sj2/gpusj1-3/
  cvmsj1-2), `na-ashburn` (7 instances / 2 VPCs, covers va/va2/va3),
  `eu-frankfurt` (1 instance, fr), `ap-hongkong` (8 instances, hk) —
  **every instance in every region reports `IPv6Addresses: []`**, matched
  against `ansible/inventory/hosts.ini` by public IP.
- **Network layer (`vpc.tencentcloudapi.com` `DescribeVpcs`/
  `DescribeSubnets`):** fc-bangkok's VPC (`vpc-a1yzanws`) and subnet
  (`subnet-hhyd2oux`) both report `Ipv6CidrBlock: ""` and
  `Ipv6CidrBlockSet: []` — confirming no IPv6 CIDR has ever been
  allocated at the network level, not merely unconfigured on an
  interface. This is the exact "routed vs. configured" distinction the
  original recommendation called out.

**Conclusion: no fleet node, on any provider region this platform runs in,
has any IPv6 allocation at all — not a routed prefix, not even a single
address.** This is a definitive "none exists," not an absence of
evidence.

## Recommended next step — updated

Per this document's own decision tree, step 3 now applies as a confirmed
finding rather than a fallback:

1. ~~Check for a routed IPv6 prefix~~ — **done, see above: none exists,
   anywhere in the fleet.**
2. ~~If one exists, extend `raw_ports`/`raw_proxy` with an IPv6 bind~~ —
   **does not apply**. Writing that code now, with no live prefix
   anywhere to bind against, would be unverifiable — no real execution
   could ever exercise it in this environment, which is exactly the
   "half-built stub" this document opened by refusing to ship.
3. **This is an infrastructure request, not a coding task.** Making
   dedicated IPv6 real requires actually provisioning a routed `/64` (or
   larger) on the VPC(s) above via Tencent Cloud's console/API
   (`AssignIpv6CidrBlock` on each VPC, then a subnet-level IPv6 CIDR) —
   a real, billable, production-network change to live VPCs carrying
   tenant traffic, not a reversible local edit. That decision belongs to
   the operator, not to an unattended code change.

No code changes were made under this PRD row. Implementing a fake
"dedicated IP" that's actually still the shared node address under a
different label would be worse than not shipping anything — it would
look done while providing nothing a user asked for. IPv4 dedicated
addressing remains what it always was: a per-address purchase/attach
decision with the same provider, orthogonal to this investigation.
