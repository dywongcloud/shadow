#!/usr/bin/env bash
# Renew the wildcard TLS cert for the standalone iroh-relay.service on the relay-hosting nodes
# (RELAY_NODES) and redistribute it. Each relay's
# /etc/iroh-relay.toml runs cert_mode = "Reloading", which re-reads manual_cert_path/key on a
# 24h poll, so renewal is copy-two-files with no restart. Not scheduled anywhere yet.
#
# Requires: acme.sh (curl https://get.acme.sh | sh); VERCEL_API_TOKEN is read live from the sj
# node's hive-node.service unit, so the credential is configured in exactly one place.
#
# Usage:
#   scripts/renew-relay-cert.sh                 # issue + distribute to every RELAY_NODES entry
#   SSH_KEY=~/.ssh/other.pem scripts/renew-relay-cert.sh
set -euo pipefail

DOMAIN="*.relay.shadw.app"
DOMAIN_APEX="relay.shadw.app"
SSH_KEY="${SSH_KEY:-$HOME/.ssh/id_ed25519}"
SSH_OPTS=(-i "$SSH_KEY" -o ConnectTimeout=8 -o StrictHostKeyChecking=accept-new -o BatchMode=yes)
ACME="${ACME_SH:-$HOME/.acme.sh/acme.sh}"

declare -A RELAY_NODES=(
  [fc-phoenix]=93.188.162.67
  [fc-virginia]=43.166.206.175
  [fc-sanjose]=170.106.158.151
)
CRED_HOST=170.106.158.151 # fc-sanjose already holds VERCEL_API_TOKEN

if [ ! -x "$ACME" ]; then
  echo "acme.sh not found at $ACME — install: curl https://get.acme.sh | sh" >&2
  exit 1
fi

echo "== fetching VERCEL_API_TOKEN from fc-sanjose's own hive-node.service unit ==" >&2
VERCEL_TOKEN=$(ssh "${SSH_OPTS[@]}" "root@$CRED_HOST" \
  "systemctl show hive-node -p Environment --value" \
  | tr ' ' '\n' | grep '^VERCEL_API_TOKEN=' | cut -d= -f2)
if [ -z "$VERCEL_TOKEN" ]; then
  echo "could not read VERCEL_API_TOKEN from $CRED_HOST" >&2
  exit 1
fi
export VERCEL_TOKEN

echo "== issuing/renewing $DOMAIN via acme.sh (dns_vercel) ==" >&2
# No --force: acme.sh renews only inside its own window; pass --force by hand for a SAN change
# or key compromise.
"$ACME" --issue --dns dns_vercel -d "$DOMAIN" -d "$DOMAIN_APEX" "$@"

CERT_DIR="$HOME/.acme.sh/${DOMAIN}_ecc"
if [ ! -f "$CERT_DIR/fullchain.cer" ] || [ ! -f "$CERT_DIR/${DOMAIN}.key" ]; then
  echo "issuance succeeded but expected files missing under $CERT_DIR" >&2
  exit 1
fi

for name in "${!RELAY_NODES[@]}"; do
  ip="${RELAY_NODES[$name]}"
  echo "== distributing to $name ($ip) ==" >&2
  ssh "${SSH_OPTS[@]}" "root@$ip" \
    "cp /etc/iroh-relay/cert.pem /etc/iroh-relay/cert.pem.bak-\$(date +%s); cp /etc/iroh-relay/key.pem /etc/iroh-relay/key.pem.bak-\$(date +%s)"
  scp "${SSH_OPTS[@]}" "$CERT_DIR/fullchain.cer" "root@$ip:/etc/iroh-relay/cert.pem"
  scp "${SSH_OPTS[@]}" "$CERT_DIR/${DOMAIN}.key" "root@$ip:/etc/iroh-relay/key.pem"
  # No restart: Reloading mode picks the files up on its next poll — the interruption it exists to avoid.
  echo "  cert+key copied; iroh-relay will pick it up within its next reload poll (no restart triggered)" >&2
done

echo "== done: verifying live cert on all 3 ==" >&2
for name in "${!RELAY_NODES[@]}"; do
  ip="${RELAY_NODES[$name]}"
  subject=$(echo | openssl s_client -connect "$ip:3343" -servername "$name.relay.shadw.app" 2>/dev/null \
    | openssl x509 -noout -subject 2>/dev/null || echo "UNREACHABLE")
  echo "  $name: $subject (note: will still show the file's cert; Reloading serves the NEW one only after its own poll fires or the node is restarted)"
done
