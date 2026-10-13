#!/usr/bin/env bash
# Mint a short-lived platform-operator JWT for a hive-cloud node. The signing secret is read from the RUNNING process's
# environment (/proc/<pid>/environ), never from a unit file: the file can lie, /proc cannot.
#
# Usage:
#   scripts/mint-operator-jwt.sh <host>            # ssh root@<host> for the secret
#   scripts/mint-operator-jwt.sh --local           # read the local hive-cloud process
#   scripts/mint-operator-jwt.sh <host> <ttl_secs> # custom TTL (default 300)
#
# The token carries the full claim set `operator_allowed` checks (platform_admin:true, crates/hive-cloud/src/admin.rs); a
# {"role":"operator"}-only token is 403. The secret travels over the ssh channel and the HMAC is minted locally, so only
# the token is printed.
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
