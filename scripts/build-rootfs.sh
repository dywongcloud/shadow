#!/usr/bin/env bash
# Build an ext4 rootfs for a Hive cell from an OCI image, with the cell agent
# baked in as the guest init. Run this INSIDE the Lima guest (needs root, an OCI
# container CLI, and mkfs.ext4). The result lands in
# $ROOTFS_DIR/<sanitized-image>.ext4 and is what
# the Firecracker backend boots for that logical image name.
#
# Usage: sudo ./build-rootfs.sh <oci-image> <logical-name> [size-mib]
#        sudo ./build-rootfs.sh --preflight-agent
#   e.g. sudo ./build-rootfs.sh node:20-slim default 4096
#
# WASMER (hive_core::Runtime::Wasmer): set WASMER_TARBALL=/path/to/
# wasmer-linux-amd64.tar.gz to bake the `wasmer` CLI into the guest. This is
# REQUIRED for Wasmer functions on a Firecracker node and is not optional
# plumbing: `hive-cell-agent` runs as PID1 INSIDE the microVM and execs
# `start_cmd[0]` against the GUEST PATH, so a wasmer installed on the host is
# invisible to it. Only `bin/wasmer` is staged (~200 MiB); the tarball's
# `lib/*.a` static archives are another ~560 MiB that `wasmer run` never opens.
# When staged, a `<name>.wasmer` marker file is written NEXT TO the image — that
# marker is what `hive-cloud`'s `detect_wasm_runtime` probe stats to decide
# whether this node may advertise `NodeInfo::wasm_runtime`, because mounting an
# ext4 image to look inside would need root and a loop device at every boot.
#
# BUN (hive_core::Runtime::Bun): identical requirement and identical reason —
# `hive-cell-agent` execs a Bun `start_cmd[0]` against the GUEST PATH too, so a
# `bun` installed on the host is equally invisible to it. Two staging forms,
# since (unlike wasmer's upstream `.tar.gz`) Bun's own release artifact is a
# `.zip`: set BUN_TARBALL=/path/to/bun-linux-x64.tar.gz for a repackaged
# tarball with `bun` at its root (`tar czf bun-linux-x64.tar.gz -C
# bun-linux-x64 bun`, mirroring the wasmer convention above exactly), or set
# BUN_BINARY=/path/to/bun for an already-extracted executable (e.g. after
# unzipping the upstream artifact yourself) — set at most one. When staged, a
# `<name>.bun` marker file is written NEXT TO the image, the same way and for
# the same reason as `<name>.wasmer` — `hive-cloud`'s `detect_bun_runtime`
# probe (`NodeInfo::bun_runtime`) stats it.
set -euo pipefail
# Convert controller/timeout signals into a normal shell exit so whichever EXIT
# cleanup is current unmounts and removes private staging before the remote
# restart guard brings hive-node back.
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

AGENT_BIN="${AGENT_BIN:-/usr/local/bin/hive-cell-agent}"
AGENT_RELEASE_FACT="${AGENT_RELEASE_FACT:-${AGENT_BIN}.release.json}"
ROOTFS_SCHEMA_EXPECTED="${ROOTFS_SCHEMA_EXPECTED:-2}"
RUNTIME_ARTIFACT_PROTOCOL_EXPECTED="${RUNTIME_ARTIFACT_PROTOCOL_EXPECTED:-1}"
AGENT_WIRE_PROTOCOL_EXPECTED="${AGENT_WIRE_PROTOCOL_EXPECTED:-2}"
AGENT_WIRE_CAPABILITIES_EXPECTED="${AGENT_WIRE_CAPABILITIES_EXPECTED:-15}"
AGENT_PROTOCOL_TIMEOUT_SECS="${AGENT_PROTOCOL_TIMEOUT_SECS:-5}"
CELL_AGENT_RELEASE_SCHEMA_EXPECTED="${CELL_AGENT_RELEASE_SCHEMA_EXPECTED:-2}"

