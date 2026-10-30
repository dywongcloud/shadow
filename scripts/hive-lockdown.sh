#!/bin/bash
# hive-lockdown — block PUBLIC access to the node gateway (TCP 8787, direct
# HTTP/HTTPS-adjacent admin port -- the fleet now serves real 80/443 via the
# DNS migration, ngrok is no longer the path), the iroh-relay metrics/README
# (TCP 9090), and the hive-rt-node workflow-runtime worker ports (TCP 3000,
# 7799-7804 — bound to 0.0.0.0 by the runtime itself with no auth/rate-limiting
# of their own, so they must never be reachable from outside the trusted
# mesh). SSH (22), iroh QUIC (UDP), the iroh-relay (3340), loopback, and the
# peer mesh nodes are all left open, so p2p connectivity, SSH tunnels, and the
# relays keep working. Idempotent; supports both iptables and nftables hosts.
#
# THIS is the fleet's sole firewall. `firewalld` (and any other stock distro
# firewall manager) must be disabled+masked on every node -- it runs its own
# netfilter tables independently of this script's iptables/nft chain, and its
# restrictive default zone silently blocks ports this script explicitly
# allows for peers (live-witnessed on fc-sanjose-2: a fresh Rocky 10 image
# ships firewalld active, whose default zone only opens 80/443/ssh, and it
# blocked the relay/discovery ports from every OTHER fleet node even though
# this script's own rules explicitly permit them).
PATH=/usr/sbin:/sbin:/usr/bin:/bin:$PATH

# Every fleet node's public IP. NOT hand-copied -- regenerate this line via
# `scripts/gen-hive-lockdown.sh`, which awk-derives it from
# ansible/inventory/hosts.ini (the same source-of-truth pattern
# scripts/deploy-ui-fleet.sh already uses for its own host list), whenever a
# node joins/leaves/changes IP. A hand-typed roster is exactly how this list
# went stale the first time (6 of 12 current nodes, missing every GPU/CVM node).
PEERS="209.23.0.183 43.166.206.175 170.106.40.67 43.172.25.45 170.106.158.151 170.106.177.140 93.188.162.67"
# Fleet nodes' PRIVATE (VPC/CCN) addresses, from `hive_private_ip=` in the same
# inventory. A peer reaching this node over the private path arrives with this
# source, never its public IP, so strict mode (below) must know both.
PRIVATE_PEERS="10.0.0.8 10.0.0.10 10.0.0.12 10.0.0.14"
# 50052 = llama.cpp rpc-server on the GPU nodes (ggml RPC backend, NO
# authentication of its own -- must never be internet-reachable; peers only).
# 50100:50999 = managed-inference llama-server endpoints (inference.rs) --
# fleet-internal OpenAI-compatible APIs reached via HIVE_INFERENCE_URL from
# app containers (whose egress NATs through a peer host IP); never public.
# Listed fleet-wide: harmless on nodes with nothing bound there. A colon
# range token works verbatim for iptables --dport; the nft branch converts
# it to nft's dash form.
LOCKED_PORTS="8787 9090 3000 7799 7800 7801 7802 7803 7804 50052 50100:50999"

# nft FIRST, iptables only as a fallback for a host that has no nft at all.
# Reordered 2026-10-09 after fc-orlando: that host ships BOTH binaries, and
# /usr/sbin/iptables there is iptables-nft, which does not create its own
# table -- it adopts the pre-existing nft-native `ip filter` table the vendor
# image installs (Atlantic.Net's cloud firewall) and appends HIVE_LOCKDOWN to
# IT. The rules then land inside a table we neither own nor control (a vendor
# re-render drops them), they are IPv4-only where the nft branch is `inet`,
# and `iptables -S` afterwards refuses outright with "table `filter' is
# incompatible, use 'nft' tool" -- so the ruleset that is actually protecting
# the host cannot be listed or managed through the tool that wrote it. The
# nft branch is also the one the fleet already runs (fc-phoenix has no
# iptables binary at all and has always built `inet hive_lockdown`).
if command -v nft >/dev/null 2>&1; then
  # Build the nft address-set literal FROM $PEERS at runtime (comma-joined)
  # instead of a second hardcoded list -- the iptables loop below and this
  # branch can no longer drift apart from each other, only $PEERS can go stale.
  PEERS_NFT="$(echo "$PEERS" | tr ' ' ',')"
  PORTS_NFT="$(echo "$LOCKED_PORTS" | tr ' :' ',-')"
  nft delete table inet hive_lockdown 2>/dev/null || true
  nft add table inet hive_lockdown
  nft 'add chain inet hive_lockdown input { type filter hook input priority -100 ; policy accept ; }'
  # Return traffic of a connection THIS host opened must never be evaluated by
  # the port drops below: a locked dport range that overlaps
  # net.ipv4.ip_local_port_range (50100-50999 does -- 32768 60999 by default)
  # gets handed out by the kernel as the SOURCE port of ordinary outbound
  # connects, so the SYN-ACK comes back with dport inside the locked range and
  # is dropped -> Connection timed out (os error 110), socket wedged in
  # SYN-SENT. Witnessed fleet-wide on fc-sanjose 2026-10-13 (2.2M dropped
  # 56-byte packets; 100% of wedged sockets had a source port in the range).
  # Belt-and-braces with `net.ipv4.ip_local_reserved_ports=50100-50999`
  # (/etc/sysctl.d/60-hive-reserved-ports.conf), which stops the kernel ever
  # allocating those ports automatically; this rule also covers a process that
  # binds one explicitly.
  nft add rule inet hive_lockdown input ct state established,related accept
  nft add rule inet hive_lockdown input iif lo accept
  nft add rule inet hive_lockdown input ip saddr "{ $PEERS_NFT }" accept
  nft add rule inet hive_lockdown input tcp dport "{ $PORTS_NFT }" drop
  # Retire the legacy iptables-nft era chain, AFTER the table above is live so
  # there is never a window without a lockdown. Hosts hardened before the
  # nft-first reorder carry `chain HIVE_LOCKDOWN` inside the vendor `ip filter`
  # table, entered by a `jump HIVE_LOCKDOWN` from INPUT. Left in place it keeps
  # enforcing its OLD roster beside the new table: both hook INPUT, so the
  # effective policy is their INTERSECTION and a peer added to $PEERS is still
  # dropped by the stale chain (reproduced in a scratch netns, 2026-10-13: a new
  # peer timed out on 8787 while the audit read the new roster as healthy).
  nft -a list table ip filter 2>/dev/null | awk '
    /^[ \t]*chain / { chain = $2 }
    /jump HIVE_LOCKDOWN/ { for (i = 1; i <= NF; i++) if ($i == "handle") print chain, $(i + 1) }
  ' | while read -r legacy_chain legacy_handle; do
    nft delete rule ip filter "$legacy_chain" handle "$legacy_handle"
  done
  if nft list chain ip filter HIVE_LOCKDOWN >/dev/null 2>&1; then
    nft flush chain ip filter HIVE_LOCKDOWN && nft delete chain ip filter HIVE_LOCKDOWN \
      || { echo "ERROR: legacy ip filter HIVE_LOCKDOWN chain could not be retired" >&2; exit 1; }
    echo "retired the legacy ip filter HIVE_LOCKDOWN chain"
  fi
  echo "lockdown applied via nftables"
