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
PEERS="43.152.247.70 43.128.46.225 43.166.206.175 170.106.40.67 43.172.25.45 170.106.158.151 43.173.78.95 43.153.114.15 43.172.117.92 43.135.132.93 43.166.76.159 162.62.83.144 43.166.223.197 43.166.233.114 93.188.162.67 43.133.30.68 150.109.247.150 170.106.162.175 43.130.153.237 43.153.106.173 170.106.155.130 162.62.83.91"
# 50052 = llama.cpp rpc-server on the GPU nodes (ggml RPC backend, NO
# authentication of its own -- must never be internet-reachable; peers only).
# 50100:50999 = managed-inference llama-server endpoints (inference.rs) --
# fleet-internal OpenAI-compatible APIs reached via HIVE_INFERENCE_URL from
# app containers (whose egress NATs through a peer host IP); never public.
# Listed fleet-wide: harmless on nodes with nothing bound there. A colon
# range token works verbatim for iptables --dport; the nft branch converts
# it to nft's dash form.
LOCKED_PORTS="8787 9090 3000 7799 7800 7801 7802 7803 7804 50052 50100:50999"

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
  # Build the nft address-set literal FROM $PEERS at runtime (comma-joined)
  # instead of a second hardcoded list -- the iptables loop above and this
  # branch can no longer drift apart from each other, only $PEERS can go stale.
  PEERS_NFT="$(echo "$PEERS" | tr ' ' ',')"
  PORTS_NFT="$(echo "$LOCKED_PORTS" | tr ' :' ',-')"
  nft delete table inet hive_lockdown 2>/dev/null || true
  nft add table inet hive_lockdown
  nft 'add chain inet hive_lockdown input { type filter hook input priority -100 ; policy accept ; }'
  nft add rule inet hive_lockdown input iif lo accept
  nft add rule inet hive_lockdown input ip saddr "{ $PEERS_NFT }" accept
  nft add rule inet hive_lockdown input tcp dport "{ $PORTS_NFT }" drop
  echo "lockdown applied via nftables"
else
  echo "ERROR: neither iptables nor nft found" >&2; exit 1
fi
