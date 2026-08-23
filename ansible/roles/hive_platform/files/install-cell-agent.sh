#!/usr/bin/env bash
set -euo pipefail

SOURCE="${1:?usage: hive-install-cell-agent <candidate> [destination]}"
DEST="${2:-/usr/local/bin/hive-cell-agent}"
FACT="${CELL_AGENT_RELEASE_FACT:-${DEST}.release.json}"
EXPECTED_SHA256="${CELL_AGENT_EXPECTED_SHA256:?CELL_AGENT_EXPECTED_SHA256 is required}"
EXPECTED_PROTOCOL="${CELL_AGENT_PROTOCOL_EXPECTED:-1}"
PROBE_TIMEOUT_SECS="${CELL_AGENT_PROTOCOL_TIMEOUT_SECS:-5}"
# Version of this packaging envelope only. It deliberately does not claim to
# version the Rust AgentRequest/AgentEvent wire contract.
RELEASE_SCHEMA=1

fail() {
  printf 'hive-cell-agent install refused: %s\n' "$*" >&2
  exit 1
}

for tool in flock install mktemp mv python3 sha256sum sync timeout; do
  command -v "$tool" >/dev/null 2>&1 || fail "required tool is missing: $tool"
done
case "$EXPECTED_SHA256" in
  ''|*[!0-9a-f]*) fail "expected SHA-256 must be 64 lowercase hexadecimal characters" ;;
esac
[ "${#EXPECTED_SHA256}" -eq 64 ] || fail "expected SHA-256 has the wrong length"
case "$EXPECTED_PROTOCOL" in
  ''|*[!0-9]*) fail "expected runtime-artifact protocol must be an unsigned integer" ;;
esac
case "$PROBE_TIMEOUT_SECS" in
  ''|*[!0-9]*) fail "protocol probe timeout must be an integer from 1 through 30 seconds" ;;
esac
[ "$PROBE_TIMEOUT_SECS" -ge 1 ] && [ "$PROBE_TIMEOUT_SECS" -le 30 ] || \
  fail "protocol probe timeout must be from 1 through 30 seconds"
[ -f "$SOURCE" ] && [ ! -L "$SOURCE" ] && [ -x "$SOURCE" ] || \
  fail "candidate must be a non-symlink executable regular file: $SOURCE"

DEST_DIR="$(dirname -- "$DEST")"
FACT_DIR="$(dirname -- "$FACT")"
mkdir -p -- "$DEST_DIR" "$FACT_DIR"
exec 9>"${DEST}.install.lock"
flock -n 9 || fail "another installer owns ${DEST}.install.lock"

STAGE="$(mktemp "${DEST}.new.XXXXXX")"
FACT_TMP="$(mktemp "${FACT}.new.XXXXXX")"
PROBE_ERR="$(mktemp "${DEST}.probe.XXXXXX")"
OLD_TMP=""
OLD_FACT_TMP=""
cleanup() {
  rm -f -- "$STAGE" "$FACT_TMP" "$PROBE_ERR"
  [ -z "$OLD_TMP" ] || rm -f -- "$OLD_TMP"
  [ -z "$OLD_FACT_TMP" ] || rm -f -- "$OLD_FACT_TMP"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

install -m 0755 -- "$SOURCE" "$STAGE"
STAGED_SHA256="$(sha256sum -- "$STAGE" | cut -d' ' -f1)"
[ "$STAGED_SHA256" = "$EXPECTED_SHA256" ] || \
  fail "staged digest $STAGED_SHA256 does not match built digest $EXPECTED_SHA256"

set +e
AGENT_PROTOCOL="$(timeout --signal=TERM --kill-after=1s \
  "${PROBE_TIMEOUT_SECS}s" "$STAGE" --runtime-artifact-protocol 2>"$PROBE_ERR")"
PROBE_RC=$?
set -e
[ "$PROBE_RC" -eq 0 ] || \
  fail "bounded --runtime-artifact-protocol probe failed (rc=$PROBE_RC, timeout=${PROBE_TIMEOUT_SECS}s)"
case "$AGENT_PROTOCOL" in
  ''|*[!0-9]*) fail "protocol probe returned a non-integer fact" ;;
esac
[ "$AGENT_PROTOCOL" = "$EXPECTED_PROTOCOL" ] || \
  fail "candidate protocol $AGENT_PROTOCOL does not match required $EXPECTED_PROTOCOL"

