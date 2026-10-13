#!/usr/bin/env bash
# One-action cutover of the production dashboard from the Clerk DEVELOPMENT instance (third-party
# session on *.clerk.accounts.dev, blocked by iOS ITP) to a Clerk PRODUCTION instance served
# first-party at clerk.shadw.cloud. Creating the production instance is a Clerk DASHBOARD action
# (the Backend API key is scoped to one instance): create it, set the domain to shadw.cloud, copy
# the pk_live_/sk_live_ keys, then run this script, which:
#   1. creates the instance's CNAMEs in Vercel DNS (the platform reconciler only touches A/AAAA/NS)
#   2. waits for public resolution + Clerk FAPI
#   3. swaps the keys in ui/.env.local locally and on every node (rsync-excluded from deploys)
#   4. runs scripts/deploy-ui-fleet.sh (the publishable key is baked into the client bundle)
#
# Usage:
#   scripts/clerk-prod-cutover.sh --check                       # read-only preflight
#   CLERK_PK_LIVE=pk_live_… CLERK_SK_LIVE=sk_live_… scripts/clerk-prod-cutover.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PEM="${HIVE_FLEET_PEM:-$HOME/Documents/billing.pem}"
INVENTORY="$REPO_ROOT/ansible/inventory/hosts.ini"
ENV_LOCAL="$REPO_ROOT/ui/.env.local"
CHECK_ONLY=0
[ "${1:-}" = "--check" ] && CHECK_ONLY=1

PK="${CLERK_PK_LIVE:-}"
SK="${CLERK_SK_LIVE:-}"

fail() { echo "FATAL: $*" >&2; exit 1; }
mask() { printf '%s\n' "${1:0:12}…(${#1} chars)"; }

[ -f "$INVENTORY" ] || fail "inventory not found: $INVENTORY"
[ -f "$ENV_LOCAL" ] || fail "local env file not found: $ENV_LOCAL"
[ -f "$PEM" ] || fail "fleet SSH key not found: $PEM"

ssh_opts=(-i "$PEM" -o StrictHostKeyChecking=no -o ConnectTimeout=8)

mapfile -t HOSTS < <(
  awk '
    /^hive_cp_owner_chain=/ { sub(/^hive_cp_owner_chain=/,""); split($0, c, ","); leader_name=c[1] }
    /ansible_host=/ {
      ip=""; name="";
      for (i=1; i<=NF; i++) {
        if ($i ~ /^ansible_host=/) { split($i, a, "="); ip=a[2] }
        if ($i ~ /^hive_name=/)    { split($i, b, "="); name=b[2] }
      }
      if (ip != "") { ips[++n]=ip; names[n]=name }
    }
    END {
      for (i=1; i<=n; i++) if (names[i] == leader_name) print ips[i];
      for (i=1; i<=n; i++) if (names[i] != leader_name) print ips[i];
    }
  ' "$INVENTORY"
)
[ ${#HOSTS[@]} -gt 0 ] || fail "no hosts parsed from inventory"
echo "fleet: ${#HOSTS[@]} node(s), leader-first: ${HOSTS[*]}"

VERCEL_TOKEN="${VERCEL_API_TOKEN:-}"
if [ -z "$VERCEL_TOKEN" ]; then
  VERCEL_TOKEN="$(ssh "${ssh_opts[@]}" "root@${HOSTS[0]}" \
    'pid=$(systemctl show -p MainPID --value hive-node); tr "\0" "\n" < /proc/$pid/environ | sed -n "s/^VERCEL_API_TOKEN=//p"' | tr -d '\r\n')"
fi
[ -n "$VERCEL_TOKEN" ] || fail "no VERCEL_API_TOKEN in env or on leader ${HOSTS[0]}"
echo "vercel token: $(mask "$VERCEL_TOKEN")"

clerk_get() { curl -sS -H "Authorization: Bearer $1" "https://api.clerk.com/v1/$2"; }

CUR_SK="$(sed -n 's/^CLERK_SECRET_KEY=//p' "$ENV_LOCAL" | head -1)"
if [ -n "$CUR_SK" ]; then
  echo "current instance ($(mask "$CUR_SK")): $(clerk_get "$CUR_SK" instance)"
fi
echo "existing clerk-ish DNS records in the zone:"
curl -sS -H "Authorization: Bearer $VERCEL_TOKEN" \
  "https://api.vercel.com/v4/domains/shadw.cloud/records?limit=100" |
  python3 -c 'import sys,json;[print(" ",r["type"],r["name"],"->",r["value"]) for r in json.load(sys.stdin).get("records",[]) if any(k in (r.get("name") or "") for k in ("clerk","accounts","clkmail","domainkey"))]' \
  || fail "vercel record listing failed (token invalid?)"

if [ "$CHECK_ONLY" = 1 ]; then
  echo "--check: preflight complete. Provide CLERK_PK_LIVE/CLERK_SK_LIVE to cut over."
  exit 0
fi

case "$PK" in pk_live_*) ;; *) fail "CLERK_PK_LIVE must be a pk_live_… key (got: ${PK:-<empty>})";; esac
case "$SK" in sk_live_*) ;; *) fail "CLERK_SK_LIVE must be an sk_live_… key (got: $(mask "${SK:-<empty>}"))";; esac

