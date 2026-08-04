#!/usr/bin/env bash
# mint-operator-jwt.sh — mint a short-lived platform-operator JWT for a
# hive-cloud node, reading the signing secret from the RUNNING process's
# environment (never from a unit file — AGENTS.md's drop-in gotcha: the file
# can lie, /proc cannot).
#
# Usage:
#   scripts/mint-operator-jwt.sh <host>            # ssh root@<host> for the secret
#   scripts/mint-operator-jwt.sh --local           # read the local hive-cloud process
#   scripts/mint-operator-jwt.sh <host> <ttl_secs> # custom TTL (default 300)
#
# The token carries platform_admin:true, which is what require_operator actually
# checks (crates/hive-cloud/src/admin.rs operator_allowed). Prior ad-hoc mints
# kept failing two ways: (1) nested-quoting inline python through ssh mangled it
# (str/bytes concat errors), and (2) a {"role":"operator"}-only token is 403 —
# the full claim set is {sub, tenant, role, iat, exp, platform_admin}. This
# script exists so neither mistake is ever re-derived by hand. The secret is
# fetched over the ssh channel and the HMAC mint happens locally — nothing
# nested, nothing printed but the token.
set -euo pipefail

HOST="${1:?usage: mint-operator-jwt.sh <host>|--local [ttl_secs]}"
TTL="${2:-300}"

READ_SECRET='
pid=$(pgrep -x hive-cloud | head -1)
[ -n "$pid" ] || { echo "no hive-cloud process on this host" >&2; exit 1; }
tr "\0" "\n" < /proc/"$pid"/environ | grep "^HIVE_JWT_SECRET=" | cut -d= -f2-
'

if [ "$HOST" = "--local" ]; then
  SECRET=$(bash -c "$READ_SECRET")
else
  SECRET=$(ssh -o BatchMode=yes -o ConnectTimeout=8 "root@$HOST" "$READ_SECRET")
fi
[ -n "$SECRET" ] || { echo "HIVE_JWT_SECRET not found in hive-cloud environ" >&2; exit 1; }

python3 - "$SECRET" "$TTL" <<'PYEOF'
import hmac, hashlib, json, base64, time, sys
secret, ttl = sys.argv[1].encode(), int(sys.argv[2])
def b64(x): return base64.urlsafe_b64encode(x).rstrip(b"=").decode()
h = b64(json.dumps({"alg": "HS256", "typ": "JWT"}, separators=(",", ":")).encode())
p = b64(json.dumps({"sub": "operator", "tenant": "personal", "role": "operator",
                    "platform_admin": True, "iat": int(time.time()),
                    "exp": int(time.time()) + ttl}, separators=(",", ":")).encode())
print(h + "." + p + "." + b64(hmac.new(secret, (h + "." + p).encode(), hashlib.sha256).digest()))
PYEOF