preflight_agent() {
  command -v debugfs >/dev/null 2>&1 || {
    echo "debugfs is required to verify exact in-rootfs facts" >&2
    return 1
  }
  command -v flock >/dev/null 2>&1 || {
    echo "flock is required to serialize cell-agent packaging with the rootfs bake" >&2
    return 1
  }
  command -v python3 >/dev/null 2>&1 || {
    echo "python3 is required to verify the cell-agent release fact" >&2
    return 1
  }
  command -v sha256sum >/dev/null 2>&1 || {
    echo "sha256sum is required to verify the cell-agent release" >&2
    return 1
  }
  command -v timeout >/dev/null 2>&1 || {
    echo "timeout is required to bound the cell-agent protocol probe" >&2
    return 1
  }
  local numeric name
  for name in ROOTFS_SCHEMA_EXPECTED RUNTIME_ARTIFACT_PROTOCOL_EXPECTED \
    AGENT_WIRE_PROTOCOL_EXPECTED AGENT_WIRE_CAPABILITIES_EXPECTED \
    CELL_AGENT_RELEASE_SCHEMA_EXPECTED; do
    numeric="${!name}"
    case "$numeric" in
      ''|*[!0-9]*) echo "$name must be an unsigned integer" >&2; return 1 ;;
    esac
  done
  case "$AGENT_PROTOCOL_TIMEOUT_SECS" in
    ''|*[!0-9]*) echo "cell-agent protocol timeout must be from 1 through 30 seconds" >&2; return 1 ;;
  esac
  [ "$AGENT_PROTOCOL_TIMEOUT_SECS" -ge 1 ] && [ "$AGENT_PROTOCOL_TIMEOUT_SECS" -le 30 ] || {
    echo "cell-agent protocol timeout must be from 1 through 30 seconds" >&2
    return 1
  }
  [ -f "$AGENT_BIN" ] && [ ! -L "$AGENT_BIN" ] && [ -x "$AGENT_BIN" ] || {
    echo "missing non-symlink executable agent binary at $AGENT_BIN (run the backend packaging role first)" >&2
    return 1
  }
  [ -f "$AGENT_RELEASE_FACT" ] && [ ! -L "$AGENT_RELEASE_FACT" ] || {
    echo "missing cell-agent release fact at $AGENT_RELEASE_FACT (run the backend packaging role first)" >&2
    return 1
  }

  local fact_values
  fact_values="$(python3 - "$AGENT_BIN" "$AGENT_RELEASE_FACT" <<'PYFACT'
import json, os, stat, sys
agent_path, path = sys.argv[1:]
agent_metadata = os.lstat(agent_path)
if not stat.S_ISREG(agent_metadata.st_mode) or agent_metadata.st_uid != 0 or agent_metadata.st_nlink != 1:
    raise SystemExit('agent must be a root-owned, single-link regular file')
if stat.S_IMODE(agent_metadata.st_mode) != 0o755 or agent_metadata.st_size <= 0:
    raise SystemExit('agent must be a non-empty mode-0755 executable')
metadata = os.lstat(path)
if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != 0 or metadata.st_nlink != 1:
    raise SystemExit('release fact must be a root-owned, single-link regular file')
if stat.S_IMODE(metadata.st_mode) != 0o444 or metadata.st_size <= 0 or metadata.st_size > 4096:
    raise SystemExit('release fact must be mode 0444 and at most 4096 bytes')
with open(path, 'rb') as handle:
    fact = json.load(handle)
keys = {
    'schema', 'rootfs_schema', 'runtime_artifact_protocol',
    'agent_wire_protocol', 'agent_wire_capabilities', 'agent_sha256',
}
if set(fact) != keys:
    raise SystemExit('release fact has unexpected fields')
for key in keys - {'agent_sha256'}:
    if type(fact[key]) is not int:
        raise SystemExit(f'release fact {key} must be an integer')
