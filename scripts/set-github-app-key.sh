#!/usr/bin/env bash
# Load a GitHub App private key (.pem) into the ansible vault.
#
# GitHub has NO API that issues a private key for an EXISTING App — the key is
# generated once in the web UI (App settings -> "Generate a private key") and
# downloaded as `<slug>.<date>.private-key.pem`. This script is the other half:
# it takes that file and puts it in the vault correctly, so the two things that
# are easy to get wrong by hand are not gotten wrong.
#
# Those two things:
#   1. NEWLINES. The vault value must carry LITERAL `\n` escapes, not real
#      newlines. Both readers (ui/lib/github-installation.ts and
#      crates/hive-cloud/src/github_app_auth.rs) accept the escaped form and
#      un-escape it themselves; a real multi-line scalar renders a broken
#      `Environment=` line in the systemd unit and a broken `.env.local`.
#   2. EMPTY-VS-ABSENT. An empty value renders a bare `GITHUB_APP_PRIVATE_KEY=`
#      line, which is falsy in the JS that reads it — indistinguishable from
#      "configured" to anyone eyeballing the env file. The templates guard on
#      length for this reason; this script refuses to write an empty value.
#
# Usage:  scripts/set-github-app-key.sh ~/Downloads/your-app.private-key.pem
#         scripts/set-github-app-key.sh --check      (report state, change nothing)
#
# The key is never printed, never echoed, and never leaves this machine except
# as vault ciphertext. The vault is gitignored.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VAULT="$REPO_ROOT/ansible/inventory/group_vars/all/vault.yml"
KEYVAR="vault_github_app_private_key"

cd "$REPO_ROOT/ansible"
[ -f "$VAULT" ] || { echo "no vault at $VAULT" >&2; exit 1; }

# Report the CURRENT state of every GitHub App var by name + length only.
report() {
  for k in vault_github_app_client_id vault_github_app_client_secret \
           vault_github_app_id vault_github_app_redirect_uri "$KEYVAR"; do
    # Strip BOTH quote styles: this script writes single-quoted YAML (double
    # quotes would interpret the key's `\n` escapes), so a `"`-only strip
    # reported every value as 2 chars longer than it is — cosmetic, but it makes
    # a correct vault look wrong at exactly the moment you are checking it.
    v=$(ansible-vault view "$VAULT" 2>/dev/null | grep -E "^$k:" | sed -E "s/^$k:[[:space:]]*//" | sed -E "s/^['\"](.*)['\"]$/\1/")
    if [ -z "$v" ]; then printf '  %-32s EMPTY\n' "$k"; else printf '  %-32s SET (%d chars)\n' "$k" "${#v}"; fi
  done
}

if [ "${1:-}" = "--check" ]; then
  echo "GitHub App vault state:"; report; exit 0
fi

PEM="${1:?usage: set-github-app-key.sh <path-to-private-key.pem> | --check}"
[ -f "$PEM" ] || { echo "not a file: $PEM" >&2; exit 1; }

# Prove it is a real RSA key BEFORE touching the vault — a truncated or
# wrong-file paste otherwise fails much later, at runtime, as an opaque 401.
openssl rsa -in "$PEM" -noout -check >/dev/null 2>&1 \
  || { echo "not a valid RSA private key (openssl rejected it): $PEM" >&2; exit 1; }

umask 077
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
cp "$VAULT" "$WORK/vault.bak"

# Real newlines -> literal \n, written as a SINGLE-QUOTED YAML scalar.
#
# The quoting style is load-bearing, not cosmetic. A YAML DOUBLE-quoted scalar
# processes backslash escapes, so `key: "...\n..."` is parsed back into REAL
# newlines — the value reaches the template as a 28-line block, renders as a
# multi-line `GITHUB_APP_PRIVATE_KEY=-----BEGIN...` in .env.local, and every
# line after the first becomes garbage. Caught by rendering the real template
# and counting lines; reading the file would never have shown it. A
# SINGLE-quoted scalar performs no escape processing, so the two-character `\n`
# survives to the template, which is exactly what both readers un-escape.
ESCAPED="$(awk 'BEGIN{ORS=""} {print $0 "\\n"}' "$PEM")"
[ -n "$ESCAPED" ] || { echo "key file produced an empty value — refusing to write" >&2; exit 1; }
# Single-quoted YAML escapes an embedded quote by doubling it. A PEM contains
# none, but a malformed input must not be able to break out of the scalar.
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

# Parse the result before re-encrypting: a broken vault is worse than an unset key.
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
