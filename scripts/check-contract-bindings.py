#!/usr/bin/env python3
"""CI contract-binding gate.

A binding is NOT a mock and NOT a test. It pins one live assertion (unit,
integration, system, acceptance) to the canonical bytes of the REAL construct
that assertion exercises. When a construct changes, is renamed/migrated or is
removed, --check fails loudly and names the exact regeneration command. --sync
re-pins; --move and --remove make a migration or a deletion an explicit,
recorded decision instead of a silent skip.

Stdlib only. No test files, no fixtures, no mocking libraries: the assertions
themselves stay on the surfaces this repo already runs (cargo test
--workspace --locked, scripts/cli-acceptance.sh, scripts/deploy-e2e.sh).

  python3 -I scripts/check-contract-bindings.py --check
  python3 -I scripts/check-contract-bindings.py --sync
  python3 -I scripts/check-contract-bindings.py --sync --move ID:path:start
  python3 -I scripts/check-contract-bindings.py --remove ID:reason

Canonical bytes are LENGTH-PREFIXED (<len>:<bytes>), so a bare join can never
collide, and SHA-256 is a BINDING here, not a BLAKE3 content identity.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import sys
from pathlib import Path

SELF = Path(__file__).resolve()
ROOT = SELF.parent.parent
MANIFEST = ROOT / "scripts" / "contract-bindings.json"
CI_WORKFLOW = ".github/workflows/rust-ci.yml"
RELEASE_WORKFLOW = ".github/workflows/release-npm-cli.yml"
HARNESS = "scripts/cli-acceptance.sh"
DOMAIN = b"hive-contract-binding-v1\0"
REGENERATE = "python3 -I scripts/check-contract-bindings.py --sync"

# One row per construct a live assertion spends. `start` is a literal substring
# that must appear exactly once; `end` is the exact stripped line that closes
# the span (inclusive). A comment or a reformat inside the span is invisible;
# a changed identifier, literal, route or field is not.
BINDINGS = [
    dict(id="unit.cli.cmd", layer="unit", path="crates/shadw-cli/src/main.rs",
         start="enum Cmd {", end="}", assertion="cargo test -p shadw-cli --locked --verbose",
         why="the CLI subcommand surface the unit and integration tests dispatch on"),
    dict(id="unit.cli.auth", layer="unit", path="crates/shadw-cli/src/main.rs",
         start="enum AuthCmd {", end="}", assertion="auth login saves config + verifies",
         why="auth login/whoami/logout/token variants used by the acceptance harness"),
    dict(id="unit.cli.keys", layer="unit", path="crates/shadw-cli/src/main.rs",
         start="enum KeyCmd {", end="}", assertion="keys create returns id+token",
         why="keys create/list/revoke round-trip"),
    dict(id="unit.cli.obs", layer="unit", path="crates/shadw-cli/src/main.rs",
         start="enum ObsCmd {", end="}", assertion="obs overview",
         why="observability read-only commands"),
    dict(id="unit.cli.projects", layer="unit", path="crates/shadw-cli/src/main.rs",
         start="enum ProjCmd {", end="}", assertion="env list shows the key",
         why="project env set/list/rm and delete"),
    dict(id="unit.cli.domains", layer="unit", path="crates/shadw-cli/src/main.rs",
         start="enum DomCmd {", end="}", assertion="domains record add returns id",
         why="domain record add/list/rm"),
    dict(id="unit.cli.bin", layer="unit", path="crates/shadw-cli/Cargo.toml",
         start='name = "shadw"', end=None, assertion="cargo test -p shadw-cli --locked --verbose",
         why="the shadw binary the harness execs at ./target/debug/shadw"),
    dict(id="unit.node.bin", layer="unit", path="crates/hive-cloud/Cargo.toml",
         start='[[bin]]', end='name = "hive-cloud"', end_match="prefix",
         assertion="cargo test -p hive-cloud --locked --verbose",
         why="the hive-cloud binary the harness boots as a real node"),
    dict(id="unit.roles.rules", layer="unit", path="crates/hive-cloud/src/roles.rs",
         start="const RULES: &[Rule] = &[", end="];",
         assertion="beta BLOCKED from alpha's database (404)",
         why="the RULES matrix that decides every authenticated route"),
    dict(id="ui.roles.mirror", layer="integration", path="ui/lib/team-role.ts",
         start="export const TEAM_ACTIONS = {", end="} as const", end_match="prefix",
         assertion="npm run lint",
         why="the dashboard mirror of roles::RULES; drift opens a control the server refuses"),
    dict(id="integration.store.registry", layer="integration",
         path="crates/hive-cloud/src/store_sync.rs",
         start="pub static REGISTRY: &[SyncedStore] = &[", end="];",
         assertion="cargo test --workspace --locked --verbose",
         why="whole-sale snapshot adoption is gated on this exact store set"),
    dict(id="integration.store.merge", layer="integration",
         path="crates/hive-cloud/src/store_sync.rs",
         start="pub static MERGE_STORES: &[&str] = &[", end="];",
         assertion="cargo test --workspace --locked --verbose",
         why="only these stores may merge per-row from a backup owner"),
    dict(id="integration.browser.policy.rust", layer="integration",
         path="crates/fluid-core/src/lib.rs",
         start="pub fn browser_policy_digest(", end="}",
         assertion="cargo test -p fluid-core --locked",
         why="ONE contract, TWO implementations: the Rust signer"),
    dict(id="integration.browser.policy.js", layer="integration",
         path="crates/hive-browser/www/artifact-policy.js",
         start="export function policyDigest(", end="}",
         assertion="cargo test -p fluid-core --locked",
         why="ONE contract, TWO implementations: the JS policyDigest; drift rejects every handshake"),
    dict(id="integration.browser.mesh.rust", layer="integration",
         path="crates/hive-browser/src/lib.rs",
         start="const MESH_SIG_DOMAIN: &[u8]", end=None,
         assertion="cargo test -p fluid-core --locked",
         why="MESH_SIG_DOMAIN must be byte-identical to peer-mesh.js"),
    dict(id="integration.browser.mesh.js", layer="integration",
         path="crates/hive-browser/www/peer-mesh.js",
         start="export const MESH_SIG_DOMAIN =", end=None,
         assertion="cargo test -p fluid-core --locked",
         why="MESH_SIG_DOMAIN must be byte-identical to the Rust constant"),
    dict(id="integration.landing.cache.proxy", layer="integration", path="ui/proxy.ts",
         start="const LANDING_CACHE =", end=None, assertion="npm run lint",
         why="LANDING_CACHE stays byte-identical in proxy.ts and next.config.mjs"),
    dict(id="integration.landing.cache.next", layer="integration",
         path="ui/next.config.mjs", start="const LANDING_CACHE =", end=None,
         assertion="npm run lint", why="the config copy wins over the catch-all"),
    dict(id="integration.sync.node_worker", layer="integration",
         path="ui/scripts/sync-browser-node.mjs",
         start="const NODE_WORKER_SET =", end=None, assertion="npm run build",
         why="every hand-written module the worker imports at RUNTIME; a missing one 404s on the fleet"),
    dict(id="integration.sync.sqlite", layer="integration",
         path="ui/scripts/sync-browser-node.mjs",
         start="const SQLITE_SET = [", end="];", assertion="npm run build",
         why="the browser-db lane's runtime module set"),
    dict(id="integration.sync.musql", layer="integration",
         path="ui/scripts/sync-browser-node.mjs",
         start="const MUSQL_SET = [", end="];", assertion="npm run build",
         why="the musql browser lane's runtime module set"),
    dict(id="system.harness", layer="system", path=HARNESS,
         extractor="harness_assertions", assertion="./scripts/cli-acceptance.sh",
         why="the REAL node + REAL CLI system/acceptance assertion inventory"),
    dict(id="acceptance.harness.paths", layer="acceptance", path=HARNESS,
         extractor="harness_paths", assertion="./scripts/cli-acceptance.sh",
         why="the admin API paths and ports the acceptance run spends"),
    dict(id="system.harness.macos.temp", layer="system", path=HARNESS,
         start="Darwin) TMP_ROOT=/private/tmp", end=None,
         assertion="./scripts/cli-acceptance.sh",
         why="the /private/tmp temp root: measured 35/0 there vs 31/4 under the macOS default"),
    dict(id="system.harness.platform.probe", layer="system", path=HARNESS,
         start="if [ -n \"${HIVE_CI_PLATFORM:-}\" ]", end=None,
         assertion="./scripts/cli-acceptance.sh",
         why="CI's declared platform must agree with uname's probe; a mislabeled runner cannot hide a missing Mac leg"),
]

# Blocking commands, one per layer. A layer with no blocking command in
# rust-ci.yml is a layer CI does not actually assert.
GATES = [
    dict(id="unit", command="cargo test --workspace --locked --verbose"),
    dict(id="integration", command="cargo test -p shadw-cli --locked --verbose"),
    dict(id="system", command="cargo test -p fluid-core --locked"),
    dict(id="acceptance", command="./scripts/cli-acceptance.sh"),
    dict(id="dashboard.types", command="npx tsc --noEmit -p tsconfig.ci.json"),
    dict(id="dashboard.lint", command="npm run lint"),
    dict(id="dashboard.build", command="npm run build"),
    dict(id="dashboard.install", command="npm ci"),
    dict(id="whitespace", command="git diff --check"),
    dict(id="bindings", command="python3 -I scripts/check-contract-bindings.py --check"),
    dict(id="acceptance.count", command="grep -qE '35 passed, 0 failed'"),
]

# Every blocking layer must run on BOTH platforms. A removed macOS leg is a
# failed build, not a silently narrower pipeline. `macos-15` is arm64 and
# `macos-15-intel` is x64; the retired macos-13/macos-14 labels are refused.
REQUIRED_MACOS_JOBS = ["contracts", "test", "dashboard", "core-tests", "cli", "verify"]
MACOS_LABELS = ["macos-15", "macos-15-intel"]
RETIRED_MACOS_LABELS = ["macos-13", "macos-14"]

# The ONLY steps allowed to stay non-blocking. Each needs a reason. Adding a
# new fail-open step without an entry here is refused; removing the step while
# leaving the entry is refused too. This is a ratchet, not a snapshot.
FAIL_OPEN = [
    dict(step="rustfmt (check)", reason="pre-existing cargo-fmt drift across the workspace"),
    dict(step="clippy", reason="pre-existing workspace lint nits outside shadw-cli"),
    dict(step="cargo audit (Rust dependencies)",
         reason="known advisory backlog in the pinned Cargo.lock; see CHANGELOG.md"),
    dict(step="npm audit (dashboard)",
         reason="known advisory backlog in the pinned ui/package-lock.json; see CHANGELOG.md"),
]
FAIL_OPEN_NAMES = {r["step"] for r in FAIL_OPEN}

FORBIDDEN = [
    ("npm ci || npm install", "a broken lockfile must fail, never fall back to a floating install"),
    ("|| true", "a swallowed failure is not an assertion"),
    ("set +e", "a swallowed exit status is not an assertion"),
    ("--sync", "CI must never regenerate the manifest it is auditing; that would launder every drift"),
]


def lp(part: bytes) -> bytes:
    return str(len(part)).encode() + b":" + part


def digest(parts: list[bytes]) -> str:
    h = hashlib.sha256()
    h.update(lp(DOMAIN))
    for p in parts:
        h.update(lp(p))
    return h.hexdigest()


def strip_inline_comment(line: str) -> str:
    """Drop // comments outside quotes; a URL inside a string literal stays."""
    out, quote, i = [], None, 0
    while i < len(line):
        c = line[i]
        if quote:
            out.append(c)
            if c == "\\" and i + 1 < len(line):
                out.append(line[i + 1])
                i += 2
                continue
            if c == quote:
                quote = None
            i += 1
            continue
        if c in ('"', "'", "`"):
            quote = c
            out.append(c)
            i += 1
            continue
        if c == "/" and i + 1 < len(line) and line[i + 1] == "/":
            break
        out.append(c)
        i += 1
    return "".join(out)


