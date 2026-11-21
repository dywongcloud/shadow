#!/usr/bin/env python3 -I
"""Generate and apply a TTL-bounded nft deny set from Project Honey Pot snapshots.

NO FETCHING. This program never requests projecthoneypot.org. The operator
places the three human-visited pages in --snapshot-dir and runs `generate`.
No fleet node, request path or boot path ever makes an outbound request to a
third-party reputation site. The only network use is reverse DNS through the
operator's own resolver, to classify shared cloud/CDN egress; --no-ptr skips
it and falls back to the reviewed exact exclusion list.

Why a separate nft table and not BLOCKED_CIDRS in scripts/hive-lockdown.sh:
  * The three /list_of_ips.php pages are "Top 25 ... By Last Bad Event"
    leaderboards, not a blocklist. They rotate continuously (the RSS
    lastBuildDate is a timestamp of the current day), so a committed snapshot
    goes stale within hours and permanently denies addresses that are later
    reassigned. Every element therefore carries an nft `timeout`.
  * scripts/hive-lockdown.sh is the SOLE host firewall and is regenerated at
    boot. Putting a rotating third-party list in it would make the deny a
    permanent part of the boot path. This table is added and removed with one
    command and never touches that script.
  * This table's DROP runs at priority -105, BEFORE hive_lockdown (-100),
    the strict table (-90) and fail2ban (-1). A DROP verdict is terminal, so
    the denied source cannot be rescued by a later chain. Putting the deny
    after hive_lockdown would depend on its `accept` NOT being terminal,
    which is exactly the trap AGENTS.md records for priority -95 accept
    tables. The deny is a separate table so it can be removed in one command
    without touching scripts/hive-lockdown.sh, the sole host firewall.

Fail-closed guards: an empty or unparsable snapshot is refused; any non-global
address in the payload is refused; any address that is a fleet peer, private
VPC peer or an already-blocked operator address is refused; the set must stay
between 1 and --max-ips elements; and IP-shaped data from RSS feeds that is
not on the three reviewed pages is excluded unless --include-unreviewed.
`apply` runs `nft -c -f` (check mode) before loading anything, re-creates the
table so a re-run is idempotent, lists the set back, and requires an explicit
--operator-confirm.

Usage:
  scripts/honeypot-blocklist.py generate --snapshot-dir DIR [--report FILE]
  scripts/honeypot-blocklist.py apply --snapshot-dir DIR --operator-confirm
  scripts/honeypot-blocklist.py status
  scripts/honeypot-blocklist.py remove --operator-confirm
"""
from __future__ import annotations

import argparse
import concurrent.futures
import ipaddress
import json
import os
import pathlib
import re
import shutil
import socket
import subprocess
import sys
import tempfile

TABLE = "inet honeypot_deny"
SET = "blocked_v4"
DEFAULT_TTL_DAYS = 7
DEFAULT_MAX_IPS = 200

# NO COPY of a third-party IP list is committed here. The site's Terms of
# Website Use prohibit reproduction of its content ("Reproduction of the
# content of the Website without the written permission of Unspam is
# prohibited") and restrict Non-Human Visitors. The operator supplies the
# three human-visited pages at run time; this generator only classifies the
# addresses against generic, publicly known shared-infrastructure networks
# and reverse-DNS markers. Nothing from the source pages is stored in git.
#
# Reviewed 2026-11-20: of the 42 unique addresses then on the three pages,
# 19 were shared or ephemeral egress (15 GCP ephemeral VMs, Google/Googlebot,
# Cloudflare CDN, a Tor exit, shared mail relays). A flat deny of those breaks
# legitimate traffic and bans addresses that are reassigned. These networks
# are infrastructure facts, not a reproduction of the source list.
SHARED_NETWORKS = tuple(ipaddress.ip_network(n) for n in (
    "104.16.0.0/12",   # Cloudflare shared CDN egress
    "172.253.0.0/16",  # Google / Googlebot (1e100.net)
    "23.129.64.0/24",  # shared Tor exit range
))
# Deliberately NO whole 34/8 or 35/8 exclusion: those /8s contain far more
# than GCP, and a blanket carve-out would silently spare every host in them.
# Ephemeral GCP VMs are identified by their `bc.googleusercontent.com` PTR
# below. --no-ptr skips that classification and therefore admits MORE
# addresses to the deny set; it is an explicit operator choice, not a default.

