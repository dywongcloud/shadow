#!/usr/bin/env bash
# Renew the wildcard TLS cert for the standalone iroh-relay.service on every
# relay-hosting fleet node (bkk/va/sj — the 3 nodes HIVE_RELAY_URLS names),
# and redistribute it.
#
# Why this exists / how it fits together (see AGENTS.md's Geo-DNS & ACME
# section and `recall("relay-tls-acme-automation")` for the full design):
# each relay node's /etc/iroh-relay.toml runs `cert_mode = "Reloading"`
# (crates.io iroh-relay 1.0.x — verified against real vendored source,
# 2026-08-02: Reloading reads the EXACT SAME manual_cert_path/manual_key_path
# files as Manual mode, just re-reads them on a fixed 24h poll and hot-swaps
# with NO restart). So renewal is just: get a fresh cert, overwrite those two
# files on all 3 nodes, done — no service restart, no code change, no
# redeploy of hive-cloud itself needed.
#
# This script does NOT run automatically yet — it's real, tested, reusable
# tooling (matches the exact manual process that issued the currently-live
# cert), meant to be run by an operator or wired into a scheduler (cron on a
# node that already holds VERCEL_API_TOKEN — sj does — or an external CI
# job) once someone decides where that scheduled run should live. That's a
# separate operational decision (which host runs the timer, what alerts on
# failure) from "does the renewal mechanism work," which this script answers.
#
# Requires: acme.sh installed locally (curl https://get.acme.sh | sh), and
# the real VERCEL_API_TOKEN this fleet already uses for DNS-01 (read live off
# the sj node's own hive-node.service unit rather than duplicated here, so
# there is exactly one place this credential is configured).
#
# Usage:
#   scripts/renew-relay-cert.sh                 # issue + distribute to all 3
#   SSH_KEY=~/.ssh/other.pem scripts/renew-relay-cert.sh
set -euo pipefail

DOMAIN="*.relay.shadw.app"
DOMAIN_APEX="relay.shadw.app"
SSH_KEY="${SSH_KEY:-$HOME/.ssh/id_ed25519}"
SSH_OPTS=(-i "$SSH_KEY" -o ConnectTimeout=8 -o StrictHostKeyChecking=accept-new -o BatchMode=yes)
ACME="${ACME_SH:-$HOME/.acme.sh/acme.sh}"

# name -> ip, matching ansible/inventory/hosts.ini's bkk/va/sj entries —
# hardcoded here rather than parsed from the inventory (unlike
# audit-runtime-versions.sh) because this script targets a fixed, small,
# named set of relay-TLS nodes specifically, not "every node of some group."
declare -A RELAY_NODES=(
  [fc-bangkok]=43.152.247.70
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
# No --force: acme.sh's own default (skip if not within its renewal window,
# ~1/3 of validity remaining) is exactly right for a script meant to run
# periodically/on a schedule -- forcing every run would burn real rate-limit
# budget on issuances that were never due. Pass --force by hand for the rare
# case (SAN change, key compromise) that genuinely needs an out-of-window
# reissue.
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
  # No restart: cert_mode=Reloading picks up the new files within its own
  # poll interval (24h default). This deliberately does NOT force an
  # immediate restart -- that would reintroduce the brief interruption
  # Reloading mode exists to avoid.
  echo "  cert+key copied; iroh-relay will pick it up within its next reload poll (no restart triggered)" >&2
done

echo "== done: verifying live cert on all 3 ==" >&2
for name in "${!RELAY_NODES[@]}"; do
  ip="${RELAY_NODES[$name]}"
  subject=$(echo | openssl s_client -connect "$ip:3343" -servername "$name.relay.shadw.app" 2>/dev/null \
    | openssl x509 -noout -subject 2>/dev/null || echo "UNREACHABLE")
  echo "  $name: $subject (note: will still show the file's cert; Reloading serves the NEW one only after its own poll fires or the node is restarted)"
done