def canonical(lines: list[str]) -> str:
    kept = []
    for raw in lines:
        s = strip_inline_comment(raw.rstrip()).strip()
        if not s:
            continue
        if s.startswith(("//", "/*", "*", "#")) and not s.startswith("#[") and not s.startswith("#!"):
            continue
        kept.append(re.sub(r"\s+", " ", s))
    return "\n".join(kept)


def span_bytes(rel: str, start: str, end: str | None, end_match: str = "exact", limit: int = 500):
    path = ROOT / rel
    if not path.is_file():
        return None, "MISSING", f"path={rel} no longer exists; a construct was removed"
    lines = path.read_text(errors="replace").splitlines()
    hits = [i for i, l in enumerate(lines) if start in l]
    if len(hits) != 1:
        return None, "MOVED", (
            f"path={rel} start={start!r} matched {len(hits)} lines; "
            f"the construct was renamed, migrated or duplicated"
        )
    i = hits[0]
    if end is None:
        selected = [lines[i]]
    else:
        j = None
        for k in range(i, min(i + limit, len(lines))):
            stripped = lines[k].strip()
            matched = (stripped == end) if end_match == "exact" else stripped.startswith(end)
            if matched:
                j = k
                break
        if j is None:
            return None, "TRUNCATED", (
                f"path={rel} end={end!r} not found within {limit} lines of the start"
            )
        selected = lines[i:j + 1]
    body = canonical(selected)
    if not body:
        return None, "UNBOUND", f"path={rel} canonicalized to an empty span"
    return body.encode(), None, None


