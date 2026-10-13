#!/usr/bin/env bash
# Load a GitHub App private key (.pem, generated once in the App's web UI) into the ansible vault
# correctly: the vault value must carry LITERAL `\n` escapes (both readers,
# ui/lib/github-installation.ts and crates/hive-cloud/src/github_app_auth.rs, un-escape it; a
# real multi-line scalar breaks the systemd Environment= line and .env.local), and it must never
# be empty (a bare `GITHUB_APP_PRIVATE_KEY=` line is falsy yet looks configured).
#
# Usage:  scripts/set-github-app-key.sh ~/Downloads/your-app.private-key.pem
#         scripts/set-github-app-key.sh --check      (report state, change nothing)
#
# The key is never printed and never leaves this machine except as vault ciphertext.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VAULT="$REPO_ROOT/ansible/inventory/group_vars/all/vault.yml"
KEYVAR="vault_github_app_private_key"

cd "$REPO_ROOT/ansible"
[ -f "$VAULT" ] || { echo "no vault at $VAULT" >&2; exit 1; }

report() {
  for k in vault_github_app_client_id vault_github_app_client_secret \
           vault_github_app_id vault_github_app_redirect_uri "$KEYVAR"; do
    # Strip both quote styles: this script writes single-quoted YAML.
    v=$(ansible-vault view "$VAULT" 2>/dev/null | grep -E "^$k:" | sed -E "s/^$k:[[:space:]]*//" | sed -E "s/^['\"](.*)['\"]$/\1/")
    if [ -z "$v" ]; then printf '  %-32s EMPTY\n' "$k"; else printf '  %-32s SET (%d chars)\n' "$k" "${#v}"; fi
  done
}

if [ "${1:-}" = "--check" ]; then
  echo "GitHub App vault state:"; report; exit 0
fi

PEM="${1:?usage: set-github-app-key.sh <path-to-private-key.pem> | --check}"
[ -f "$PEM" ] || { echo "not a file: $PEM" >&2; exit 1; }

# Prove it is a real RSA key before touching the vault; a bad paste otherwise fails later as a 401.
openssl rsa -in "$PEM" -noout -check >/dev/null 2>&1 \
  || { echo "not a valid RSA private key (openssl rejected it): $PEM" >&2; exit 1; }

umask 077
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
cp "$VAULT" "$WORK/vault.bak"

# Real newlines -> literal \n as a SINGLE-quoted YAML scalar: a double-quoted scalar would parse
# the escapes back into real newlines and render a multi-line Environment= value.
ESCAPED="$(awk 'BEGIN{ORS=""} {print $0 "\\n"}' "$PEM")"
[ -n "$ESCAPED" ] || { echo "key file produced an empty value — refusing to write" >&2; exit 1; }
# Single-quoted YAML escapes an embedded quote by doubling it.
ESCAPED="${ESCAPED//\'/\'\'}"

ansible-vault decrypt --output "$WORK/plain.yml" "$VAULT"
KEYVAR="$KEYVAR" ESCAPED="$ESCAPED" "$(head -1 "$(command -v ansible)" | sed 's/^#!//' | awk '{print $1}')" - "$WORK/plain.yml" <<'PY'
import os, re, sys
path, key, val = sys.argv[1], os.environ["KEYVAR"], os.environ["ESCAPED"]
lines = open(path).read().split("\n")
for i, ln in enumerate(lines):
    if re.match(rf'^{re.escape(key)}:\s*', ln):
        lines[i] = f"{key}: '{val}'"
        break
else:
    lines.append(f"{key}: '{val}'")
open(path, "w").write("\n".join(lines))
PY

"$(head -1 "$(command -v ansible)" | sed 's/^#!//' | awk '{print $1}')" -c \
  "import yaml,sys; d=yaml.safe_load(open('$WORK/plain.yml')); sys.exit(0 if isinstance(d,dict) and d.get('$KEYVAR') else 1)" \
  || { echo "vault YAML did not round-trip — leaving the original untouched" >&2; exit 1; }

ansible-vault encrypt --output "$VAULT" "$WORK/plain.yml"
rm -f "$WORK/plain.yml"

echo "GitHub App vault state:"; report
cat <<'EOF'

Next: deploy so the nodes actually receive it.
  ansible-playbook ansible/playbooks/parallel-deploy.yml --tags ui        # dashboard
  ansible-playbook ansible/playbooks/parallel-deploy.yml --tags backend   # webhook/auto-deploy

Then confirm installationState flips from "unconfigured" to "present":
  curl -s http://127.0.0.1:3002/api/github/status   (on any node)
EOF