digest = fact['agent_sha256']
if not isinstance(digest, str) or len(digest) != 64 or any(c not in '0123456789abcdef' for c in digest):
    raise SystemExit('release fact agent digest is not lowercase SHA-256')
print(
    fact['schema'], fact['rootfs_schema'], fact['runtime_artifact_protocol'],
    fact['agent_wire_protocol'], fact['agent_wire_capabilities'], digest,
)
PYFACT
  )" || {
    echo "cell-agent release fact is not a parseable supported schema-2 fact" >&2
    return 1
  }

  local fact_schema fact_rootfs_schema fact_protocol fact_wire fact_capabilities fact_sha256
  read -r fact_schema fact_rootfs_schema fact_protocol fact_wire fact_capabilities fact_sha256 <<<"$fact_values"
  [ "$fact_schema" = "$CELL_AGENT_RELEASE_SCHEMA_EXPECTED" ] || {
    echo "cell-agent release schema $fact_schema does not match required $CELL_AGENT_RELEASE_SCHEMA_EXPECTED" >&2
    return 1
  }
  [ "$fact_rootfs_schema" = "$ROOTFS_SCHEMA_EXPECTED" ] || {
    echo "cell-agent rootfs schema $fact_rootfs_schema does not match required $ROOTFS_SCHEMA_EXPECTED" >&2
    return 1
  }
  [ "$fact_protocol" = "$RUNTIME_ARTIFACT_PROTOCOL_EXPECTED" ] || {
    echo "cell-agent runtime-artifact protocol $fact_protocol does not match required $RUNTIME_ARTIFACT_PROTOCOL_EXPECTED" >&2
    return 1
  }
  [ "$fact_wire" = "$AGENT_WIRE_PROTOCOL_EXPECTED" ] || {
    echo "cell-agent wire protocol $fact_wire does not match required $AGENT_WIRE_PROTOCOL_EXPECTED" >&2
    return 1
  }
  [ "$fact_capabilities" = "$AGENT_WIRE_CAPABILITIES_EXPECTED" ] || {
    echo "cell-agent capabilities $fact_capabilities do not match required $AGENT_WIRE_CAPABILITIES_EXPECTED" >&2
    return 1
  }
  local actual_sha256
  actual_sha256="$(sha256sum "$AGENT_BIN" | cut -d' ' -f1)"
  [ "$actual_sha256" = "$fact_sha256" ] || {
    echo "cell-agent bytes do not match release fact (actual $actual_sha256, fact $fact_sha256)" >&2
    return 1
  }

  local probe_json probe_values probe_rc
  set +e
  probe_json="$(timeout --signal=TERM --kill-after=1s \
    "${AGENT_PROTOCOL_TIMEOUT_SECS}s" "$AGENT_BIN" --agent-protocol-fact 2>/dev/null)"
  probe_rc=$?
  set -e
  [ "$probe_rc" -eq 0 ] || {
    echo "bounded cell-agent --agent-protocol-fact probe failed (rc=$probe_rc, timeout=${AGENT_PROTOCOL_TIMEOUT_SECS}s); refusing before rootfs or service mutation" >&2
    return 1
  }
  probe_values="$(python3 - "$probe_json" <<'PYPROBE'
import json, sys
try:
    fact = json.loads(sys.argv[1])
except Exception as error:
    raise SystemExit(f'agent protocol fact is invalid JSON: {error}')
keys = {
    'rootfs_schema', 'runtime_artifact_protocol',
    'agent_wire_protocol', 'agent_wire_capabilities',
}
if set(fact) != keys or any(type(fact[key]) is not int for key in keys):
    raise SystemExit('agent protocol fact has missing, extra, or non-integer fields')
