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

## What's unverified — and why this session doesn't claim more

Whether any current fleet node actually has a routed IPv6 prefix beyond
its own single address is **not established**. Nothing in
`ansible/inventory/hosts.ini.example` or the ansible roles provisions or
records one; the codebase's only IPv6 awareness (`dns_geo.rs`,
`dns_probe.rs`, `dnsserver.rs`) is about serving AAAA *records* for
existing addresses, not about a per-deployment address pool. Confirming
this needs a real `ip -6 addr` (or equivalent) on an actual fleet node —
SSH access this session doesn't have. **Do not treat this document's
absence of a "yes, node X has a /64" finding as evidence there is one, or
that there isn't** — it's genuinely unconfirmed.

## Recommended next step (not taken here)

1. On a real fleet node: check for a routed IPv6 prefix (`ip -6 addr
   show`, and check the cloud provider's console/API for what's actually
   routed, not just what's configured — a provider can route a prefix
   without an interface being configured for all of it yet).
2. If one exists: extend `raw_ports::RawPortAllocation` with an optional
   bound IPv6 address, add a matching bind alongside each `0.0.0.0` call
   in `raw_proxy.rs`/`raw_ports.rs`'s probe, and surface the assigned
   address next to the existing `public_port` in the deployment record and
   the dashboard's Network settings / raw-port-connections display.
3. If none exists: this becomes an infrastructure request (ask the
   hosting provider for routed IPv6 per node), not a follow-up coding
   task — track it as that, not as "someone forgot to implement it."

No code changes were made under this PRD row. Implementing a fake
"dedicated IP" that's actually still the shared node address under a
different label would be worse than not shipping anything — it would
look done while providing nothing a user asked for.
