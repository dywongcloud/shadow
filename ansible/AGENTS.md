# ansible/AGENTS.md

Failure rules for `ansible/` and `scripts/` (rolls, host bring-up, role contracts): present-tense, what must
be true now; history lives in `CHANGELOG.md`. Cap 12,000 bytes (`wc -c ansible/AGENTS.md`); past it,
recompile from the code, the git log and the memory files. Loaded through `ansible/CLAUDE.md`; the root
`AGENTS.md` rules still apply.

## Building and rolling

- `scripts/build-fleet-binary.sh [2.38|2.39] [--clean-jemalloc]` builds the fleet binaries (verifies
  `.symtab`, so heap dumps symbolize). cargo-zigbuild builds BOTH glibc targets into one
  `target/x86_64-unknown-linux-gnu/` and zig applies the glibc version at LINK time, so the script removes
  the binary and bumps the bin crate's mtime to force a relink. A link failing on undefined
  `sdallocx`/`mallocx` is a 96-byte stub `libjemalloc.a` from a killed build (`--clean-jemalloc`). `GROUPS`
  is a special shell array: the script uses `TARGET_GROUPS`.
- A roll: `ansible-playbook ansible/playbooks/parallel-deploy.yml --tags backend` (the unit is
  `hive-node.service`, never `hive-cloud`), once the read-only `scripts/audit-secret-keys.sh` passes (a node
  with a set-but-EMPTY or malformed `HIVE_SECRET_KEY` will not boot). It verifies CONTENT (this run's
  artifact sha against the on-disk binary and `/proc/<MainPID>/exe`, unit start after the run began on each
  host's own clock), refuses a run that changed no host's binary unless `allow_noop_deploy=true`, restarts
  the backend serial 1 and the UI serial 3, and re-kicks the first three restarted nodes (their trunks
  formed against a fleet that then bounced). `rsync -a` preserves mtimes, so the fan-out build host touches
  `crates/**/*.rs` when a sync changed anything (cargo fingerprints would skip the rebuild); `vendor/` rides
  every roll (never edit it between a roll's sync and its build); the cr-sqlite extension
  (`/var/lib/hive/crsqlite.so`, per glibc group) is built and installed on EVERY run.
- `scripts/deploy-ui-fleet.sh`: ONE build, many nodes (Next/Turbopack builds are not deterministic: per-node
  builds behind one round-robin hostname 404 each other's chunks): the elected build host (the control-plane
  leader) builds once and every other host gets the identical artifact (`--exclude=.gm`). It awk-matches
  `ansible_host=` INCLUDING comments: delete dead hosts, never comment them. `hive-ui.service`'s PATH starts
  with node's own directory (`hive_ui_node_dir`, discovered by `hive_ui` and the `hive_ui_fanout` restart,
  never defaulted): `next` runs through `#!/usr/bin/env node`. Dashboard hosts need Node >= 22.5
  (`@workflow/web` needs `node:sqlite`); its patched build is a second artifact overlaid on every follower
  after `npm ci` and marker-verified. Use one ssh ControlMaster per host (sshd `MaxStartups` drops parallel
  pre-auth connections).

## Inventory, vault, gotchas

- `ansible/inventory/hosts.ini` is an untracked, git-ignored operator file (its edits never ride a commit).
  `hive_retired=true` means the instance is gone: `gen-hive-lockdown.sh` drops it from PEERS, `audit-*.sh`
  skip it unless `INCLUDE_RETIRED=1`, fail2ban excludes it (a provider re-issues a released IP to a
  stranger). The sections `[glibc239]`/`[glibc238]`/`[browser_nodes]`/`[litebox]` still NAME the five
  deleted hosts (bkk hk va2 sp fr) with no definition line; `[glibc238]` has no live member; every live host
  is glibc 2.39 (`scripts/audit-runtime-versions.sh`). `litebox_verified=true` stays on every host that has
  run verified Litebox: an unlimited `--tags litebox` run otherwise removes its drop-in and restarts it into
  MockBackend.
- `ansible.cfg`: `forks = 20` (the default 5 silently batches every play without `serial:`;
  `serial:`/`throttle:` are the real bounds); `vault_password_file = .vault_pass` is mandatory: a missing
  gitignored `ansible/.vault_pass` aborts EVERY ansible command, `--syntax-check` included.
- Gotchas: TencentOS reports the OS family "TencentOS Server", so assert `ansible_pkg_mgr == "dnf"`; test
  exact versions, never a substring ('7.2.1' accepts 7.2.10); `get_url` never re-downloads an existing dest
  (pin + checksum + `force: true`); group_vars/all outranks inventory `[group:vars]`, so
  `hive_cp_owner_chain`/`hive_cp_voters` are never defaulted there; a Jinja regex_search with a capture
  group returns None on no match (use `regex_findall`). A verification task must FAIL when its input fact is
  missing (`failed_when: x is defined and <mismatch>` passes exactly when the build play never ran).

## Credentials and templates

- `roles/hive_ui/templates/env.local.j2` OVERWRITES the node's whole `.env.local`, so the Clerk, Composio
  and GitHub App blocks are each guarded `| default('', true) | string | length > 0` (undefined, null, empty
  and numeric values all survive; `default('') | length` aborts the render on a YAML null or integer), never
  `is defined` (an EMPTY vault key renders a bare falsy `KEY=`: next-server 500s, working keys wiped).
  `hive_owner_email` decides `isOwner` in `ui/app/api/token/route.ts` and the backend's `admin_emails`, so a
  hand-added address survives only until the next `--tags ui` run.
- `GITHUB_APP_PRIVATE_KEY` is DOUBLE-QUOTED in `.env.local` (systemd's `EnvironmentFile=` eats the backslash
  of an unquoted `\n`) and a single-quoted scalar with LITERAL `\n` escapes in the node env; load keys only
  through `scripts/set-github-app-key.sh`. The App pair reaches hive-node as
  `EnvironmentFile=/etc/hive-github-app.env` (0600, drop-in `github-app.conf`): that keeps it out of the
  world-readable unit but is NOT the final custody boundary (systemd expands it into `/proc/<pid>/environ`).
- Dedicated IPv4 needs `TENCENTCLOUD_SECRET_ID`/`_SECRET_KEY` fleet-wide plus each node's OWN
  `hive_tencent_region` and `hive_tencent_instance_id` (`hive-node.service.j2` renders the block only when
  all exist; never a shared default). The Tencent CCN private path dials the SAME `HIVE_IROH_PORT`: open
  inbound UDP between CCN peers in the Tencent SG scoped to the CCN CIDR, never 0.0.0.0/0.

## Node bring-up and operations

- `playbooks/node-join.yml` builds the allowlist it writes and propagates from the LIVE ids slurped off an
  existing member's `peer-trust.conf` (`fleet_base_trust_ids`), never the inventory default
  `hive_trusted_node_ids: []` (that would cut every node off from its peers).
- `roles/prerequisites`: firewalld stays disabled+masked, Cockpit masked, the sshd drop-in is
  `00-hive-hardening.conf` (sshd keeps the FIRST value it reads); `net.core.rmem_max`/`wmem_max` = 8388608
  (iroh wants a 7 MiB UDP buffer, sized at bind time: hive-node restarts once); toolchains are pinned and
  sha256-verified (`bun-linux-x64` needs AVX2).
- `hive_health_timeout` (10) must exceed hive-p2p's 5 s connect budget and stay below the probe interval;
  `hive_p2p_discovery_ms` is 8000 (the code default 4000 ships the DHT inert); `hive_dht_port` stays empty.
  Relay URLs (`hive_relay_urls`, the browser node's relays) are the https `:3343` hostnames (wildcard cert
  `*.relay.shadw.app`: an IP-literal URL fails verification).
- fail2ban installs from the upstream tarball (TencentOS 4 has no package), bans via
  `nftables-multiport`/`-allports`, and `ignoreip` is every non-retired host plus
  `hive_fail2ban_ignoreip_extra`.
- A guardian-filled node (`GUARDIAN_GC_SECS=0` disables blob GC and the store grows without bound): stop
  hive-node, remove `iroh_store`, `iroh_docs` AND `keystore` together under the guardian `iroh/` dir (a kept
  keystore wedges init), keep `node_secret.key`.
- fc-orlando (Rocky 10, SELinux Enforcing, real KVM, so no `pti=off`): a binary under /root fails 203/EXEC
  until labelled bin_t (`semanage fcontext` + `restorecon`), and `runsc` needs
  `--security-opt label=disable`, which the tree never passes, so that host runs `crun`.

## Isolation roles

- Firecracker: `firecracker_version` (`group_vars/fc_kvm.yml`) stays outside CVE-2026-5747 (1.13.0-1.14.3,
  1.15.0) and CVE-2026-1386 (<=1.13.1, 1.14.0); the install clobbers on any checksum difference, so the role
  refuses an OLDER version over a newer. The PVM fork builds for `x86_64-unknown-linux-gnu`, which has no
  seccomp JSON: PVM hosts run the VMM without seccomp.
- PVM: the host kernel is virt-pvm pvm-612 plus a patch series pinned by a FULL 40-char sha (its ancestry
  includes the KVM shadow-paging use-after-free fix: never move the pin older), applied with GNU
  `patch --fuzz=3` (never `git apply`/`--3way`), declared never globbed, to BOTH kernel trees from one sha
  (`force: true` is load-bearing: mixed revisions triple-fault). `site.yml` runs `pvm_firecracker` with
  `serial: 1` (the role REBOOTS, so a roll stops at the first host that fails to boot); `pvm_skip` excludes
  a host. `pvm_already_provisioned` (/dev/kvm present and the running release CONTAINS `host_localversion`)
  makes the kernel stages no-ops; the smoke test is off by default (`pvm_run_smoke_test`: booting a microVM
  on an unproven PVM kernel hard-resets the host).
- Litebox role: patches apply in order `networking.patch`, `fork-inplace.patch`, `tcp-keepalive.patch` and
  fail loudly when a pin bump stops one applying; the `git` task carries
  `refspec: +<litebox_commit>:refs/remotes/origin/pinned` (upstream's history was rewritten twice and left
  the pin dangling: only a full 40-hex sha fetches it). Before re-pinning run the six-step legitimacy check
  in `roles/litebox/files/PATCHES.md` (the strongest step: `networking.patch` still passes
  `git apply --check`), then `hive-cloud --litebox-probe`. Staging the runner never restarts hive-node (it
  reads the path at startup, only with `HIVE_LITEBOX_VERIFIED=1`).
- BuildExecutor roles: the probe mirrors a real build (hive-build-init needs no_new_privs CLEAR, so never
  `--security-opt=no-new-privileges`; exactly the eleven `BUILDER_CAPABILITIES`). The nft policy loads
  BEFORE the Netavark network and is re-applied every run; `policy_id` is `sha256-<hex>`, never
  `sha256:<hex>` (`validate_identifier` rejects the whole capability). `build_executor` (run by
  `playbooks/nw-build-executor.yml`) and the opt-in `hive_build_executor` both publish
  `/var/lib/hive/build-executor.json`: never both on one host; `roles/hive_build_executor/vars/main.yml`
  stays byte-identical to the `REVIEWED_*` constants or the capability goes blank fleet-wide.
- The cell agent is the guest's PID 1 and the guest's glibc is the OCI base image's, so it is built
  STATICALLY (`-C target-feature=+crt-static`) and `scripts/build-rootfs.sh` refuses a dynamic one;
  `hive-install-cell-agent` publishes proof-first (SHA-verify, bounded `--agent-protocol-fact` probe, rename
  the binary BEFORE the release fact); rootfs image names are sanitized identically in four places
  (`build-rootfs.sh` and three Rust filters): keep them in sync.

## Browser nodes

- A headless node's only credential is a JWT minted by `POST /v1/token` with `x-hive-internal`
  (`fresh_user_claims` rejects `key:` subjects, `role == "service"` and tokens older than
  `HIVE_BROWSER_SESSION_MAX_AGE_SECS` 300); `scripts/hive-browser-node-broker.mjs` holds it via systemd
  `LoadCredential=` and mints member-role single-tenant cookies from its own configuration, never the
  request body. Headless Chromium keeps the four liveness flags (an occluded page's timers drop to about
  1/minute) and runs WITHOUT `--no-sandbox`; the supervisor recycles only after consecutive UNREACHABLE
  `/healthz` probes (never a 503); the host page never sends `unloading` (its admission DELETE writes a
  10-min relay-denylist entry).
- `playbooks/browser-nodes.yml` polls fleet presence (12 x 10 s) before asserting it; the
  `hive_browser_node` handlers run in DECLARATION order, so the broker restarts before the browser: never
  reorder them. node-worker's `dist/` is a COMMITTED artifact built by `scripts/build-node-worker.sh` and
  published by `ui/scripts/sync-browser-node.mjs`.