printf '{"schema":%s,"runtime_artifact_protocol":%s,"agent_sha256":"%s"}\n' \
  "$RELEASE_SCHEMA" "$AGENT_PROTOCOL" "$STAGED_SHA256" >"$FACT_TMP"
chmod 0444 "$FACT_TMP"
python3 - "$FACT_TMP" "$EXPECTED_PROTOCOL" "$STAGED_SHA256" <<'PYFACT'
import json, sys
path, expected_protocol, expected_sha = sys.argv[1:]
with open(path, 'rb') as handle:
    fact = json.load(handle)
if set(fact) != {'schema', 'runtime_artifact_protocol', 'agent_sha256'}:
    raise SystemExit('release fact has unexpected fields')
if fact['schema'] != 1:
    raise SystemExit('release fact schema is not 1')
if fact['runtime_artifact_protocol'] != int(expected_protocol):
    raise SystemExit('release fact protocol mismatch')
if fact['agent_sha256'] != expected_sha:
    raise SystemExit('release fact digest mismatch')
PYFACT

EXISTING_SHA256=""
if [ -e "$DEST" ]; then
  [ -f "$DEST" ] && [ ! -L "$DEST" ] || \
    fail "installed destination is not a non-symlink regular file: $DEST"
  EXISTING_SHA256="$(sha256sum -- "$DEST" | cut -d' ' -f1)"
fi

if [ -n "$EXISTING_SHA256" ] && [ "$EXISTING_SHA256" != "$STAGED_SHA256" ]; then
  # Remove the old proof first. A crash while rotating rollback state therefore
  # leaves an unproved .old binary, never a proof that can bless the wrong bytes.
  rm -f -- "${FACT}.old"
  OLD_TMP="$(mktemp "${DEST}.old.new.XXXXXX")"
  install -m 0755 -- "$DEST" "$OLD_TMP"
  [ "$(sha256sum -- "$OLD_TMP" | cut -d' ' -f1)" = "$EXISTING_SHA256" ] || \
    fail "rollback copy did not preserve the installed digest"
  sync -f "$OLD_TMP"
  mv -f -- "$OLD_TMP" "${DEST}.old"
  OLD_TMP=""

  if [ -f "$FACT" ] && [ ! -L "$FACT" ] && \
    python3 - "$FACT" "$EXISTING_SHA256" <<'PYOLD'
import json, sys
try:
    with open(sys.argv[1], 'rb') as handle:
        fact = json.load(handle)
except Exception:
    raise SystemExit(1)
raise SystemExit(0 if fact.get('agent_sha256') == sys.argv[2] else 1)
PYOLD
  then
    OLD_FACT_TMP="$(mktemp "${FACT}.old.new.XXXXXX")"
    install -m 0444 -- "$FACT" "$OLD_FACT_TMP"
    sync -f "$OLD_FACT_TMP"
    mv -f -- "$OLD_FACT_TMP" "${FACT}.old"
    OLD_FACT_TMP=""
  fi
fi

# Each rename is atomic within its directory. The binary moves first and the
# fact second; a crash between them produces a digest mismatch, so every rootfs
# bake fails closed until the pair is reconciled.
if [ "$EXISTING_SHA256" != "$STAGED_SHA256" ]; then
  sync -f "$STAGE"
  mv -f -- "$STAGE" "$DEST"
  STAGE=""
else
  rm -f -- "$STAGE"
  STAGE=""
fi
sync -f "$FACT_TMP"
mv -f -- "$FACT_TMP" "$FACT"
FACT_TMP=""
sync -f "$DEST_DIR"
if [ "$FACT_DIR" != "$DEST_DIR" ]; then
  sync -f "$FACT_DIR"
fi

INSTALLED_SHA256="$(sha256sum -- "$DEST" | cut -d' ' -f1)"
[ "$INSTALLED_SHA256" = "$STAGED_SHA256" ] || \
  fail "installed digest changed after atomic publication"
python3 - "$FACT" "$EXPECTED_PROTOCOL" "$INSTALLED_SHA256" <<'PYFINAL'
import json, sys
with open(sys.argv[1], 'rb') as handle:
    fact = json.load(handle)
if fact != {
    'schema': 1,
    'runtime_artifact_protocol': int(sys.argv[2]),
    'agent_sha256': sys.argv[3],
}:
    raise SystemExit('published release fact does not describe installed bytes')
PYFINAL
printf '{"schema":%s,"runtime_artifact_protocol":%s,"agent_sha256":"%s"}\n' \
  "$RELEASE_SCHEMA" "$AGENT_PROTOCOL" "$INSTALLED_SHA256"
