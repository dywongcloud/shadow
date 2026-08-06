# Ansible: end-to-end fleet deployment

Deploys the whole hive-cloud platform to one or more nodes: OS prerequisites,
Firecracker (real hardware KVM or PVM-without-KVM), the `hive-cloud` binary +
systemd service, mesh trust configuration, and DNS/TLS ingress via Vercel.
This codifies the manual procedure this project's own sessions used to bring
up every real fleet node (bare-metal Bangkok, Tencent CVMs in Virginia/San
Jose, and hot-joining a new machine) into reusable, idempotent playbooks.

## Prerequisites (on the machine running `ansible-playbook`, not the targets)

```bash
python3 -m pip install --user ansible
cd ansible
ansible-galaxy collection install -r requirements.yml
```

Targets: RHEL-family Linux (Rocky/Alma/TencentOS/RHEL 10 -- matches the
fleet this was built against), root SSH access, x86_64.

## Setup

```bash
cd ansible
cp inventory/hosts.ini.example inventory/hosts.ini
# edit hosts.ini: real IPs, hive_name/hive_region per host, group membership
# (fc_kvm = has real /dev/kvm, fc_pvm = doesn't -- see roles/pvm_firecracker
# for how to tell which one a given cloud VM is)

mkdir -p inventory/group_vars/all
cp inventory/group_vars/vault.yml.example inventory/group_vars/all/vault.yml
# edit vault.yml: real HIVE_JWT_SECRET / HIVE_INTERNAL_TOKEN / VERCEL_API_TOKEN
ansible-vault encrypt inventory/group_vars/all/vault.yml

echo 'your-vault-password' > .vault_pass   # gitignored; ansible.cfg points at this
chmod 600 .vault_pass
```

Every HIVE_* value with a sane default lives in
`inventory/group_vars/all/vars.yml` -- override per-group (`group_vars/fc_kvm.yml` / `group_vars/fc_pvm.yml`) or
per-host (`hosts.ini` inline vars) as needed. This file documents the vars
this fleet actually runs with; it is not exhaustive -- grep
`crates/hive-cloud/src/*.rs` for `std::env::var("HIVE_` for the full set the
codebase recognizes if you need something not templated here.

## Deploy the whole fleet from scratch

```bash
ansible-playbook playbooks/site.yml
```

~20-40 minutes per `fc_pvm` host on first run (two kernel compiles +
Firecracker), much faster for `fc_kvm` hosts (no kernel work) and on re-runs
(every expensive step is idempotent, guarded by `creates:`).

Run only part of it:

```bash
ansible-playbook playbooks/site.yml --tags prerequisites
ansible-playbook playbooks/site.yml --tags hive_platform      # code only, see also platform-only.yml
ansible-playbook playbooks/site.yml --limit fc_pvm --tags pvm_firecracker
```

## Redeploy just the platform code (fast path)

```bash
ansible-playbook playbooks/platform-only.yml
```

One host at a time (`serial: 1`) so mesh quorum is never lost across more
than one node at once -- the same discipline this project's sessions used
manually every time a code fix needed rolling out fleet-wide.

## Parallel fleet deploy (replaces the bash roll scripts)

```bash
ansible-playbook playbooks/parallel-deploy.yml
```

Redeploys BOTH the `hive-cloud` backend and the `ui/` dashboard across every
`[platform]` host, replacing `scripts/roll-backend-fleet.sh` and
`scripts/deploy-ui-fleet.sh`. Runs in three phases: build once per glibc
group (backend) / once on the control-plane leader (UI) -- these builds run
in parallel with each other (`strategy: free`); push the resulting artifact
to every host in parallel (`strategy: free`, bounded by `ansible.cfg`'s
`forks = 20` so this is a real fan-out and not the default 5-at-a-time
batching -- no service touched yet); then restart `hive-node`/`hive-ui` a
bounded number of hosts at a time (`serial`) so the control-plane leader
chain and the public round-robin dashboard both stay up throughout the roll.

Backend and UI restart batches have **separate** defaults, reasoned
differently -- collapsing them into one shared knob was the bug this
playbook used to have:

- `deploy_serial_backend` (default **1**): `hive_cp_owner_chain` has only 3
  candidate control-plane-leader nodes fleet-wide; there's no live
  measurement that a bigger batch can't land 2 of those 3 down together, so
  this stays conservative -- matching `platform-only.yml`'s own precedent --
  until someone measures otherwise.
- `deploy_serial_ui` (default **3**): the dashboard has no leader-election
  concern, just round-robin DNS across every node, so a batch of 2-3 out of
  14 is safe (11+ nodes keep serving) and is explicitly NOT left at 1 --
  `serial: 1` here would run ~14 sequential restart-and-health-check rounds
  and defeat the point of parallelizing the roll.