INSTANCE_JSON="$(clerk_get "$SK" instance)"
echo "target instance: $INSTANCE_JSON"
echo "$INSTANCE_JSON" | python3 -c '
import sys, json
d = json.load(sys.stdin)
et = d.get("environment_type")
if et != "production":
    raise SystemExit("refusing: instance %s is %r, not production" % (d.get("id"), et))
' || fail "target instance is not a production instance"

# The frontend host is encoded in the pk (base64 of "<host>$").
FAPI_HOST="$(printf '%s' "${PK#pk_live_}" | base64 -d 2>/dev/null | tr -d '$' || true)"
echo "frontend API host from key: ${FAPI_HOST:-<undecodable>}"
case "$FAPI_HOST" in
  *.shadw.cloud) ;;
  *) fail "pk_live decodes to '$FAPI_HOST' — not under shadw.cloud; the whole point is a first-party session. Set the instance domain to shadw.cloud in the Clerk dashboard first." ;;
esac

DOMAINS_JSON="$(clerk_get "$SK" domains)"
echo "$DOMAINS_JSON" | python3 -c '
import sys, json
d = json.load(sys.stdin)
prim = [x for x in d.get("data", []) if not x.get("is_satellite")]
assert prim, "no primary domain on the production instance"
dom = prim[0]
zone = dom["name"]
targets = dom.get("cname_targets") or []
assert targets, ("production domain %r has no cname_targets in the API response - "
                 "read them off the Clerk dashboard DNS page instead" % zone)
for t in targets:
    host = t["host"]
    name = host[:-len("." + zone)] if host.endswith("." + zone) else host
    print("\t".join([zone, name, t["value"], "required" if t.get("required", True) else "optional"]))
' > /tmp/clerk_cnames.tsv
echo "required CNAMEs from Clerk:"
column -t /tmp/clerk_cnames.tsv

ZONE="$(head -1 /tmp/clerk_cnames.tsv | cut -f1)"
EXISTING="$(curl -sS -H "Authorization: Bearer $VERCEL_TOKEN" "https://api.vercel.com/v4/domains/$ZONE/records?limit=100")"
while IFS=$'\t' read -r zone name value _req; do
  if echo "$EXISTING" | python3 -c "
