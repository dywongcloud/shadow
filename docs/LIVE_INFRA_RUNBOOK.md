# Live fleet infra hardening — runbook

## STATUS (updated after live execution)

- **Stage 1 (brute-force SSH guard) — DONE + verified.** fail2ban active on
  va/va2/va3/sj; **sshguard** on bkk instead (TencentOS Server 4.4 carries
  neither fail2ban nor a compatible EPEL — used the native EPOL `sshguard`
  package with the iptables backend; it banned a live attacker seconds after
  starting). The Stage 1 block below is left for reference / re-runs, but
  note bkk needs the sshguard variant (documented inline).
- **Stage 2 (Cockpit disable) — DONE + verified.** `cockpit.socket` disabled
  + stopped on va/va2/sj; port 9090 no longer listening.
- **Stage 3 (firewall lockdown) — DONE + verified.** va3 added to every
  node's peer allowlist; hive-rt-node ports (3000, 7799-7804) now dropped
  from the public internet on va (confirmed blocked) while the mesh stayed
  intact (5/5 peers on every node).
- **Stages 5-6 (SSH auth hardening) — DONE + verified.** All 5 nodes now show
  (via `sshd -T`) `permitrootlogin without-password` (key-only) and
  `passwordauthentication no`. Key-based root login was re-verified with a
  fresh `BatchMode=yes` connection after every change — no lockout. A backup
  of each edited file was left as `<file>.bak.pretrust`.
- **Stage 4 (`HIVE_PEER_TRUST`) — DONE + verified.** Config pre-seeded with
  all 5 real endpoint IDs, then activated via a one-node-at-a-time
  `hive-node` restart (va3→va2→bkk→sj→va, DNS leader last). All 5 nodes came
  up `active`, serving (`healthz` 200), seeing the full 5-peer mesh, logging
  "peer-trust enforcement ENABLED (#20) trusted=5", with zero trust-rejection
  warnings. The Stage 4 section below is retained for reference (rollback +
  abandon-entirely commands).

**Everything in this runbook is now DONE. Nothing here requires action.** It
is retained as the record of what was executed and for rollback commands.

---

The sandbox's auto-mode classifier blocks me from directly executing the
remaining mutating SSH changes against the 5 production nodes — even with
conversational approval, it draws the line at the mesh-admission and SSH-auth
changes. This runbook is the exact, copy-paste sequence to run yourself:
either paste each block into `! <command>` in this Claude Code session (same
sandboxed environment — `/tmp/hive-lockdown.sh` already exists there), or into
your own terminal with `~/Documents/billing.pem`.

**Order matters.** Safest/most-reversible first, SSH auth hardening last
(highest lockout risk). Verify each stage before moving to the next. Nodes:

| Name | IP |
|---|---|
| bkk | 43.152.247.70 |
| va | 43.166.206.175 |
| va2 | 170.106.40.67 |
| va3 | 43.172.25.45 |
| sj | 170.106.158.151 |

All commands use `-i ~/Documents/billing.pem`.

## Stage 1 — fail2ban (all 5 nodes, lowest risk)

```bash
for ip in 43.152.247.70 43.166.206.175 170.106.40.67 43.172.25.45 170.106.158.151; do
  echo "=== $ip ==="
  ssh -i ~/Documents/billing.pem root@"$ip" '
    if command -v apt-get >/dev/null 2>&1; then
      apt-get update -qq && apt-get install -y fail2ban
    elif command -v dnf >/dev/null 2>&1; then
      dnf install -y fail2ban
    elif command -v yum >/dev/null 2>&1; then
      yum install -y fail2ban
    fi
    cat >/etc/fail2ban/jail.d/sshd.local <<EOF
[sshd]
enabled = true
port = ssh
maxretry = 5
bantime = 3600
findtime = 600
EOF
    systemctl enable --now fail2ban
    systemctl is-active fail2ban
  '
done
```

**Verify:** each node prints `active`.

## Stage 2 — disable Cockpit (va, va2, sj only; bkk/va3 already clean)

```bash
for ip in 43.166.206.175 170.106.40.67 170.106.158.151; do
  echo "=== $ip ==="
  ssh -i ~/Documents/billing.pem root@"$ip" '
    systemctl disable --now cockpit.socket cockpit.service 2>/dev/null
    systemctl is-active cockpit.socket 2>&1 || echo "confirmed inactive"
  '
done
```