print(
    fact['rootfs_schema'], fact['runtime_artifact_protocol'],
    fact['agent_wire_protocol'], fact['agent_wire_capabilities'],
)
PYPROBE
  )" || {
    echo "cell-agent --agent-protocol-fact did not return the exact supported fact" >&2
    return 1
  }
  local probe_rootfs probe_protocol probe_wire probe_capabilities
  read -r probe_rootfs probe_protocol probe_wire probe_capabilities <<<"$probe_values"
  [ "$probe_rootfs $probe_protocol $probe_wire $probe_capabilities" = \
    "$fact_rootfs_schema $fact_protocol $fact_wire $fact_capabilities" ] || {
    echo "cell-agent protocol probe disagrees with its release fact" >&2
    return 1
  }

  ROOTFS_SCHEMA="$probe_rootfs"
  AGENT_PROTOCOL="$probe_protocol"
  AGENT_WIRE_PROTOCOL="$probe_wire"
  AGENT_WIRE_CAPABILITIES="$probe_capabilities"
  AGENT_SHA256="$actual_sha256"
  printf '{"schema":%s,"rootfs_schema":%s,"runtime_artifact_protocol":%s,"agent_wire_protocol":%s,"agent_wire_capabilities":%s,"agent_sha256":"%s"}\n' \
    "$fact_schema" "$ROOTFS_SCHEMA" "$AGENT_PROTOCOL" "$AGENT_WIRE_PROTOCOL" \
    "$AGENT_WIRE_CAPABILITIES" "$AGENT_SHA256"
}

if [ "${1:-}" = "--preflight-agent" ]; then
  [ "$#" -eq 1 ] || {
    echo "--preflight-agent accepts no positional arguments" >&2
    exit 2
  }
  preflight_agent
  exit
fi

OCI_IMAGE="${1:?usage: build-rootfs.sh <oci-image> <logical-name> [size-mib]}"
LOGICAL_NAME="${2:?missing logical name}"
SIZE_MIB="${3:-2048}"
[ "$#" -le 3 ] || { echo "too many arguments" >&2; exit 2; }

ROOTFS_DIR="${ROOTFS_DIR:-/var/lib/hive/rootfs}"
CONTAINER_CLI="${CONTAINER_CLI:-docker}"
WASMER_TARBALL="${WASMER_TARBALL:-}"
BUN_TARBALL="${BUN_TARBALL:-}"
BUN_BINARY="${BUN_BINARY:-}"

# Prove the exact installed release BEFORE creating a container, rootfs lock,
# image, or mount. Then own the installer's release lock and re-prove under that
# serialization for the entire bake: the installed fact cannot rotate between
# the snapshot and atomic rootfs publication.
preflight_agent >/dev/null
exec 8>"${AGENT_BIN}.install.lock"
flock -n 8 || {
  echo "cell-agent packaging owns ${AGENT_BIN}.install.lock; refusing a split release/rootfs bake" >&2
  exit 1
}
preflight_agent >/dev/null
PACKAGED_RELEASE_FACT="$(printf '{"schema":%s,"rootfs_schema":%s,"runtime_artifact_protocol":%s,"agent_wire_protocol":%s,"agent_wire_capabilities":%s,"agent_sha256":"%s"}' \
  "$CELL_AGENT_RELEASE_SCHEMA_EXPECTED" "$ROOTFS_SCHEMA" "$AGENT_PROTOCOL" \
  "$AGENT_WIRE_PROTOCOL" "$AGENT_WIRE_CAPABILITIES" "$AGENT_SHA256")"

# Snapshot the exact proved inode before doing slow image work. Backend packaging
# publishes the host agent by atomic rename, so a concurrent release could
# otherwise make the later install read different bytes than AGENT_SHA256 names.
# Re-hashing the private copy closes that path without holding the packaging lock
# for the whole rootfs build.
AGENT_SNAPSHOT="$(mktemp)"
trap 'rm -f "$AGENT_SNAPSHOT"' EXIT
install -m 0755 "$AGENT_BIN" "$AGENT_SNAPSHOT"
SNAPSHOT_SHA256="$(sha256sum "$AGENT_SNAPSHOT" | cut -d' ' -f1)"
[ "$SNAPSHOT_SHA256" = "$AGENT_SHA256" ] || {
  echo "cell-agent changed while its verified snapshot was being captured; refusing before rootfs mutation" >&2
  exit 1
}