- `deploy_serial` (legacy): if set and the two specific vars above are not,
  applies to both plays.
- `hive_push_throttle` (default **4**): caps concurrency of just the heavy
  ~100MB binary / `.next` bundle `copy` tasks, independent of `forks`. Every
  push here originates from the control host (this operator machine)
  straight to one target -- never one fleet node relaying to several peers
  -- so it structurally can't reproduce the sshd `MaxStartups` pileup
  `roll-backend-fleet.sh`'s header names (many SSH `-A` hops converging on
  one *source* node). What it DOES still bound is the operator machine's own
  uplink bandwidth pushing to a dozen-plus hosts at once -- the same
  "residential uplink" problem that script's header cites as its reason for
  node-to-node distribution in the first place. Raise it from a
  well-provisioned box; the default is a conservative guess, not a measured
  number.

```bash
ansible-playbook playbooks/parallel-deploy.yml
ansible-playbook playbooks/parallel-deploy.yml -e deploy_serial_ui=2        # faster UI restart batches, still bounded
ansible-playbook playbooks/parallel-deploy.yml -e deploy_serial_backend=2   # only with real evidence multi-node restart is quorum-safe
ansible-playbook playbooks/parallel-deploy.yml -e hive_push_throttle=8      # more concurrent big-artifact pushes (needs real uplink headroom)
ansible-playbook playbooks/parallel-deploy.yml --tags backend       # hive-cloud only
ansible-playbook playbooks/parallel-deploy.yml --tags ui             # ui/ dashboard only
ansible-playbook playbooks/parallel-deploy.yml --limit fc_pvm        # subset of hosts
```

Requires `inventory/hosts.ini`'s `[glibc239]`/`[glibc238]` groups to stay
current with the fleet's real glibc membership (AGENTS.md "Fleet has two
glibc groups" -- membership is by OS image, not region; verify with
`scripts/audit-runtime-versions.sh` before trusting the group split after
adding or re-imaging a node).

`ansible.cfg`'s `[ssh_connection] ssh_args` already carries
`ControlMaster=auto`/`ControlPersist=120m` fleet-wide (the same fix
`scripts/deploy-ui-fleet.sh`'s header documents for the `MaxStartups`
failure class) -- every task in this playbook reuses one multiplexed
connection per host rather than opening a fresh one per task/module call.

## Add one new node (day 2)

```bash
# 1. add it to inventory/hosts.ini under [fc_kvm] or [fc_pvm]
# 2. run:
ansible-playbook playbooks/node-join.yml -e new_node=<inventory_hostname>
```

Provisions the new node fully, then -- only if `hive_peer_trust_enabled` is
set -- pushes its iroh id into every OTHER node's `HIVE_TRUSTED_NODE_IDS`.
This closes the exact gap a real session hit: a new node's id missing from
even one other node's allowlist leaves that node silently rejecting the
newcomer's gossip, while the newcomer's OWN view of the mesh still looks
healthy (a one-directional gap that is easy to miss without checking a
THIRD node's view of the newcomer, not just the newcomer's own report).

## Headless browser nodes (five hosts, one each)

```bash
ansible-playbook playbooks/browser-nodes.yml
```

Runs the `/run-node` browser node unattended on **fc-bangkok, fc-sanjose,
fc-virginia, fc-saopaulo, fc-frankfurt** — headless Chromium loading a loopback
page that owns the shipped `ui/public/run-node-worker.js` (verbatim), which
boots `crates/hive-browser`'s wasm `BrowserNode` over iroh QUIC via a WSS relay.

Two things make it a fleet service rather than a browser tab someone left open:

* **Auth with no human.** `browser_admission.rs::fresh_user_claims` rejects API
  keys (`sub` = `key:…`), service roles, and anything whose `iat` is older than
  300s — so the *only* admissible credential is a freshly minted platform JWT.
  A node-local broker mints one on loopback with `x-hive-internal` (delivered by
  systemd `LoadCredential=` from a root-only file, never a unit `Environment=`
  line) and hands the browser nothing but a short-lived, single-tenant,
  `role: member`, non-admin cookie. No Clerk sign-in, no stored user credential.
* **Caps that cannot starve `hive-node`.** `hive-node` runs uncapped, so the
  browser unit carries fact-derived `MemoryMax`/`CPUQuota`/`TasksMax` plus
  `CPUWeight=20` against the platform's default 100 and `OOMScoreAdjust=700`
  against its 0 — under real pressure the browser dies and the platform lives.

Full reasoning for both, plus the exact numbers and how to verify a node is
live in fleet presence: `roles/hive_browser_node/README.md`.

## Secrets