def harness_assertions():
    lines = (ROOT / HARNESS).read_text(errors="replace").splitlines()
    found = []
    for l in lines:
        if re.search(r"\b(assert|assert_out|assert_fail)\s", l) or re.search(r"\b(ok|bad)\s+\"", l):
            found.extend(re.findall(r'"([^"]{4,})"', l))
    return ("\n".join(sorted(found))).encode()


def harness_paths():
    lines = (ROOT / HARNESS).read_text(errors="replace").splitlines()
    found = set()
    for l in lines:
        found.update(re.findall(r"(/v1/[A-Za-z0-9_/$?=&.-]+|/deployments|/healthz|/auth)", l))
    return ("\n".join(sorted(found))).encode()


def effective_bindings(doc: dict) -> list[dict]:
    """Apply recorded migrations/removals before computing anything.

    A construct that moved is followed to its new path/anchor; a construct
    that was deliberately removed is no longer expected. Both decisions live
    in the committed manifest, so they are reviewable and never silent.
    """
    overrides = doc.get("overrides", {})
    removed = set(doc.get("removed", []))
    out = []
    for b in BINDINGS:
        if b["id"] in removed:
            continue
        row = dict(b)
        row.update(overrides.get(b["id"], {}))
        out.append(row)
    return out


def compute(binding: dict):
    if binding.get("extractor") == "harness_assertions":
        body = harness_assertions()
        return body, None, None
    if binding.get("extractor") == "harness_paths":
        body = harness_paths()
        return body, None, None
    return span_bytes(binding["path"], binding["start"], binding.get("end"),
                      binding.get("end_match", "exact"))