# Reverse-DNS markers for shared infrastructure. These catch NEW addresses on
# a rotated feed without hardcoding a third-party IP list into the repo. A PTR
# is only ever used to EXCLUDE, never to admit a new address to the deny set.
SHARED_PTR_MARKERS = (
    "googleusercontent.com", "1e100.net", "googlebot.com", "google.com",
    "cloudflare.com", "cloudflare", "emeraldonion.net", "tor-exit",
    "amazonaws.com", "azure.com", "microsoft.com", "sparkpostmail.com",
    "docomo.ne.jp",
)


def shared_reason(addr: str, ptr: str) -> str | None:
    ip = ipaddress.IPv4Address(addr)
    for net in SHARED_NETWORKS:
        if ip in net:
            return f"shared CDN/search/exit infrastructure in {net}"
    low = ptr.lower()
    for marker in SHARED_PTR_MARKERS:
        if marker in low:
            return f"reverse DNS names shared infrastructure ({marker})"
    return None

IPV4 = re.compile(r"(?<![\d.])(?:\d{1,3}\.){3}\d{1,3}(?![\d.])")
IP_LINK = re.compile(r"/ip_((?:\d{1,3}\.){3}\d{1,3})")
RSS_TITLE = re.compile(r"<title>\s*((?:\d{1,3}\.){3}\d{1,3})")
PEER_VARS = re.compile(r'^(PEERS|PRIVATE_PEERS|BLOCKED_CIDRS)="([^"]*)"', re.MULTILINE)


def die(msg: str) -> None:
    print("REFUSED: " + msg, file=sys.stderr)
    raise SystemExit(2)


def load_snapshots(directory: pathlib.Path, include_feeds: bool) -> list[tuple[str, str, bool]]:
    """(name, text, is_reviewed_page). RSS/XML rows are NOT reviewed pages."""
    if not directory.is_dir():
        die(f"snapshot dir is not a directory: {directory}")
    allowed = {".html", ".htm", ".txt"}
    if include_feeds:
        allowed |= {".xml", ".rss"}
    files = sorted(p for p in directory.iterdir() if p.is_file()
                   and p.suffix.lower() in allowed)
    if not files:
        die(f"no {sorted(allowed)} snapshots in {directory}; an empty payload "
            "must never produce an empty (or stale) deny set")
    out = []
    for p in files:
        text = p.read_text(errors="replace")
        if not text.strip():
            die(f"empty snapshot file {p.name}; an unparsable payload is refused")
        feed = p.suffix.lower() in {".xml", ".rss"} or "<rss" in text[:400]
        out.append((p.name, text, not feed))
    return out


def extract(name: str, text: str) -> list[str]:
    suffix = pathlib.Path(name).suffix.lower()
    is_feed = suffix in {".xml", ".rss"} or "<rss" in text[:400]
    if is_feed:
        found = RSS_TITLE.findall(text)
    elif suffix in {".html", ".htm"}:
        found = IP_LINK.findall(text) or IPV4.findall(text)
    else:
        found = IPV4.findall(text)
    return list(dict.fromkeys(found))


def peer_networks(repo_root: pathlib.Path) -> list:
    script = repo_root / "scripts" / "hive-lockdown.sh"
    if not script.is_file():
        die(f"cannot read {script}; the fleet-peer blast-radius guard needs it")
    tokens = []
    for _var, value in PEER_VARS.findall(script.read_text()):
        tokens += value.split()
    if not tokens:
        die("no PEERS/PRIVATE_PEERS/BLOCKED_CIDRS tokens parsed; "
            "refusing to generate a deny set without a peer guard")
    nets = []
    for tok in tokens:
        try:
            nets.append(ipaddress.ip_network(tok, strict=False))
        except ValueError:
            die(f"unparsable peer token {tok!r} in hive-lockdown.sh")
    return nets


def reverse_names(addrs: list[str]) -> dict[str, str]:
    def one(ip: str) -> tuple[str, str]:
        try:
            socket.setdefaulttimeout(3)
            return ip, socket.gethostbyaddr(ip)[0]
        except Exception as exc:
            return ip, "NO_PTR:" + type(exc).__name__
    with concurrent.futures.ThreadPoolExecutor(max_workers=12) as pool:
        return dict(pool.map(one, addrs))


