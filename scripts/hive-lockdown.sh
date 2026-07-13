#!/bin/bash
# hive-lockdown — block PUBLIC access to the node gateway (TCP 8787, served via
# ngrok over loopback), the iroh-relay metrics/README (TCP 9090), and the
# hive-rt-node workflow-runtime worker ports (TCP 3000, 7799-7804 — bound to
# 0.0.0.0 by the runtime itself with no auth/rate-limiting of their own, so
# they must never be reachable from outside the trusted mesh). SSH (22), iroh
# QUIC (UDP), the iroh-relay (3340), loopback, and the peer mesh nodes are all
# left open, so p2p connectivity, SSH tunnels, ngrok and the relays keep
# working. Idempotent; supports both iptables and nftables hosts.
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

# Every fleet node's public IP — kept in sync by hand (this script has no
# runtime way to discover the roster; see PEERS in the deployed copy on each
# node, updated whenever a node joins/leaves).
PEERS="43.152.247.70 43.166.206.175 170.106.158.151 170.106.40.67 43.172.25.45 43.173.78.95"
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
  nft add rule inet hive_lockdown input ip saddr { 43.152.247.70, 43.166.206.175, 170.106.158.151, 170.106.40.67, 43.172.25.45, 43.173.78.95 } accept
  nft add rule inet hive_lockdown input tcp dport { 8787, 9090, 3000, 7799, 7800, 7801, 7802, 7803, 7804 } drop
  echo "lockdown applied via nftables"
else
  echo "ERROR: neither iptables nor nft found" >&2; exit 1
fi