def check_bindings(doc: dict, errors: list[str]) -> int:
    pinned = {r["id"]: r for r in doc.get("bindings", [])}
    bindings = effective_bindings(doc)
    live = {b["id"] for b in bindings}
    removed = set(doc.get("removed", []))
    overrides = doc.get("overrides", {})
    seen = 0
    for b in bindings:
        body, cls, why = compute(b)
        if cls:
            errors.append(
                f"CONTRACT_DRIFT {b['id']} {cls} {why} REGENERATE: {REGENERATE}"
            )
            continue
        want = digest([b["id"].encode(), b["path"].encode(), body])
        row = pinned.get(b["id"])
        if row is None:
            errors.append(
                f"CONTRACT_DRIFT {b['id']} UNBOUND no pinned binding for this live "
                f"assertion REGENERATE: {REGENERATE}"
            )
            continue
        if row.get("path") != b["path"]:
            errors.append(
                f"CONTRACT_DRIFT {b['id']} STALE_PATH pinned={row.get('path')!r} "
                f"live={b['path']!r}; a --move was recorded but --sync was never run "
                f"REGENERATE: {REGENERATE}"
            )
            continue
        if row.get("canonical_sha256") != want:
            errors.append(
                f"CONTRACT_DRIFT {b['id']} CHANGED expected={row.get('canonical_sha256')} "
                f"actual={want} assertion={b['assertion']!r} REGENERATE: {REGENERATE}"
            )
            continue
        seen += 1
    for orphan in sorted(set(pinned) - live - removed):
        errors.append(
            f"CONTRACT_DRIFT {orphan} ORPHAN pinned binding has no live construct; "
            f"record the deletion: python3 -I scripts/check-contract-bindings.py "
            f"--sync --remove {orphan}:reason"
        )
    for oid in sorted(set(overrides) - live):
        errors.append(
            f"CONTRACT_DRIFT {oid} STALE_OVERRIDE override names no live binding; "
            f"REGENERATE: {REGENERATE}"
        )
    for rid in sorted(removed & live):
        errors.append(
            f"CONTRACT_DRIFT {rid} STALE_REMOVAL still a live construct; "
            f"REGENERATE: {REGENERATE}"
        )
    return seen