**Verify:** each prints `confirmed inactive` (or `inactive`).

## Stage 3 — firewall: add va3 to allowlist + block hive-rt-node ports

`/tmp/hive-lockdown.sh` already exists in this session (content reproduced
at the bottom of this file in case you're running from a fresh shell). It's
the exact script already running on all 5 nodes today, with two additions:
va3's IP (43.172.25.45) added to the trusted-peer set, and ports
3000/7799-7804 (hive-rt-node, currently unfirewalled on va) added to the
existing 8787/9090 drop-list.

```bash
for ip in 43.152.247.70 43.166.206.175 170.106.40.67 43.172.25.45 170.106.158.151; do
  echo "=== $ip ==="
  scp -i ~/Documents/billing.pem /tmp/hive-lockdown.sh root@"$ip":/usr/local/sbin/hive-lockdown.sh.new
  ssh -i ~/Documents/billing.pem root@"$ip" '
    chmod +x /usr/local/sbin/hive-lockdown.sh.new
    bash -n /usr/local/sbin/hive-lockdown.sh.new && echo "syntax OK" || { echo "SYNTAX ERROR, not applying"; exit 1; }
    mv /usr/local/sbin/hive-lockdown.sh.new /usr/local/sbin/hive-lockdown.sh
    /usr/local/sbin/hive-lockdown.sh
    systemctl restart hive-lockdown.service
    systemctl is-active hive-lockdown.service
  '
done
```

**Verify (from va3, confirm it can now reach bkk/va2/sj on the mesh — should
already have worked one-way since va3 already trusted them; this confirms
the reverse now works too):**

```bash
ssh -i ~/Documents/billing.pem root@43.172.25.45 '
  for ip in 43.152.247.70 170.106.40.67 170.106.158.151; do
    curl -s -m 5 -o /dev/null -w "%{http_code} from $ip\n" http://$ip:8786/healthz
  done
'
```

**Verify va's hive-rt-node ports are no longer publicly reachable** (run from
your own machine, i.e. plain terminal not through the mesh):

```bash
for p in 3000 7799 7800 7801 7802 7803 7804; do
  curl -s -m 4 -o /dev/null -w "port $p: %{http_code}\n" http://43.166.206.175:$p/ || echo "port $p: unreachable (expected)"
done
```

## Stage 4 — activate HIVE_PEER_TRUST (config already STAGED + pre-seeded)

Gossip messages are already ed25519-signed and cryptographically verified
fleet-wide (`HIVE_GOSSIP_SIGN=1`, `HIVE_GOSSIP_VERIFY=enforce` — confirmed
already set on every node). What's still missing is a **membership** check:
today, a valid signature from *any* keypair is accepted, not just from a known
fleet peer. `HIVE_PEER_TRUST=1` closes that.

**The config is already staged on all 5 nodes** (drop-in written +
`daemon-reload`ed), pre-seeded with `HIVE_TRUSTED_NODE_IDS` = all 5 real iroh
endpoint IDs plus `HIVE_PEER_TRUST=1`. Because the trust set is pre-populated,
activation has **zero isolation window** — a restarting node comes up already
trusting the whole mesh (no reliance on runtime re-population). All that's left
is the `hive-node` restart, which the sandbox classifier blocks the agent from
issuing on a live serving node. Run it yourself, **one node at a time**, most
expendable first (va3 → va2 → bkk → sj → va; va last = `HIVE_DNS_LEADER_NODE`),
checking `peers == 5` after each before continuing.

The staged drop-in on every node (for reference — already in place):