# Sanitize the logical name the same way the Rust backend does (`/`,`:` -> `_`).
SAFE_NAME="$(printf '%s' "$LOGICAL_NAME" | tr -c 'A-Za-z0-9.-' '_')"
OUT="${ROOTFS_DIR}/${SAFE_NAME}.ext4"
ROOTFS_PROTOCOL_MARKER="/etc/hive/runtime-artifact-protocol.json"
ROOTFS_PROTOCOL_SIDECAR="${OUT}.runtime-artifact-protocol.json"
mkdir -p "$ROOTFS_DIR"
exec 9>"${OUT}.build.lock"
flock -n 9 || {
  echo "another rootfs writer already owns ${OUT}.build.lock; refusing a concurrent rebuild"
  exit 1
}

command -v "$CONTAINER_CLI" >/dev/null 2>&1 || {
  echo "missing container CLI: $CONTAINER_CLI"
  exit 1
}

echo ">> exporting $OCI_IMAGE filesystem"
CID="$("$CONTAINER_CLI" create "$OCI_IMAGE" /bin/true)"
trap '"$CONTAINER_CLI" rm -f "$CID" >/dev/null 2>&1 || true; rm -f "$AGENT_SNAPSHOT"' EXIT

WORK="$(mktemp -d)"
MNT="$WORK/mnt"
mkdir -p "$MNT"

# BUILD ASIDE, THEN RENAME. Never dd/mkfs over $OUT itself: on an already
# provisioned node that path IS the live base image, and every cold start copies
# it (`FirecrackerBackend::provision` -> reflink_or_copy(base, overlay)) with
# only an existence check ahead of the copy — which `dd` satisfies the instant it
# creates the file. Writing in place therefore gave a multi-minute window where
# arriving cold starts copied a zeroed or half-populated filesystem, booted a
# microVM with no /sbin/hive-cell-agent, never reached FunctionReady, and
# surfaced as DEPLOYMENT_START_FAILED — telling the tenant to debug an entrypoint
# that was perfectly correct. Already-RUNNING microVMs were unaffected (they hold
# their own per-cell overlay), so the blast radius was precisely the cold starts
# during the rebuild.
#
# `mv` within one directory is an atomic rename: a cold start either copies the
# whole OLD image or the whole NEW one, never a torn one, and a copy already in
# flight completes against the old inode. This is the same tmp+rename the
# per-deployment path next door already uses (`deliver_build` writes
# `<image>.ext4.tmp` then renames) — the base-image path simply never got it.
OUT_TMP="${OUT}.tmp"
SIDECAR_TMP="${ROOTFS_PROTOCOL_SIDECAR}.tmp.$$"
echo ">> creating ${SIZE_MIB}MiB ext4 at $OUT_TMP (renamed onto $OUT at the end)"
sudo rm -f "$OUT_TMP"
dd if=/dev/zero of="$OUT_TMP" bs=1M count="$SIZE_MIB" status=none
mkfs.ext4 -F -q "$OUT_TMP"
sudo mount -o loop "$OUT_TMP" "$MNT"
# Clean up the PARTIAL image on any failure — a half-built .tmp left behind
# would otherwise be renamed by a later run that assumes it is complete.
trap 'sudo umount "$MNT" 2>/dev/null || true; sudo rm -f "$OUT_TMP" "$SIDECAR_TMP"; "$CONTAINER_CLI" rm -f "$CID" >/dev/null 2>&1 || true; rm -rf "$WORK"; rm -f "$AGENT_SNAPSHOT"' EXIT

echo ">> unpacking container fs into image"
"$CONTAINER_CLI" export "$CID" | sudo tar -x -C "$MNT"

