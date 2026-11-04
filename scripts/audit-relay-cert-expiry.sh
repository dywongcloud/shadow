#!/usr/bin/env bash
# Read-only verdict on every iroh relay's TLS cert: strict verification from a fleet
# vantage plus runway on the served file. Exit 1 when any relay fails, so cron alerts.
set -uo pipefail

K="${SSH_KEY:-$HOME/.ssh/id_ed25519}"
O=(-i "$K" -o BatchMode=yes -o ConnectTimeout=10 -o StrictHostKeyChecking=accept-new)
D="${HIVE_RELAY_CERT_WARN_DAYS:-30}"
V="${HIVE_RELAY_CERT_VANTAGE:-93.188.162.67}"
N=(
  "fc-phoenix.relay.shadw.app:93.188.162.67"
  "fc-virginia.relay.shadw.app:43.166.206.175"
  "fc-sanjose.relay.shadw.app:170.106.158.151"
)
f=0
for e in "${N[@]}"; do
  n="${e%%:*}"; ip="${e##*:}"
  r=$(ssh "${O[@]}" "root@$V" "curl -sS --max-time 8 --resolve ${n}:3343:${ip} https://${n}:3343/ -o /dev/null -w '%{http_code} %{ssl_verify_result}' 2>/dev/null" 2>/dev/null)
  h="${r%% *}"; v="${r##* }"
  c=$(ssh "${O[@]}" "root@$ip" "openssl x509 -in /etc/iroh-relay/cert.pem -checkend $((D * 86400)) -noout >/dev/null 2>&1 && echo ok || echo short" 2>/dev/null)
  if [ "$h" = "200" ] && [ "$v" = "0" ] && [ "$c" = "ok" ]; then
    echo "PASS ${n} http=${h} verify=0 runway>=${D}d"
  else
    echo "FAIL ${n} http=${h} verify=${v} runway=${c}"
    f=1
  fi
done
exit "$f"
