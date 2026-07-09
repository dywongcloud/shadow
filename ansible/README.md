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

cp inventory/group_vars/vault.yml.example inventory/group_vars/vault.yml
# edit vault.yml: real HIVE_JWT_SECRET / HIVE_INTERNAL_TOKEN / VERCEL_API_TOKEN
ansible-vault encrypt inventory/group_vars/vault.yml

echo 'your-vault-password' > .vault_pass   # gitignored; ansible.cfg points at this
chmod 600 .vault_pass
```

Every HIVE_* value with a sane default lives in `inventory/group_vars/all.yml`
-- override per-group (`group_vars/fc_kvm.yml` / `group_vars/fc_pvm.yml`) or
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

## Secrets

Never commit real secrets. `inventory/hosts.ini`, `inventory/group_vars/
vault.yml`, and `.vault_pass` are all gitignored -- only the `.example`
templates ship in git. Secrets referenced in role templates
(`vault_hive_jwt_secret`, `vault_hive_internal_token`,
`vault_vercel_api_token`, `vault_vercel_team_id`, `vault_stripe_secret_key`)
resolve from the encrypted `vault.yml`.

## Roles

| Role | Does |
|---|---|
| `prerequisites` | base packages, Rust toolchain, podman, firewall, Node.js |
| `firecracker_kvm` | Firecracker on hosts with real hardware `/dev/kvm` |
| `pvm_firecracker` | Firecracker via PVM on hosts without hardware KVM (imported, independently-verified role -- see its own README for the fsgsbase requirement and the critical per-VM kernel non-portability gotcha) |
| `hive_platform` | builds + installs the `hive-cloud` binary and its systemd unit |
| `mesh_bootstrap` | mesh trust config (join-proof self-admit by default, or the opt-in static `HIVE_TRUSTED_NODE_IDS` allowlist) |
| `dns_vercel` | DNS/TLS ingress systemd drop-in (Vercel DNS + ACME DNS-01), matching `RUNBOOK.md` |

## Verification

```bash
# syntax + basic sanity, no target needed
ansible-playbook playbooks/site.yml --syntax-check
ansible-playbook playbooks/node-join.yml --syntax-check
ansible-playbook playbooks/platform-only.yml --syntax-check

# dry run against a real host -- proves inventory/connectivity/templating
# actually work end-to-end without mutating anything
ansible-playbook playbooks/site.yml --check --diff --limit <a-real-host>
```

## What this does NOT cover

- The local macOS dev nodes (launchd, not systemd) -- this suite targets
  Linux cloud/bare-metal nodes. The local Mac fleet remains hand-managed via
  `scripts/shadwd.sh` and the `dev.shadw.*` launchd agents.
- The dashboard (`ui/`, Next.js) build/deploy -- a separate concern from the
  Rust platform binary this suite provisions.
- Custom per-tenant domains / per-deployment certs (RUNBOOK.md notes these
  as follow-ups to the base DNS migration this role automates).