echo ">> installing cell agent as /sbin/hive-cell-agent and build dir"
sudo install -D -m0755 "$AGENT_SNAPSHOT" "$MNT/sbin/hive-cell-agent"
sudo mkdir -p "$MNT/build" "$MNT/root" "$MNT/proc" "$MNT/sys" "$MNT/dev" "$MNT/tmp"

# The marker is INSIDE the exact rootfs image and names the exact agent bytes
# installed above. The host-side sidecar written after unmount binds this marker
# to the whole ext4 image digest, so a sidecar copied beside a different/legacy
# image cannot create a capability.
ROOTFS_MARKER_TMP="$WORK/runtime-artifact-protocol.json"
printf '{"schema":%s,"protocol":%s,"agent_wire_protocol":%s,"agent_wire_capabilities":%s,"agent_sha256":"%s"}\n' \
  "$ROOTFS_SCHEMA" "$AGENT_PROTOCOL" "$AGENT_WIRE_PROTOCOL" \
  "$AGENT_WIRE_CAPABILITIES" "$AGENT_SHA256" >"$ROOTFS_MARKER_TMP"
sudo install -D -o root -g root -m0444 \
  "$ROOTFS_MARKER_TMP" "$MNT$ROOTFS_PROTOCOL_MARKER"

# Make sure git + a shell exist (most base images have sh; add git if missing).
if [ ! -e "$MNT/usr/bin/git" ] && [ ! -e "$MNT/bin/git" ]; then
  echo "   note: '$OCI_IMAGE' has no git; clones will fail. Bake git into your image if needed."
fi

# Wasmer CLI into the GUEST (see the header note for why the host copy cannot
# serve this purpose). /usr/local/bin is on the exact PATH the agent sets.
MARKER="${ROOTFS_DIR}/${SAFE_NAME}.wasmer"
sudo rm -f "$MARKER"
if [ -n "$WASMER_TARBALL" ]; then
  [ -f "$WASMER_TARBALL" ] || { echo "WASMER_TARBALL=$WASMER_TARBALL not found"; exit 1; }
  echo ">> baking wasmer CLI into the guest at /usr/local/bin/wasmer"
  WTMP="$WORK/wasmer"
  mkdir -p "$WTMP"
  # Extract ONLY the CLI: the lib/*.a archives are ~560 MiB of build-time
  # artifacts `wasmer run` never opens, and this image has a fixed size budget.
  tar -xzf "$WASMER_TARBALL" -C "$WTMP" bin/wasmer
  sudo install -D -m0755 "$WTMP/bin/wasmer" "$MNT/usr/local/bin/wasmer"
  # Fail loudly rather than shipping an image that silently cannot run wasm.
  [ -x "$MNT/usr/local/bin/wasmer" ] || { echo "wasmer staging failed"; exit 1; }
  WVER="$("$WTMP/bin/wasmer" --version 2>/dev/null || echo unknown)"
  echo "   staged: $WVER"
fi

# Bun CLI into the GUEST (see the header note for why the host copy cannot
# serve this purpose — identical reasoning to wasmer above). /usr/local/bin is
# on the exact PATH the agent sets.
BUN_MARKER="${ROOTFS_DIR}/${SAFE_NAME}.bun"
sudo rm -f "$BUN_MARKER"
if [ -n "$BUN_TARBALL" ] && [ -n "$BUN_BINARY" ]; then
  echo "set only one of BUN_TARBALL or BUN_BINARY, not both"
  exit 1
fi
BUN_STAGED_BIN=""
if [ -n "$BUN_TARBALL" ]; then
  [ -f "$BUN_TARBALL" ] || { echo "BUN_TARBALL=$BUN_TARBALL not found"; exit 1; }
  echo ">> baking bun CLI into the guest at /usr/local/bin/bun"
  BTMP="$WORK/bun"
  mkdir -p "$BTMP"
  tar -xzf "$BUN_TARBALL" -C "$BTMP" bun
  sudo install -D -m0755 "$BTMP/bun" "$MNT/usr/local/bin/bun"
  BUN_STAGED_BIN="$BTMP/bun"