def build(args) -> tuple[str, dict]:
    repo_root = pathlib.Path(__file__).resolve().parent.parent
    snapshots = load_snapshots(pathlib.Path(args.snapshot_dir), args.include_feeds)
    nets = peer_networks(repo_root)

    extra = set()
    if args.extra_exclude:
        for line in pathlib.Path(args.extra_exclude).read_text().splitlines():
            line = line.split("#", 1)[0].strip()
            if line:
                try:
                    extra.add(str(ipaddress.IPv4Address(line)))
                except ValueError:
                    die(f"unparsable --extra-exclude entry {line!r}")

    seen: dict[str, str] = {}
    reviewed: set[str] = set()
    for name, text, is_reviewed in snapshots:
        for raw in extract(name, text):
            try:
                ip = ipaddress.IPv4Address(raw)
            except ValueError:
                die(f"invalid IPv4 {raw!r} in {name}")
            if not ip.is_global:
                die(f"non-global address {raw} in {name}; a loopback/private/"
                    "reserved address in a reputation payload is a malformed "
                    "or hostile input")
            addr = str(ip)
            seen.setdefault(addr, name)
            if is_reviewed:
                reviewed.add(addr)
    if not seen:
        die("no IPv4 addresses parsed from the snapshots; never adopt an empty "
            "or unparsable payload")
    if not reviewed:
        die("no REVIEWED page in the snapshot set; the three human-visited "
            "HTML pages are the review gate, an RSS-only payload is not")

    ordered = sorted(seen, key=lambda a: int(ipaddress.IPv4Address(a)))
    for addr in ordered:
        ip = ipaddress.IPv4Address(addr)
        for net in nets:
            if ip in net:
                die(f"{addr} collides with fleet/peer network {net}; a deny "
                    "that bans a fleet peer or operator egress is a lockout "
                    "with no in-band recovery")

    ptrs = {} if args.no_ptr else reverse_names(ordered)
    denied, skipped = [], []

    def skip(ip: str, reason: str, ptr: str | None = None) -> None:
        row = {"ip": ip, "reason": reason}
        if ptr:
            row["ptr"] = ptr
        skipped.append(row)

    for addr in ordered:
        ptr = ptrs.get(addr, "")
        reason = shared_reason(addr, ptr)
        if addr not in reviewed and not args.include_unreviewed:
            skip(addr, "unreviewed: present only in an RSS/XML feed, not on "
                 "the three visited pages (use --include-unreviewed)")
        elif addr in extra:
            skip(addr, "operator --extra-exclude")
        elif reason and not args.include_shared_infra:
            skip(addr, reason, ptr or None)
        else:
            denied.append(addr)

    if not denied:
        die("every parsed address was excluded; refusing to emit an empty "
            "deny set (an empty set would silently disable the deny)")
    if len(denied) > args.max_ips:
        die(f"{len(denied)} addresses exceeds --max-ips {args.max_ips}; "
            "a feed glitch must not inject an unbounded deny list")

    elements = ", ".join(denied)
    ruleset = f"""# Generated by scripts/honeypot-blocklist.py -- do not hand-edit.
# {len(denied)} exact /32 entries, each expiring after {args.ttl_days}d.
# A later reassignment of any address is therefore not a permanent ban.
table {TABLE} {{
  set {SET} {{
    type ipv4_addr
    flags timeout
    timeout {args.ttl_days}d
    elements = {{ {elements} }}
  }}
  chain input {{
    type filter hook input priority -105; policy accept;
    # Runs before hive_lockdown (-100), strict (-90) and fail2ban (-1). A
    # DROP verdict is terminal, so no later accept can rescue this source.
    # policy accept on THIS chain lets every other packet pass through to
    # the normal firewall chains untouched.
    ip saddr @{SET} counter drop
  }}
}}
"""
    report = {
        "snapshots": [{"name": n, "reviewed_page": r} for n, _t, r in snapshots],
        "parsed_unique": len(seen),
        "reviewed_unique": len(reviewed),
        "denied": len(denied),
        "excluded_total": len(skipped),
        "ttl_days": args.ttl_days,
        "max_ips": args.max_ips,
        "include_feeds": bool(args.include_feeds),
        "include_unreviewed": bool(args.include_unreviewed),
        "include_shared_infra": bool(args.include_shared_infra),
        "ptr_classification": not args.no_ptr,
        "excluded": skipped,
        "denied_ips": denied,
    }
    return ruleset, report