Never commit real secrets. `inventory/hosts.ini`, `inventory/group_vars/
all/vault.yml`, and `.vault_pass` are all gitignored -- only the `.example`
templates ship in git. `vault.yml` lives inside `group_vars/all/` (not as a
flat `group_vars/all.yml`-style file) because ansible's group_vars loader
does NOT merge a flat `group_vars/<group>.yml` with a same-named
`group_vars/<group>/` directory -- whichever form is present for a given
group name, the OTHER form for that same name is silently ignored entirely.
`inventory/group_vars/all/vars.yml` (plaintext defaults) and
`inventory/group_vars/all/vault.yml` (encrypted secrets) both live inside
the `all/` directory specifically so both actually load. Secrets referenced in role templates
(`vault_hive_jwt_secret`, `vault_hive_internal_token`,
`vault_vercel_api_token`, `vault_vercel_team_id`, `vault_stripe_secret_key`,
`vault_composio_api_key`, `vault_github_app_client_id`,
`vault_github_app_client_secret`, `vault_github_app_redirect_uri`,
`vault_hive_github_auth_config_id`, `vault_github_webhook_secret`,
`vault_github_token`, `vault_hive_secret_key`)
resolve from the encrypted `vault.yml`.

## Roles

| Role | Does |
|---|---|
| `prerequisites` | base packages, Rust toolchain, podman, firewall, Node.js |
| `firecracker_kvm` | Firecracker on hosts with real hardware `/dev/kvm` |
| `pvm_firecracker` | Firecracker via PVM on hosts without hardware KVM (imported, independently-verified role -- see its own README for the fsgsbase requirement and the critical per-VM kernel non-portability gotcha) |
| `hive_platform` | builds + installs the `hive-cloud` binary and its systemd unit |
| `hive_ui` | builds + installs the `ui/` dashboard (Next.js) and its systemd unit -- used by `site.yml`'s from-scratch path |
| `hive_platform_fanout` | build/push/restart task files backing `parallel-deploy.yml`'s backend phases (one build per glibc group, parallel push, bounded-serial restart) |
| `hive_ui_fanout` | build/push/restart task files backing `parallel-deploy.yml`'s UI phases (one canonical build, parallel push, bounded-serial restart) |
| `mesh_bootstrap` | mesh trust config (join-proof self-admit by default, or the opt-in static `HIVE_TRUSTED_NODE_IDS` allowlist) |
| `dns_vercel` | DNS/TLS ingress systemd drop-in (Vercel DNS + ACME DNS-01), matching `RUNBOOK.md` |
| `hive_browser_node` | one capped headless browser node per host on the five `[browser_nodes]` hosts: headless Chromium + a loopback session broker that mints the short-lived tenant JWT the admission requires (see its own README for the auth decision and every cap value) |

## Verification

```bash
# syntax + basic sanity, no target needed -- purely local, opens no
# connections, safe to run any time
ansible-playbook playbooks/site.yml --syntax-check
ansible-playbook playbooks/node-join.yml --syntax-check
ansible-playbook playbooks/platform-only.yml --syntax-check
ansible-playbook playbooks/parallel-deploy.yml --syntax-check
ansible-playbook playbooks/browser-nodes.yml --syntax-check
ansible-playbook playbooks/parallel-deploy.yml --list-tasks   # confirm the 8-play structure/tags without connecting anywhere

# dry run against a real host -- proves inventory/connectivity/templating
# actually work end-to-end without mutating anything. NOTE for
# parallel-deploy.yml specifically: `command`/`shell` tasks (the actual
# cargo/npm builds) do not execute under --check -- they're skipped, so a
# green --check run proves connectivity/templating, not that a real build
# would succeed. Prefer --limit against ONE known-safe host before a real
# fleet-wide run, and never run this against production hosts without
# explicit sign-off given what it restarts.
ansible-playbook playbooks/site.yml --check --diff --limit <a-real-host>
ansible-playbook playbooks/parallel-deploy.yml --check --diff --limit <a-real-host> --tags backend
```

## What this does NOT cover

- The local macOS dev nodes (launchd, not systemd) -- this suite targets
  Linux cloud/bare-metal nodes. The local Mac fleet remains hand-managed via
  `scripts/shadwd.sh` and the `dev.shadw.*` launchd agents.
- Custom per-tenant domains / per-deployment certs (RUNBOOK.md notes these
  as follow-ups to the base DNS migration this role automates).

The dashboard (`ui/`, Next.js) build/deploy WAS a separate concern (the
`scripts/deploy-ui-fleet.sh` bash script) but is now also covered by
`playbooks/parallel-deploy.yml --tags ui` / the `hive_ui`+`hive_ui_fanout`
roles above -- kept as a genuinely separate set of plays/tags from the
backend (`--tags backend`) since the two are independently-running services
per AGENTS.md's Process section, not because it's unhandled here.
