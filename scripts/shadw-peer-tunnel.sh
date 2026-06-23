#!/usr/bin/env bash
# Keep a BIDIRECTIONAL SSH tunnel up so this Mac's node-a and the remote Bangkok
# node can gossip BOTH ways (each must pull the other's /v1/serve-hosts to know
# where deployments are actually placed — see the Network "serving" view).
#
#   -R  remote 127.0.0.1:<RPORT>  ->  this Mac 127.0.0.1:8786 (node-a admin)
#         Bangkok runs `--peer http://127.0.0.1:<RPORT>` → announces + learns LA.
#   -L  this Mac 127.0.0.1:<FPORT> ->  Bangkok 127.0.0.1:8786 (its admin)
#         node-a runs `--peer http://127.0.0.1:<FPORT>` → learns Bangkok's
#         placement (so fc-bangkok shows as "serving" and node-a can route to it).
#
# Admins stay loopback-only; nothing is exposed publicly. The inner loop
# reconnects on drop; launchd KeepAlive backstops.
set -u
KEY="${BILLING_PEM:-$HOME/.ssh/billing.pem}"
HOSTSPEC="${PEER_HOST:-root@43.152.247.70}"
RPORT="${PEER_RPORT:-18786}"   # Bangkok-local port → node-a admin (reverse)
FPORT="${PEER_FPORT:-18796}"   # Mac-local port → Bangkok admin (forward)
LADMIN="${LA_ADMIN:-127.0.0.1:8786}"
RADMIN="${REMOTE_ADMIN:-127.0.0.1:8786}"

echo "$(date '+%F %T') peer tunnel: -R ${RPORT}->${LADMIN}  -L ${FPORT}->${RADMIN}  (${HOSTSPEC})"
while true; do
  ssh -i "$KEY" -o StrictHostKeyChecking=accept-new \
    -o ServerAliveInterval=15 -o ServerAliveCountMax=3 -o ExitOnForwardFailure=yes \
    -N \
    -R "127.0.0.1:${RPORT}:${LADMIN}" \
    -L "127.0.0.1:${FPORT}:${RADMIN}" \
    "$HOSTSPEC"
  echo "$(date '+%F %T') peer tunnel dropped (code $?) — reconnecting in 5s"
  sleep 5
done