def need_nft() -> str:
    exe = shutil.which("nft")
    if not exe:
        die("nft is not installed on this host; this tool runs on fleet nodes only")
    if os.geteuid() != 0:
        die("apply/remove need root (nft ruleset changes)")
    return exe


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    sub = ap.add_subparsers(dest="cmd", required=True)
    base = argparse.ArgumentParser(add_help=False)
    base.add_argument("--include-shared-infra", action="store_true")
    base.add_argument("--extra-exclude")
    base.add_argument("--ttl-days", type=int, default=DEFAULT_TTL_DAYS)
    base.add_argument("--max-ips", type=int, default=DEFAULT_MAX_IPS)
    common = argparse.ArgumentParser(add_help=False, parents=[base])
    common.add_argument("--snapshot-dir", required=True)
    common.add_argument("--include-feeds", action="store_true")
    common.add_argument("--include-unreviewed", action="store_true")
    common.add_argument("--no-ptr", action="store_true")
    common.add_argument("--report")
    sub.add_parser("generate", parents=[common])
    a = sub.add_parser("apply", parents=[common])
    a.add_argument("--operator-confirm", action="store_true")
    sub.add_parser("status")
    r = sub.add_parser("remove")
    r.add_argument("--operator-confirm", action="store_true")
    args = ap.parse_args()

    if args.cmd in {"apply", "remove"} and not args.operator_confirm:
        die("this is a production firewall change; pass --operator-confirm "
            "only after the operator has reviewed the generated set")

    report = None
    ruleset = None
    tmp = None
    if args.cmd in {"generate", "apply"}:
        if args.ttl_days < 1 or args.ttl_days > 90:
            die("--ttl-days must be between 1 and 90")
        if args.max_ips < 1:
            die("--max-ips must be positive")
        ruleset, report = build(args)
        if args.report:
            pathlib.Path(args.report).write_text(
                json.dumps(report, indent=2, sort_keys=True) + "\n")
        print(f"RESULT PASS denied={report['denied']} "
              f"excluded={report['excluded_total']} "
              f"reviewed={report['reviewed_unique']} "
              f"ttl_days={args.ttl_days}", file=sys.stderr)
        if args.cmd == "generate":
            print(ruleset, end="")
            return
        f = tempfile.NamedTemporaryFile("w", suffix=".nft", delete=False)
        f.write(ruleset)
        f.close()
        tmp = pathlib.Path(f.name)

    exe = need_nft()
    if args.cmd == "status":
        p = subprocess.run([exe, "list", "table", *TABLE.split()],
                           capture_output=True, text=True)
        print(p.stdout or p.stderr, end="")
        raise SystemExit(p.returncode)

    if args.cmd == "remove":
        p = subprocess.run([exe, "delete", "table", *TABLE.split()],
                           capture_output=True, text=True)
        print(p.stdout or p.stderr or f"removed {TABLE}")
        raise SystemExit(p.returncode)

    # apply: check syntax first, then re-create the table so a re-run is
    # idempotent and cannot accumulate duplicate chains or rules.
    check = subprocess.run([exe, "-c", "-f", str(tmp)],
                           capture_output=True, text=True)
    if check.returncode != 0:
        die(f"nft -c -f rejected the ruleset, nothing loaded: "
            f"{(check.stderr or check.stdout).strip()}")
    subprocess.run([exe, "delete", "table", *TABLE.split()],
                   capture_output=True, text=True)
    load = subprocess.run([exe, "-f", str(tmp)], capture_output=True, text=True)
    if load.returncode != 0:
        die(f"nft -f failed: {(load.stderr or load.stdout).strip()}")
    verify = subprocess.run([exe, "list", "set", *TABLE.split(), SET],
                            capture_output=True, text=True)
    if verify.returncode != 0:
        die("ruleset loaded but the deny set could not be listed back")
    print(f"RESULT PASS applied {TABLE}: {report['denied']} entries, "
          f"{args.ttl_days}d timeout, verified with nft list set")
    tmp.unlink(missing_ok=True)


if __name__ == "__main__":
    main()