import sys,json
recs=json.load(sys.stdin).get('records',[])
hit=[r for r in recs if r.get('name')=='$name' and r.get('type')=='CNAME' and (r.get('value') or '').rstrip('.')=='$value'.rstrip('.')]
sys.exit(0 if hit else 1)"; then
    echo "exists, skipping: CNAME $name -> $value"
    continue
  fi
  echo "creating: CNAME $name.$zone -> $value"
  for attempt in 1 2 3 4 5; do
    code="$(curl -sS -o /tmp/clerk_dns_resp.json -w '%{http_code}' -X POST \
      -H "Authorization: Bearer $VERCEL_TOKEN" -H "Content-Type: application/json" \
      -d "{\"name\":\"$name\",\"type\":\"CNAME\",\"value\":\"$value\",\"ttl\":60}" \
      "https://api.vercel.com/v2/domains/$zone/records")"
    if [ "$code" = 200 ] || [ "$code" = 201 ]; then break; fi
    echo "  attempt $attempt: HTTP $code $(cat /tmp/clerk_dns_resp.json | head -c 200) — retrying"
    sleep $((attempt * 2))
    [ "$attempt" = 5 ] && fail "could not create CNAME $name after 5 attempts"
  done
  sleep 1.2   # Vercel DNS creates are individually rate-limited — pace them.
done < /tmp/clerk_cnames.tsv

echo "waiting for public DNS + Clerk to provision (TLS can take minutes)…"
deadline=$(( $(date +%s) + 600 ))
while :; do
  ok=1
  while IFS=$'\t' read -r zone name value req; do
    [ "$req" = required ] || continue
    fqdn="$name.$zone"
    got="$(dig +short CNAME "$fqdn" @1.1.1.1 | head -1)"
    [ -n "$got" ] || { ok=0; echo "  not yet visible: $fqdn"; }
  done < /tmp/clerk_cnames.tsv
  [ "$ok" = 1 ] && break
  [ "$(date +%s)" -ge "$deadline" ] && { echo "WARN: DNS not fully visible after 10m; continuing (Clerk re-checks continuously)"; break; }
  sleep 15
done
env_code="$(curl -sS -o /dev/null -w '%{http_code}' "https://$FAPI_HOST/v1/environment" || true)"
echo "https://$FAPI_HOST/v1/environment -> HTTP $env_code (200 once Clerk finishes deploying; if not yet, the env/deploy below is still safe — Clerk serves the moment its deploy completes)"

cp "$ENV_LOCAL" "$ENV_LOCAL.pre-clerk-prod.bak"
sed -i '' \
  -e "s|^NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY=.*|NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY=$PK|" \
  -e "s|^CLERK_SECRET_KEY=.*|CLERK_SECRET_KEY=$SK|" "$ENV_LOCAL"
echo "local $ENV_LOCAL updated (backup: .pre-clerk-prod.bak)"

failed=()
for host in "${HOSTS[@]}"; do
  echo "==> $host: updating /root/hive/ui/.env.local"
  if ! ssh "${ssh_opts[@]}" "root@$host" "
    set -e
    f=/root/hive/ui/.env.local
    [ -f \$f ] || { echo 'missing \$f'; exit 1; }
    cp \$f \$f.pre-clerk-prod.bak
    sed -i -e 's|^NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY=.*|NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY=$PK|' \
           -e 's|^CLERK_SECRET_KEY=.*|CLERK_SECRET_KEY=$SK|' \$f
    grep -c '^NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY=pk_live_' \$f
  "; then
    echo "==> $host: FAILED env swap"; failed+=("$host")
  fi
done
[ ${#failed[@]} -eq 0 ] || fail "env swap failed on: ${failed[*]} — fix before deploying (a mixed fleet = split-brain auth)"

echo "==> deploying ui fleet-wide (bakes pk_live into the client bundle)"
"$REPO_ROOT/scripts/deploy-ui-fleet.sh"

echo "==> post-check: live HTML key"
live_pk="$(curl -sS https://shadw.cloud/ | grep -o 'pk_live_[A-Za-z0-9]*' | head -1 || true)"
echo "shadw.cloud now serves: ${live_pk:-<no pk_live found - check the node builds>}"
echo
echo "DONE. Note: existing signed-in users must sign in once more (dev-instance"
echo "sessions do not carry over to the production instance)."
