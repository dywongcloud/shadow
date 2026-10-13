#!/usr/bin/env bash
# Keep a BIDIRECTIONAL SSH tunnel up so this Mac's node-a and a remote node gossip both ways (each pulls the other's
# /v1/serve-hosts to learn where deployments are placed): -R exposes node-a's admin on the remote's loopback, -L exposes the
# remote's admin on this Mac's loopback. Both stay loopback-only; the loop reconnects on drop and launchd KeepAlive backstops.
set -u
KEY="${BILLING_PEM:-$HOME/.ssh/billing.pem}"
HOSTSPEC="${PEER_HOST:-root@43.152.247.70}"
REMOTE_REVERSE_PORT="${PEER_RPORT:-18786}"
LOCAL_FORWARD_PORT="${PEER_FPORT:-18796}"
LOCAL_ADMIN="${LA_ADMIN:-127.0.0.1:8786}"
REMOTE_ADMIN="${REMOTE_ADMIN:-127.0.0.1:8786}"

echo "$(date '+%F %T') peer tunnel: -R ${REMOTE_REVERSE_PORT}->${LOCAL_ADMIN}  -L ${LOCAL_FORWARD_PORT}->${REMOTE_ADMIN}  (${HOSTSPEC})"
while true; do
  ssh -i "$KEY" -o StrictHostKeyChecking=accept-new \
    -o ServerAliveInterval=15 -o ServerAliveCountMax=3 -o ExitOnForwardFailure=yes \
    -N \
    -R "127.0.0.1:${REMOTE_REVERSE_PORT}:${LOCAL_ADMIN}" \
    -L "127.0.0.1:${LOCAL_FORWARD_PORT}:${REMOTE_ADMIN}" \
    "$HOSTSPEC"
  echo "$(date '+%F %T') peer tunnel dropped (code $?) — reconnecting in 5s"
  sleep 5
done