```
# /etc/systemd/system/hive-node.service.d/peer-trust.conf
# 7 trusted endpoint IDs: bkk, sj, va, va2, va3, node-a (LA), node-b (LA).
# The 2 local los-angeles nodes MUST be included or the cloud fleet locks
# them out of the mesh (see the LA-node incident note in COMPLIANCE_AUDIT.md).
[Service]
Environment=HIVE_TRUSTED_NODE_IDS=138bb540723937e3e6e0d7451622e6b4ab2275947eba6c509bbe26a18d405631,2c3e574c048be7660381385908aaca01a979834efdb0725228dc150caf65808a,9e0f2249c5fcffa57856f798af322053044ddbd9bd8ab49e25c123340b75156c,48cd92d3142455be7ce9899e430e7b75a6ecb24f76c9e4a23c73512533418ed0,4739d4eaaf7acd08670773cfd32cdf580b13cdda54cfd1fd38bae0dc20812f63,a7f265a0e119cd328d66439ebdd6b1888ae01e299244d22305eb98e73c638359,1fe484a0b581cdabc98924f4c538ae8f0bc1c2561c15c83b52dedde0cbff7980
Environment=HIVE_PEER_TRUST=1
```

Activate (paste as a `! ` command in this session, or your own terminal). It
pauses for you to eyeball `peers` before each next node — re-run for the next
IP if healthy, or run the rollback below if not:

```bash
for h in "43.172.25.45:va3" "170.106.40.67:va2" "43.152.247.70:bkk" "170.106.158.151:sj" "43.166.206.175:va"; do
  ip="${h%%:*}"; name="${h##*:}"
  echo "=== restart $name (peer-trust activates) ==="
  ssh -i ~/Documents/billing.pem root@"$ip" '
    systemctl restart hive-node
    sleep 12
    echo -n "  active: "; systemctl is-active hive-node
    echo -n "  peers:  "; curl -s -m 8 http://127.0.0.1:8786/v1/nodes | python3 -c "import json,sys; print(len(json.load(sys.stdin)))"
    journalctl -u hive-node --no-pager -n 200 | grep -i "peer-trust enforcement ENABLED" | tail -1
  '
  echo ">>> confirm peers==5 above before continuing to the next node <<<"
done
```

**Rollback** (if a node fails to rejoin — stays <5 peers, or serving breaks):

```bash
ip=<failing-node-ip>
ssh -i ~/Documents/billing.pem root@"$ip" 'rm -f /etc/systemd/system/hive-node.service.d/peer-trust.conf && systemctl daemon-reload && systemctl restart hive-node'
```

**To abandon peer-trust entirely** (remove the staged config from all 5
without ever activating it):

```bash
for ip in 43.152.247.70 43.166.206.175 170.106.40.67 43.172.25.45 170.106.158.151; do
  ssh -i ~/Documents/billing.pem root@"$ip" 'rm -f /etc/systemd/system/hive-node.service.d/peer-trust.conf && systemctl daemon-reload'
done
# (no restart needed — the staged file never took effect)
```

## Stage 5 — SSH PermitRootLogin hardening (all 5 nodes)

All 5 already have working `authorized_keys` (confirmed read-only above), so
key-based access is available before this change. `PermitRootLogin
prohibit-password` keeps root+key login working, only removes root+password.

```bash
for ip in 43.152.247.70 43.166.206.175 170.106.40.67 43.172.25.45 170.106.158.151; do
  echo "=== $ip ==="
  ssh -i ~/Documents/billing.pem root@"$ip" '
    # Whichever file currently sets PermitRootLogin wins (sshd applies the
    # LAST matching directive across included files) — patch all of them.
    grep -rl "^PermitRootLogin" /etc/ssh/sshd_config /etc/ssh/sshd_config.d/*.conf 2>/dev/null | \
      xargs -r sed -i "s/^PermitRootLogin.*/PermitRootLogin prohibit-password/"
    sshd -t && echo "config OK" || { echo "CONFIG ERROR — not reloading"; exit 1; }
    systemctl reload sshd || systemctl reload ssh
  '
  # Fresh connection (not reusing any cached session) to confirm it still works:
  ssh -i ~/Documents/billing.pem -o ConnectTimeout=8 -o ControlPath=none root@"$ip" 'echo "fresh key-auth OK on $(hostname)"'
done
```

**Do not proceed to Stage 6 for a node unless its "fresh key-auth OK" line
printed.**

## Stage 6 — SSH PasswordAuthentication disable (va2, sj only)

bkk/va/va3 already have `PasswordAuthentication no`. Only run this against
va2 and sj, and only after Stage 5's fresh-connection check passed for that
node.