def workflow_steps(text: str) -> list[dict]:
    steps, cur = [], None
    for line in text.splitlines():
        s = line.strip()
        if re.match(r"^-\s+(name|uses):", s):
            if cur:
                steps.append(cur)
            name = ""
            m = re.match(r"^-\s+name:\s*(.+)$", s)
            if m:
                name = m.group(1).strip().strip("'\"")
            cur = dict(name=name, lines=[], fail_open=False)
        elif cur is not None:
            cur["lines"].append(s)
            if s == "continue-on-error: true":
                cur["fail_open"] = True
    if cur:
        steps.append(cur)
    return steps


def check_workflows(errors: list[str]) -> int:
    ci = (ROOT / CI_WORKFLOW).read_text(errors="replace")
    release = (ROOT / RELEASE_WORKFLOW).read_text(errors="replace")
    steps = workflow_steps(ci)
    body = "\n".join(s for st in steps for s in st["lines"])
    checked = 0

    for g in GATES:
        if g["command"] not in body:
            errors.append(
                f"CI_GATE {g['id']} MISSING blocking command {g['command']!r} "
                f"is absent from {CI_WORKFLOW}"
            )
        else:
            checked += 1

    open_names = {st["name"] for st in steps if st["fail_open"]}
    for n in sorted(open_names - FAIL_OPEN_NAMES):
        errors.append(
            f"CI_GATE fail-open UNDECLARED step {n!r} is continue-on-error but not in "
            f"contract-bindings.json fail_open; either make it blocking or record a reason"
        )
    for n in sorted(FAIL_OPEN_NAMES - open_names):
        errors.append(
            f"CI_GATE fail-open STALE {n!r} is allowed in contract-bindings.json but "
            f"no longer a continue-on-error step; tighten the allowlist via --sync"
        )
    for r in FAIL_OPEN:
        if not r.get("reason"):
            errors.append(f"CI_GATE fail-open {r['step']!r} has no recorded reason")
        else:
            checked += 1

    # Only executable lines. A comment that documents a forbidden pattern is
    # not an invocation of it.
    executable = "\n".join(
        s for s in ci.splitlines() + release.splitlines()
        if not s.lstrip().startswith("#")
    )
    for bad, why in FORBIDDEN:
        if bad in executable:
            errors.append(f"CI_GATE FAIL_OPEN {bad!r} in a workflow: {why}")
        else:
            checked += 1

    if "permissions:" not in ci:
        errors.append(f"CI_GATE permissions MISSING {CI_WORKFLOW} has no least-privilege block")
    else:
        checked += 1
    # Only the `jobs:` block: the `on:` block also has 2-space keys without values.
    jobs_block = ci.split("\njobs:\n", 1)[1] if "\njobs:\n" in ci else ""
    jobs = len(re.findall(r"^  [a-z][a-z0-9_-]*:$", jobs_block, re.M))
    timeouts = len(re.findall(r"^    timeout-minutes:", jobs_block, re.M))
    if timeouts < jobs:
        errors.append(
            f"CI_GATE timeout MISSING {jobs} jobs but only {timeouts} timeout-minutes; "
            f"a wedged run would hang for GitHub's 360-minute default"
        )
    else:
        checked += 1

    if "if-no-files-found: error" not in release:
        errors.append(
            f"CI_GATE release-artifact MISSING {RELEASE_WORKFLOW} uploads without "
            f"if-no-files-found: error, so a missing platform package publishes a subset"
        )
    else:
        checked += 1
    if "cargo test --workspace --locked" not in release:
        errors.append(
            f"CI_GATE release-verify MISSING {RELEASE_WORKFLOW} publishes on a tag "
            f"with no Rust/UI test gate"
        )
    else:
        checked += 1

    # macOS coverage is a gate, not a nice-to-have. Parse each job's own block
    # so a macOS runner added to an unrelated job cannot satisfy a missing leg,
    # and a retired label cannot masquerade as a live runner.
    def job_slices(text: str) -> dict[str, str]:
        block = text.split("\njobs:\n", 1)[1] if "\njobs:\n" in text else ""
        heads = list(re.finditer(r"^  ([a-z][a-z0-9_-]*):$", block, re.M))
        return {
            m.group(1): block[m.start():heads[i + 1].start() if i + 1 < len(heads) else len(block)]
            for i, m in enumerate(heads)
        }

    ci_jobs = job_slices(ci)
    release_jobs = job_slices(release)
    for job in REQUIRED_MACOS_JOBS:
        owner = release if job == "verify" else ci
        slices = release_jobs if job == "verify" else ci_jobs
        slice_text = slices.get(job, "")
        if "macos-15" not in slice_text:
            errors.append(
                f"CI_GATE MACOS {job} MISSING no macOS runner in the {owner} "
                f"{job!r} job; all four test layers must gate both platforms"
            )
        else:
            checked += 1
    for label in MACOS_LABELS:
        if label not in ci or label not in release:
            errors.append(
                f"CI_GATE MACOS label {label!r} absent from one of the workflows; "
                f"both arm64 and x64 darwin must be covered"
            )
        else:
            checked += 1
    for label in RETIRED_MACOS_LABELS:
        if label in executable:
            errors.append(
                f"CI_GATE MACOS retired runner {label!r} (retired 2026-11-02); "
                f"this matrix leg cannot boot"
            )
        else:
            checked += 1
    return checked


