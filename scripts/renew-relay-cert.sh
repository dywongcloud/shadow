#!/usr/bin/env bash
# Distribute the renewed *.relay.shadw.app wildcard cert to each iroh-relay host.
# acme.sh renews the files under ~/.acme.sh; Le_RenewHook calls this script. The relay's
# cert_mode="Reloading" polls only every 24h, so each relay needs a restart to serve it.
set -euo pipefail

DOMAIN="*.relay.shadw.app"
DOMAIN_APEX="relay.shadw.app"
ACME="${ACME_SH:-$HOME/.acme.sh/acme.sh}"
SSH_KEY="${SSH_KEY:-$HOME/.ssh/id_ed25519}"
SSH_OPTS=(-i "$SSH_KEY" -o BatchMode=yes -o ConnectTimeout=15 -o StrictHostKeyChecking=accept-new)
CERT_DIR="$HOME/.acme.sh/${DOMAIN}_ecc"
WARN_DAYS="${HIVE_RELAY_CERT_WARN_DAYS:-30}"
VANTAGE="${HIVE_RELAY_CERT_VANTAGE:-93.188.162.67}"

RELAYS=(
  "fc-phoenix.relay.shadw.app:93.188.162.67"
  "fc-virginia.relay.shadw.app:43.166.206.175"
  "fc-sanjose.relay.shadw.app:170.106.158.151"
)

if [[ "${1:-}" == "--issue" ]]; then
  [[ -x "$ACME" ]] || { echo "acme.sh missing at $ACME" >&2; exit 1; }
  "$ACME" --issue --dns dns_vercel -d "$DOMAIN" -d "$DOMAIN_APEX" "${@:2}"
fi

CERT="$CERT_DIR/fullchain.cer"
KEY="$CERT_DIR/${DOMAIN}.key"
[[ -f "$CERT" && -f "$KEY" ]] || { echo "renewed cert/key missing under $CERT_DIR" >&2; exit 1; }
openssl x509 -in "$CERT" -checkend $((WARN_DAYS * 86400)) -noout >/dev/null || {
  echo "renewed cert expires within ${WARN_DAYS}d; refusing to distribute" >&2; exit 1; }

stage=/tmp/hive-relay-cert-stage
for entry in "${RELAYS[@]}"; do
  name="${entry%%:*}"; ip="${entry##*:}"
  echo "== $name ($ip) ==" >&2
  extra=()
  if [[ "$ip" == 170.106.* ]]; then
    extra=(-o ProxyJump=root@43.166.206.175)
  fi
  ssh "${SSH_OPTS[@]}" "${extra[@]}" "root@$ip" "rm -rf $stage; mkdir -m 700 $stage"
  scp "${SSH_OPTS[@]}" "${extra[@]}" "$CERT" "root@$ip:$stage/fullchain.cer"
  scp "${SSH_OPTS[@]}" "${extra[@]}" "$KEY" "root@$ip:$stage/relay.key"

  install_remote=$(cat <<'REMOTE'
set -e
S=/tmp/hive-relay-cert-stage
C=/etc/iroh-relay/cert.pem
K=/etc/iroh-relay/key.pem
A=$(openssl x509 -in "$S/fullchain.cer" -pubkey -noout | openssl pkey -pubin -outform der | sha256sum | cut -d' ' -f1)
B=$(openssl pkey -in "$S/relay.key" -pubout -outform der | sha256sum | cut -d' ' -f1)
[ "$A" = "$B" ] || { echo "REFUSED: cert/key pubkey mismatch" >&2; exit 3; }
TS=$(date -u +%Y%m%dT%H%M%SZ)
cp -p "$C" "$C.bak-$TS"; cp -p "$K" "$K.bak-$TS"
install -m 644 "$S/fullchain.cer" "$C"; install -m 600 "$S/relay.key" "$K"
systemctl restart iroh-relay.service
for i in $(seq 1 15); do
  ss -lnt | awk '$4 ~ /:3343$/ {ok=1} END{exit !ok}' && break
  sleep 1
done
rm -rf "$S"
echo "$TS"
REMOTE
)
  ts=$(ssh "${SSH_OPTS[@]}" "${extra[@]}" "root@$ip" bash -s <<< "$install_remote") || {
    echo "install failed on $name; backups are /etc/iroh-relay/*.bak-*" >&2; exit 1; }

  probe="curl -sS --max-time 8 --resolve ${name}:3343:${ip} https://${name}:3343/ -o /dev/null -w '%{http_code} %{ssl_verify_result}' 2>/dev/null || true"
  result=$(ssh "${SSH_OPTS[@]}" "root@$VANTAGE" "$probe")
  http="${result%% *}"; verify="${result##* }"
  if [[ "$http" != "200" || "$verify" != "0" ]]; then
    echo "FAILED strict TLS on $name from $VANTAGE: http=$http verify=$verify (backup $ts)" >&2
    exit 1
  fi
  echo "  strict TLS ok (http=$http verify=0), backup $ts" >&2
done

echo "== audit ==" >&2
"$(cd "$(dirname "$0")" && pwd)/audit-relay-cert-expiry.sh"