elif [ -n "$BUN_BINARY" ]; then
  [ -f "$BUN_BINARY" ] || { echo "BUN_BINARY=$BUN_BINARY not found"; exit 1; }
  echo ">> baking bun CLI into the guest at /usr/local/bin/bun (from BUN_BINARY)"
  sudo install -D -m0755 "$BUN_BINARY" "$MNT/usr/local/bin/bun"
  BUN_STAGED_BIN="$BUN_BINARY"
fi
if [ -n "$BUN_STAGED_BIN" ]; then
  # Fail loudly rather than shipping an image that silently cannot run bun.
  [ -x "$MNT/usr/local/bin/bun" ] || { echo "bun staging failed"; exit 1; }
  BVER="$("$BUN_STAGED_BIN" --version 2>/dev/null || echo unknown)"
  echo "   staged: bun $BVER"
fi

sudo umount "$MNT"
IMAGE_SHA256="$(sha256sum "$OUT_TMP" | cut -d' ' -f1)"
IMAGE_BYTES="$(stat -c '%s' "$OUT_TMP")"
[ "${#IMAGE_SHA256}" -eq 64 ] || { echo "rootfs SHA-256 has the wrong length"; exit 1; }
[ "$IMAGE_BYTES" -gt 0 ] || { echo "rootfs image is empty"; exit 1; }

# Prepare the content proof in the SAME directory as the image. Neither name is
# published until both files are complete, fsynced, root-owned and read-only.
# Two names cannot be renamed atomically together, so remove the old proof first:
# every crash window then reads as NOT CAPABLE (old/new image without a proof),
# never as a proof for the wrong image.
SIDECAR_BODY="$WORK/runtime-artifact-rootfs-sidecar.json"
printf '{"schema":%s,"protocol":%s,"agent_wire_protocol":%s,"agent_wire_capabilities":%s,"agent_sha256":"%s","image_sha256":"%s","image_bytes":%s}\n' \
  "$ROOTFS_SCHEMA" "$AGENT_PROTOCOL" "$AGENT_WIRE_PROTOCOL" \
  "$AGENT_WIRE_CAPABILITIES" "$AGENT_SHA256" "$IMAGE_SHA256" "$IMAGE_BYTES" >"$SIDECAR_BODY"
sudo rm -f "$SIDECAR_TMP"
sudo install -o root -g root -m0444 "$SIDECAR_BODY" "$SIDECAR_TMP"
sudo sync -f "$OUT_TMP"
sudo sync -f "$SIDECAR_TMP"

# Re-prove every independently-produced fact before either canonical name moves.
# This second whole-image hash is intentional: the first produces the sidecar;
# this one proves the image remained those exact bytes through marker extraction,
# sidecar construction and fsync. The install lock held on fd 8 simultaneously
# keeps the canonical agent/release pair fixed through publication.
CURRENT_RELEASE_FACT="$(preflight_agent)"
EMBEDDED_MARKER="$(debugfs -R "cat $ROOTFS_PROTOCOL_MARKER" "$OUT_TMP" 2>/dev/null)"
python3 - \
  "$PACKAGED_RELEASE_FACT" "$CURRENT_RELEASE_FACT" "$EMBEDDED_MARKER" \
  "$SIDECAR_TMP" "$OUT_TMP" "$AGENT_SNAPSHOT" \
  "$CELL_AGENT_RELEASE_SCHEMA_EXPECTED" "$ROOTFS_SCHEMA" "$AGENT_PROTOCOL" \
  "$AGENT_WIRE_PROTOCOL" "$AGENT_WIRE_CAPABILITIES" "$AGENT_SHA256" \
  "$IMAGE_SHA256" "$IMAGE_BYTES" <<'PYPUBLISH'
