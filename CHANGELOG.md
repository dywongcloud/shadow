# Changelog

## 88fe215 — Fix multi-tenancy data-visibility/usage-consistency bugs; add relational layer on guardian-db 0.18; add per-node relay + guardian-db anti-entropy

Team Simpfi's `drugs-wtf` project and fleet-wide billing/metrics were reported
missing/inconsistent — `ProjectStore`/`BillingStore` were node-local-only with
no gossip replication. Adds a relational mirror on GuardianDB's native SQL
layer (upgraded 0.17.2 → 0.18.0) plus a fleet-aware fallback in
`gitops_projects`, closing the visibility gap. Separately root-caused the
actual cross-node read failures to a missing `fetch_from_host`
discovery-fallback and missing `gossip::dispatch` routes for the billing
endpoints — fixed both; billing now converges byte-identical fleet-wide.

Implements the requested per-node relay + guardian-db anti-entropy
architecture: every node embeds and gossips its own relay, a relay-selection
algorithm replaces stale connect hints, and a 60s anti-entropy loop detects
and reconciles guardian-db divergence between random peers — live-verified
converging a real fleet divergence.

Found and fixed a live plaintext-secret leak (a real GitHub PAT served
unmasked via project settings) with a credential-shape auto-detector, and
cleaned up 3 abandoned duplicate Simpfi projects.
