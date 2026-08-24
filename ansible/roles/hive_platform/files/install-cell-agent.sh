#!/usr/bin/env bash
set -euo pipefail

SOURCE="${1:?usage: hive-install-cell-agent <candidate> [destination]}"
DEST="${2:-/usr/local/bin/hive-cell-agent}"
FACT="${CELL_AGENT_RELEASE_FACT:-${DEST}.release.json}"
EXPECTED_SHA256="${CELL_AGENT_EXPECTED_SHA256:?CELL_AGENT_EXPECTED_SHA256 is required}"
EXPECTED_ROOTFS_SCHEMA="${CELL_AGENT_ROOTFS_SCHEMA_EXPECTED:-2}"
EXPECTED_PROTOCOL="${CELL_AGENT_PROTOCOL_EXPECTED:-1}"
EXPECTED_WIRE_PROTOCOL="${CELL_AGENT_WIRE_PROTOCOL_EXPECTED:-2}"
EXPECTED_WIRE_CAPABILITIES="${CELL_AGENT_WIRE_CAPABILITIES_EXPECTED:-15}"
PROBE_TIMEOUT_SECS="${CELL_AGENT_PROTOCOL_TIMEOUT_SECS:-5}"
# Version of the complete release-fact envelope consumed by the rootfs builder.
RELEASE_SCHEMA=2

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
for numeric in "$EXPECTED_ROOTFS_SCHEMA" "$EXPECTED_PROTOCOL" \
  "$EXPECTED_WIRE_PROTOCOL" "$EXPECTED_WIRE_CAPABILITIES"; do
  case "$numeric" in
    ''|*[!0-9]*) fail "expected protocol facts must be unsigned integers" ;;
  esac
done
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
AGENT_PROTOCOL_JSON="$(timeout --signal=TERM --kill-after=1s \
  "${PROBE_TIMEOUT_SECS}s" "$STAGE" --agent-protocol-fact 2>"$PROBE_ERR")"
PROBE_RC=$?
set -e
[ "$PROBE_RC" -eq 0 ] || \
  fail "bounded --agent-protocol-fact probe failed (rc=$PROBE_RC, timeout=${PROBE_TIMEOUT_SECS}s)"
PROBE_VALUES="$(python3 - "$AGENT_PROTOCOL_JSON" "$EXPECTED_ROOTFS_SCHEMA" \
  "$EXPECTED_PROTOCOL" "$EXPECTED_WIRE_PROTOCOL" "$EXPECTED_WIRE_CAPABILITIES" <<'PYPROBE'
import json, sys
raw, rootfs, runtime, wire, capabilities = sys.argv[1:]
try:
    fact = json.loads(raw)
except Exception as error:
    raise SystemExit(f'agent protocol fact is invalid JSON: {error}')
keys = {
    'rootfs_schema', 'runtime_artifact_protocol',
    'agent_wire_protocol', 'agent_wire_capabilities',
}
if set(fact) != keys or any(type(fact[key]) is not int for key in keys):
    raise SystemExit('agent protocol fact has missing, extra, or non-integer fields')
expected = {
    'rootfs_schema': int(rootfs),
    'runtime_artifact_protocol': int(runtime),
    'agent_wire_protocol': int(wire),
    'agent_wire_capabilities': int(capabilities),
}
if fact != expected:
    raise SystemExit(f'agent protocol fact {fact!r} does not match required {expected!r}')
print(
    fact['rootfs_schema'], fact['runtime_artifact_protocol'],
    fact['agent_wire_protocol'], fact['agent_wire_capabilities'],
)
PYPROBE
)" || fail "candidate did not emit the exact required agent protocol fact"
read -r AGENT_ROOTFS_SCHEMA AGENT_PROTOCOL AGENT_WIRE_PROTOCOL AGENT_WIRE_CAPABILITIES <<<"$PROBE_VALUES"

printf '{"schema":%s,"rootfs_schema":%s,"runtime_artifact_protocol":%s,"agent_wire_protocol":%s,"agent_wire_capabilities":%s,"agent_sha256":"%s"}\n' \
  "$RELEASE_SCHEMA" "$AGENT_ROOTFS_SCHEMA" "$AGENT_PROTOCOL" "$AGENT_WIRE_PROTOCOL" \
  "$AGENT_WIRE_CAPABILITIES" "$STAGED_SHA256" >"$FACT_TMP"
chmod 0444 "$FACT_TMP"
python3 - "$FACT_TMP" "$EXPECTED_ROOTFS_SCHEMA" "$EXPECTED_PROTOCOL" \
  "$EXPECTED_WIRE_PROTOCOL" "$EXPECTED_WIRE_CAPABILITIES" "$STAGED_SHA256" <<'PYFACT'
import json, sys
path, rootfs, runtime, wire, capabilities, expected_sha = sys.argv[1:]
with open(path, 'rb') as handle:
    fact = json.load(handle)
expected = {
    'schema': 2,
    'rootfs_schema': int(rootfs),
    'runtime_artifact_protocol': int(runtime),
    'agent_wire_protocol': int(wire),
    'agent_wire_capabilities': int(capabilities),
    'agent_sha256': expected_sha,
}
if fact != expected:
    raise SystemExit('release fact does not exactly describe the staged agent')
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
python3 - "$FACT" "$EXPECTED_ROOTFS_SCHEMA" "$EXPECTED_PROTOCOL" \
  "$EXPECTED_WIRE_PROTOCOL" "$EXPECTED_WIRE_CAPABILITIES" "$INSTALLED_SHA256" <<'PYFINAL'
import json, sys
path, rootfs, runtime, wire, capabilities, digest = sys.argv[1:]
with open(path, 'rb') as handle:
    fact = json.load(handle)
if fact != {
    'schema': 2,
    'rootfs_schema': int(rootfs),
    'runtime_artifact_protocol': int(runtime),
    'agent_wire_protocol': int(wire),
    'agent_wire_capabilities': int(capabilities),
    'agent_sha256': digest,
}:
    raise SystemExit('published release fact does not describe installed bytes')
PYFINAL
printf '{"schema":%s,"rootfs_schema":%s,"runtime_artifact_protocol":%s,"agent_wire_protocol":%s,"agent_wire_capabilities":%s,"agent_sha256":"%s"}\n' \
  "$RELEASE_SCHEMA" "$AGENT_ROOTFS_SCHEMA" "$AGENT_PROTOCOL" "$AGENT_WIRE_PROTOCOL" \
  "$AGENT_WIRE_CAPABILITIES" "$INSTALLED_SHA256"