elif command -v iptables >/dev/null 2>&1; then
  iptables -D INPUT -j HIVE_LOCKDOWN 2>/dev/null || true
  iptables -F HIVE_LOCKDOWN 2>/dev/null || true
  iptables -N HIVE_LOCKDOWN 2>/dev/null || true
  # Same reason as the nft branch above (see the comment there): reply packets
  # of locally-originated connections are never the thing this chain protects
  # against, so they RETURN before the dport drops see them.
  iptables -A HIVE_LOCKDOWN -m conntrack --ctstate ESTABLISHED,RELATED -j RETURN
  iptables -A HIVE_LOCKDOWN -i lo -j RETURN
  for p in $PEERS; do iptables -A HIVE_LOCKDOWN -s "$p" -j RETURN; done
  for port in $LOCKED_PORTS; do iptables -A HIVE_LOCKDOWN -p tcp --dport "$port" -j DROP; done
  iptables -A HIVE_LOCKDOWN -j RETURN
  iptables -I INPUT 1 -j HIVE_LOCKDOWN
  echo "lockdown applied via iptables"
else
  echo "ERROR: neither iptables nor nft found" >&2; exit 1
fi

# STRICT inbound, opt-in per host (`hive_lockdown_strict: true` in the
# inventory drops /etc/hive/lockdown-strict). The table above is policy-ACCEPT:
# it only blocks a short list of known-dangerous ports, so anything else a
# process binds on 0.0.0.0 is reachable by whatever the cloud security group
# lets through -- and Tencent's default group opens EVERY port to the whole
# private address space, which in a shared account includes other people's
# instances. Strict mode is default-DENY on the internet-facing interface only
# (the one carrying the default route): container bridges, litebox TUNs and
# loopback are untouched. It admits established traffic, fleet peers (public
# and private addresses), ICMP, and the ports that are public BY DESIGN:
#   TCP 22 ssh, 80 ACME/redirect, 443 HTTPS, 53 Seer, 5432/6379 the TLS
#   database gateway, 20000-29999 tenant raw ports; UDP 53 Seer, 11204 iroh
#   QUIC mesh, 20000-29999 tenant raw ports.
# A separate table at a later priority, so the fleet table above keeps working
# unchanged and fail2ban's own table (priority -1) still drops banned sources.
STRICT_FLAG=/etc/hive/lockdown-strict
STRICT_PUBLIC_TCP="22 80 443 53 5432 6379 20000:29999"
STRICT_PUBLIC_UDP="53 11204 20000:29999"
if command -v nft >/dev/null 2>&1; then
  nft delete table inet hive_strict 2>/dev/null || true
fi
if [ -f "$STRICT_FLAG" ]; then
  command -v nft >/dev/null 2>&1 || { echo "ERROR: strict lockdown requested ($STRICT_FLAG) but nft is not installed" >&2; exit 1; }
  WAN_IF="$(ip -o route show default | awk '{for (i = 1; i <= NF; i++) if ($i == "dev") { print $(i + 1); exit }}')"
  [ -n "$WAN_IF" ] || { echo "ERROR: strict lockdown: no default-route interface found" >&2; exit 1; }
  STRICT_PEERS="$(echo $PEERS $PRIVATE_PEERS | tr ' ' ',')"
  TCP_PUBLIC="$(echo "$STRICT_PUBLIC_TCP" | tr ' :' ',-')"
  UDP_PUBLIC="$(echo "$STRICT_PUBLIC_UDP" | tr ' :' ',-')"
  nft -f - <<NFT || { echo "ERROR: strict lockdown ruleset failed to load" >&2; exit 1; }
table inet hive_strict {
  chain input {
    type filter hook input priority -90; policy accept;
    iifname != "$WAN_IF" accept
    ct state established,related accept
    ct state invalid drop
    ip saddr { $STRICT_PEERS } accept
    meta l4proto { icmp, ipv6-icmp } accept
    tcp dport { $TCP_PUBLIC } accept
    udp dport { $UDP_PUBLIC } accept
    counter drop
  }
}
NFT
  echo "strict inbound lockdown applied on $WAN_IF"
fi