def check_secrets(errors: list[str]) -> int:
    """PEM bodies and assigned fleet-secret literals, not bare mentions."""
    pem = re.compile(r"^-----BEGIN (?:[A-Z ]* )?PRIVATE KEY-----\s*$")
    assigned = re.compile(
        r"^(?:HIVE_(?:JWT_SECRET|SECRET_KEY|INTERNAL_TOKEN|QUEUE_TOKEN)|"
        r"GITHUB_APP_PRIVATE_KEY|COMPOSIO_API_KEY|STRIPE_SECRET_KEY)\s*=\s*"
        r"['\"]?[A-Za-z0-9+/=_-]{24,}['\"]?\s*$"
    )
    skip = (".lock", ".md", "/vendor/", "/backups/", "/examples/", "/node_modules/")
    found = []
    # git's own view of the worktree: only files that could actually be committed.
    # Never walk target/ (70G), ui/.next or vendor/ (63G) — that is a stall, not a scan.
    tracked = subprocess.run(
        ["git", "ls-files", "-z"], cwd=ROOT, capture_output=True, check=True
    ).stdout.decode(errors="replace").split("\0")
    for rel in sorted(tracked):
        if not rel or any(s in rel for s in skip):
            continue
        path = ROOT / rel
        if not path.is_file() or path.stat().st_size > 4_000_000:
            continue
        try:
            lines = path.read_text(errors="replace").splitlines()
        except OSError:
            continue
        for i, l in enumerate(lines, 1):
            s = l.strip()
            if pem.match(s) or assigned.match(s):
                found.append(f"{rel}:{i}")
    if found:
        errors.append(
            "CI_GATE COMMITTED_SECRET possible private key material or a literal "
            "fleet secret at " + ", ".join(found[:12])
        )
        return 0
    return 1


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--check", action="store_true")
    ap.add_argument("--sync", action="store_true")
    ap.add_argument("--move", action="append", default=[],
                    help="ID:path:start — rebind a migrated construct, recording supersedes")
    ap.add_argument("--remove", action="append", default=[],
                    help="ID:reason — retire a deleted construct, recording the reason")
    args = ap.parse_args()
    if args.check and args.sync:
        print("choose one of --check or --sync", file=sys.stderr)
        return 2

    if args.move or args.remove:
        if not args.sync:
            print("--move/--remove are --sync operations; pass --sync too", file=sys.stderr)
            return 2

    if args.sync:
        doc = json.loads(MANIFEST.read_text()) if MANIFEST.exists() else {}
        overrides = doc.get("overrides", {})
        removed = set(doc.get("removed", []))
        history = doc.get("history", [])
        for m in args.move:
            try:
                bid, path, start = m.split(":", 2)
            except ValueError:
                print(f"bad --move {m!r}; want ID:path:start", file=sys.stderr)
                return 2
            if bid not in {b["id"] for b in BINDINGS}:
                print(f"bad --move {m!r}; unknown binding id", file=sys.stderr)
                return 2
            overrides[bid] = dict(overrides.get(bid, {}), path=path, start=start)
            removed.discard(bid)
            history.append(dict(id=bid, action="move", to=path, start=start))
        pinned_ids = {
            r["id"] for r in json.loads(MANIFEST.read_text()).get("bindings", [])
        } if MANIFEST.exists() else set()
        known = {b["id"] for b in BINDINGS} | pinned_ids | removed | set(overrides)
        for r in args.remove:
            try:
                bid, reason = r.split(":", 1)
            except ValueError:
                print(f"bad --remove {r!r}; want ID:reason", file=sys.stderr)
                return 2
            if bid not in known or not reason:
                print(f"bad --remove {r!r}; unknown id or empty reason", file=sys.stderr)
                return 2
            removed.add(bid)
            overrides.pop(bid, None)
            history.append(dict(id=bid, action="remove", reason=reason))
        doc["overrides"] = overrides
        doc["removed"] = sorted(removed)
        doc["history"] = history

        errs, rows = [], []
        for b in effective_bindings(doc):
            body, cls, why = compute(b)
            if cls:
                errs.append(f"{b['id']} {cls}: {why}")
                continue
            rows.append(dict(
                id=b["id"], layer=b["layer"], path=b["path"], assertion=b["assertion"],
                canonical_sha256=digest([b["id"].encode(), b["path"].encode(), body]),
            ))
        if errs:
            for e in errs:
                print(f"CONTRACT_DRIFT {e}", file=sys.stderr)
            print(
                "REFUSED: --sync will not silently launder a MISSING/MOVED construct. "
                "Pass --sync --move ID:path:start or --sync --remove ID:reason first.",
                file=sys.stderr,
            )
            return 1
        open_names = {
            st["name"] for st in workflow_steps((ROOT / CI_WORKFLOW).read_text())
            if st["fail_open"]
        }
        allowed = [r for r in FAIL_OPEN if r["step"] in open_names]
        doc["schema"] = 1
        doc["generator"] = REGENERATE
        doc["domain"] = DOMAIN.decode().rstrip("\0")
        doc["gate_sha256"] = hashlib.sha256(lp(DOMAIN) + lp(SELF.read_bytes())).hexdigest()
        doc["bindings"] = rows
        doc["gates"] = GATES
        doc["fail_open"] = allowed
        MANIFEST.write_text(json.dumps(doc, indent=2, sort_keys=True) + "\n")
        print(f"SYNCED {len(rows)} bindings, {len(allowed)} declared fail-open steps, "
              f"{len(overrides)} moves, {len(removed)} removals, {len(history)} history rows")
        return 0

    if not MANIFEST.is_file():
        print(f"CONTRACT_DRIFT manifest MISSING {MANIFEST} REGENERATE: {REGENERATE}",
              file=sys.stderr)
        return 1
    doc = json.loads(MANIFEST.read_text())
    errors = []
    n = check_bindings(doc, errors)
    seen_bindings = n
    gates = check_workflows(errors)
    secrets = check_secrets(errors)
    if doc.get("domain") != DOMAIN.decode().rstrip("\0"):
        errors.append("CI_GATE domain MISSING manifest domain does not match the checker")
    # The gate binds itself: weakening an assertion, dropping a GATES row or
    # deleting a binding definition cannot be done without a manifest diff.
    self_want = hashlib.sha256(lp(DOMAIN) + lp(SELF.read_bytes())).hexdigest()
    if doc.get("gate_sha256") != self_want:
        errors.append(
            "CI_GATE GATE_CHANGED the checker itself was edited without a manifest "
            f"diff (expected={doc.get('gate_sha256')} actual={self_want}); "
            f"REGENERATE: {REGENERATE}"
        )
    if errors:
        for e in errors:
            print(e, file=sys.stderr)
        print(f"RESULT FAIL bindings_ok={seen_bindings}/{len(BINDINGS)} "
              f"gates_ok={gates} secrets_clean={secrets} errors={len(errors)}",
              file=sys.stderr)
        return 1
    print(f"RESULT PASS bindings={seen_bindings} layers=4 gates={gates} "
          f"fail_open={len(doc.get('fail_open', []))} secrets_clean={secrets} "
          f"manifest={MANIFEST.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