import hashlib, json, os, sys
(
    packaged_json, current_json, marker_json, sidecar_path, image_path,
    agent_path, release_schema, rootfs_schema, runtime_protocol,
    wire_protocol, wire_capabilities, agent_sha, image_sha, image_bytes,
) = sys.argv[1:]

def digest(path):
    value = hashlib.sha256()
    with open(path, 'rb') as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b''):
            value.update(chunk)
    return value.hexdigest()

expected_release = {
    'schema': int(release_schema),
    'rootfs_schema': int(rootfs_schema),
    'runtime_artifact_protocol': int(runtime_protocol),
    'agent_wire_protocol': int(wire_protocol),
    'agent_wire_capabilities': int(wire_capabilities),
    'agent_sha256': agent_sha,
}
expected_marker = {
    'schema': int(rootfs_schema),
    'protocol': int(runtime_protocol),
    'agent_wire_protocol': int(wire_protocol),
    'agent_wire_capabilities': int(wire_capabilities),
    'agent_sha256': agent_sha,
}
expected_sidecar = {
    **expected_marker,
    'image_sha256': image_sha,
    'image_bytes': int(image_bytes),
}
with open(sidecar_path, 'rb') as handle:
    sidecar = json.load(handle)
if json.loads(packaged_json) != expected_release:
    raise SystemExit('captured release fact disagrees with the packaged agent')
if json.loads(current_json) != expected_release:
    raise SystemExit('installed release fact moved before rootfs publication')
if json.loads(marker_json) != expected_marker:
    raise SystemExit('in-rootfs marker does not exactly describe the packaged agent protocols')
if sidecar != expected_sidecar:
    raise SystemExit('rootfs sidecar does not exactly describe the packaged marker and image')
if digest(agent_path) != agent_sha:
    raise SystemExit('private packaged agent snapshot changed before rootfs publication')
if digest(image_path) != image_sha or os.stat(image_path).st_size != int(image_bytes):
    raise SystemExit('rootfs image changed after its publication identity was computed')
PYPUBLISH

# THE ONLY MOMENT THE LIVE BASE CHANGES. A cold start copies either the whole old
# image or the whole new image. The sidecar verifier hashes the exact current
# inode and refuses the short image/sidecar transition window.
sudo rm -f "$ROOTFS_PROTOCOL_SIDECAR"
sudo mv -f "$OUT_TMP" "$OUT"
sudo mv -f "$SIDECAR_TMP" "$ROOTFS_PROTOCOL_SIDECAR"
sudo sync -f "$ROOTFS_DIR"
trap '"$CONTAINER_CLI" rm -f "$CID" >/dev/null 2>&1 || true; rm -rf "$WORK"; rm -f "$AGENT_SNAPSHOT"' EXIT
# Marker LAST — after the clean unmount AND after the image is actually in
# place. `$MARKER` was removed up front, so a build that dies anywhere between
# leaves the node advertising NO wasm capability, which is the safe direction:
# placement simply skips it instead of routing Wasmer work to an image that
# never got the binary.
if [ -n "$WASMER_TARBALL" ]; then
  printf '%s\n' "$WVER" | sudo tee "$MARKER" >/dev/null
  echo ">> wrote capability marker: $MARKER"
fi
# Same "marker LAST" discipline as wasmer above, same reason: `$BUN_MARKER`
# was removed up front, so a build that dies anywhere between leaves the node
# advertising NO bun capability — placement simply skips it instead of
# routing Bun work to an image that never got the binary.
if [ -n "$BUN_STAGED_BIN" ]; then
  printf '%s\n' "$BVER" | sudo tee "$BUN_MARKER" >/dev/null
  echo ">> wrote capability marker: $BUN_MARKER"
fi
echo ">> wrote runtime-artifact rootfs proof: $ROOTFS_PROTOCOL_SIDECAR"
echo ">> done: $OUT"