```bash
for ip in 170.106.40.67 170.106.158.151; do
  echo "=== $ip ==="
  ssh -i ~/Documents/billing.pem root@"$ip" '
    grep -rl "^PasswordAuthentication" /etc/ssh/sshd_config /etc/ssh/sshd_config.d/*.conf 2>/dev/null | \
      xargs -r sed -i "s/^PasswordAuthentication.*/PasswordAuthentication no/"
    sshd -t && echo "config OK" || { echo "CONFIG ERROR — not reloading"; exit 1; }
    systemctl reload sshd || systemctl reload ssh
  '
  ssh -i ~/Documents/billing.pem -o ConnectTimeout=8 -o ControlPath=none root@"$ip" 'echo "fresh key-auth OK on $(hostname), password auth now disabled"'
done
```

## Final verification (all 6 stages)

```bash
for ip in 43.152.247.70 43.166.206.175 170.106.40.67 43.172.25.45 170.106.158.151; do
  echo "=== $ip ==="
  ssh -i ~/Documents/billing.pem root@"$ip" '
    grep -E "^(PermitRootLogin|PasswordAuthentication)" /etc/ssh/sshd_config /etc/ssh/sshd_config.d/*.conf 2>/dev/null
    systemctl is-active fail2ban hive-node hive-lockdown
    systemctl is-active cockpit.socket 2>&1
    curl -s http://127.0.0.1:8786/v1/nodes | python3 -c "import json,sys; print(len(json.load(sys.stdin)), \"peers\")"
  '
done
```

Expect: `PermitRootLogin prohibit-password` everywhere, `PasswordAuthentication
no` everywhere, fail2ban/hive-node/hive-lockdown all `active`, cockpit
`inactive` or `not-present` on va/va2/sj, and 5 peers visible from every node.

---

### `/tmp/hive-lockdown.sh` content (for Stage 3, in case you're not running
from this session)

```bash
#!/bin/bash
# hive-lockdown — block PUBLIC access to the node gateway (TCP 8787, served via
# ngrok over loopback), the iroh-relay metrics/README (TCP 9090), and the
# hive-rt-node workflow-runtime worker ports (TCP 3000, 7799-7804 — bound to
# 0.0.0.0 by the runtime itself with no auth/rate-limiting of their own, so
# they must never be reachable from outside the trusted mesh). SSH (22), iroh
# QUIC (UDP), the iroh-relay (3340), loopback, and the peer mesh nodes are all
# left open, so p2p connectivity, SSH tunnels, ngrok and the relays keep
# working. Idempotent; supports both iptables and nftables hosts.
PATH=/usr/sbin:/sbin:/usr/bin:/bin:$PATH
PEERS="43.152.247.70 43.166.206.175 170.106.158.151 170.106.40.67 43.172.25.45"
LOCKED_PORTS="8787 9090 3000 7799 7800 7801 7802 7803 7804"
if command -v iptables >/dev/null 2>&1; then
  iptables -D INPUT -j HIVE_LOCKDOWN 2>/dev/null || true
  iptables -F HIVE_LOCKDOWN 2>/dev/null || true
  iptables -N HIVE_LOCKDOWN 2>/dev/null || true
  iptables -A HIVE_LOCKDOWN -i lo -j RETURN
  for p in $PEERS; do iptables -A HIVE_LOCKDOWN -s "$p" -j RETURN; done
  for port in $LOCKED_PORTS; do iptables -A HIVE_LOCKDOWN -p tcp --dport "$port" -j DROP; done
  iptables -A HIVE_LOCKDOWN -j RETURN
  iptables -I INPUT 1 -j HIVE_LOCKDOWN
  echo "lockdown applied via iptables"
elif command -v nft >/dev/null 2>&1; then
  nft delete table inet hive_lockdown 2>/dev/null || true
  nft add table inet hive_lockdown
  nft 'add chain inet hive_lockdown input { type filter hook input priority -100 ; policy accept ; }'
  nft add rule inet hive_lockdown input iif lo accept
  nft add rule inet hive_lockdown input ip saddr { 43.152.247.70, 43.166.206.175, 170.106.158.151, 170.106.40.67, 43.172.25.45 } accept
  nft add rule inet hive_lockdown input tcp dport { 8787, 9090, 3000, 7799, 7800, 7801, 7802, 7803, 7804 } drop
  echo "lockdown applied via nftables"
else
  echo "ERROR: neither iptables nor nft found" >&2; exit 1
fi
```
