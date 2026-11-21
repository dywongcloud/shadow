# Project Honey Pot IP deny: decision record

Present tense, 2026-11-20. Answers "we should block all the IPs here too" for
the three `projecthoneypot.org/list_of_ips.php` pages (`?t=s`, `?t=d`, and the
default). No IP is blocked yet: applying the generated set to a node is a
production change and needs the operator's word. No push, no roll, no apply.

## What the three pages actually are

Fetched live 2026-11-20, each page read whole (`status 200`, `truncated:false`):

| URL | title | meta description |
| --- | --- | --- |
| `/list_of_ips.php` | Malicious IPs \| By Last Bad Event | A list of the Top 25 Malicious IPs; Arranged by their Last Bad Event. |
| `/list_of_ips.php?t=s` | Spam Server IPs \| By Last Bad Event | A list of the Top 25 Spam Server IPs; Arranged by their Last Bad Event. |
| `/list_of_ips.php?t=d` | Dictionary Attacker IPs \| By Last Bad Event | A list of the Top 25 Dictionary Attacker IPs; Arranged by their Last Bad Event. |

These are **rotating leaderboards, not a blocklist**. Each HTML page has
exactly 25 unique addresses (50 occurrences: each appears once in an
`/ip_<addr>` link and once as link text). The three HTML pages union to 42.
The `?rss=1` feeds expose 50 items each and union to 91 — 49 addresses beyond
what the linked pages show. `lastBuildDate` on the feed is a timestamp of the
current day, and rows carry `First`/`Last` dates, so the set changes daily.
A committed snapshot would be stale within hours.

`scripts/honeypot-blocklist.py` therefore produces a **TTL-bounded** nft set
(default 7 days), not a permanent rule. A later reassignment of any address
stops being denied when the element expires.

## Terms of use and the supported API

`terms_of_use.php` (read whole) says, verbatim:

> Reproduction of the content of the Website without the written permission
> of Unspam is prohibited.

> This license prohibits your downloading (other than page caching) or
> modifying any portion of it, except with express, written consent of
> Unspam.

SECTION 4, "SPECIAL LICENSE RESTRICTIONS FOR NON-HUMAN VISITORS":

> Non-Human Visitors include, but are not limited to, web spiders, bots,
> indexers, robots, crawlers, harvesters, or any other computer programs
> designed to access, read, compile or gather content from the Website
> automatically. Non-Human Visitors are restricted from taxing the resources
> of the Website beyond what would be typical of a human visitor.

SECTION 7:

> UNSPAM EXPRESSLY DISCLAIMS ANY WARRANTY OF FITNESS FOR ANY PARTICULAR
> PURPOSE OF DATA PROVIDED THROUGH THE PHP WEBSITE OR OTHER PHP SERVICES.

> YOUR ASSENT TO THIS AGREEMENT THROUGH THE USE OF THE PHP WEBSITE
> EXPLICITLY ACKNOWLEDGES THAT YOU ALONE AND WITHOUT ANY INSTRUCTION OR
> INFLUENCE BY UNSPAM MAKE ANY DETERMINATION TO BLOCK, RESTRICT, FILTER,
> OR TERMINATE ACCESS TO OR FROM ANY ENTITIES, EMAIL ADDRESS, OR IP
> ADDRESSES.

> YOU ACKNOWLEDGE THAT UNSPAM EXERCISES LITTLE IF ANY EDITORIAL CONTROL
> OVER MATERIAL POSTED BY USERS AND OTHER ENTITIES ON THE PHP WEBSITE.

The supported machine interface is **http:BL** (`httpbl_api.php`), a DNSBL:
"To use http:BL a host need simply perform a DNS lookup of a web visitor's IP
address", and "Each user of http:BL must also request an Access Key". Keys are
12 lowercase alpha characters; "sharing keys with other members or
non-members" is a Terms of Service violation. An Access Key is a secret and is
NOT in this repo, NOT requested here and NOT printed. A real-time DNS lookup
on the request path would also stall the serving runtime, which is forbidden
by the project's runtime discipline.

Consequences for this change:

- The generator makes **no request** to projecthoneypot.org. The operator
  saves the three human-visited pages to a local snapshot directory and runs
  `generate`. No fleet node, boot path or request path scrapes the site.
- **No copy of their IP list is committed.** The generator classifies against
  generic shared-infrastructure networks and reverse-DNS markers only.
- RSS/XML rows are treated as **unreviewed** and excluded unless
  `--include-unreviewed` is passed, so a feed that grows from 25 to 50 items
  cannot silently expand the deny set.
- `apply` still requires `--operator-confirm`: blocking traffic is a
  production one-way door.

## Why not "block all 42"

Reverse DNS (live, 2026-11-20) on the 42 addresses found 20 that are shared
or ephemeral egress rather than stable attacker identities:

- 15 resolve to `*.bc.googleusercontent.com` — ephemeral GCP VM addresses
  that are reassigned to unrelated customers.
- `172.253.132.207` -> `yufwakf-in-f207.1e100.net` (Google / Googlebot).
- `104.18.161.117` has no PTR but is inside Cloudflare's `104.16.0.0/12`.
- `23.129.64.202` -> `emeraldonion.net`, a shared Tor exit.
- `147.253.220.182` -> SparkPost transactional mail; `203.138.203.3` ->
  `mail103.docomo.ne.jp`, a shared mail relay.

A flat deny of those would drop Googlebot crawling `shadw.cloud` and every
`*.shadw.app` deployment, break Cloudflare-fronted tenant traffic and mail
callbacks, ban Tor users, and permanently deny an address that GCP may
reassign tomorrow. That is the same DX-harming class the path-deny index
deliberately leaves served. The default generator result on the three
reviewed pages is **22 denied, 20 excluded**.

`--include-shared-infra` is available for an operator who explicitly wants
the flat deny after reviewing that cost; it is not the default. `--no-ptr`
disables reverse-DNS classification and admits **more** addresses (39 on
this snapshot), never fewer.

## Why a separate nft table

`scripts/hive-lockdown.sh` is the SOLE host firewall and runs at boot. Its
`BLOCKED_CIDRS` is a hand-reviewed, permanent CIDR with an explicit comment
that it contains no fleet peer, private VPC or operator egress address. A
rotating third-party list belongs in neither a boot path nor that variable.

`honeypot-blocklist.py` emits `table inet honeypot_deny` with a
`type filter hook input priority -105; policy accept` chain. It runs BEFORE
`hive_lockdown` (-100), the strict table (-90) and fail2ban (-1); a DROP
verdict is terminal, so no later chain can rescue the source. `policy accept`
on that chain passes every other packet to the normal firewall untouched.
Remove is one command: `nft delete table inet honeypot_deny`.

`apply` runs `nft -c -f` (syntax check) before loading, re-creates the table
so a re-run cannot accumulate duplicate chains, then lists the set back to
prove it landed. It requires root, `nft`, and `--operator-confirm`.

## Fail-closed guards (all live-witnessed)

| guard | behaviour |
| --- | --- |
| empty or missing snapshot dir | refused |
| unparsable payload / no IPv4 | refused |
| non-global address (loopback, RFC1918, reserved) in payload | refused |
| address collides with `PEERS`, `PRIVATE_PEERS` or `BLOCKED_CIDRS` | refused — a fleet lockout has no in-band recovery |
| no reviewed HTML page in the set | refused |
| more than `--max-ips` (default 200) | refused |
| `--ttl-days` outside 1..90 | refused |
| every address excluded (empty deny set) | refused, rather than silently disabling the deny |
| `apply` / `remove` without `--operator-confirm` | refused |

## Residuals, named honestly

- **No IP is currently blocked.** No apply, no push, no roll, no
  `site.yml --tags lockdown`. All need the operator's word.
- `nft` is not installed on this macOS host, so the emitted ruleset was
  witnessed structurally (table/set/chain/drop/timeout and the exclusion
  assertions) and by `py_compile`, but `nft -c -f`, `nft -f` and `nft list
  set` were NOT executed here. `apply` performs all three on the node.
- IPv6 is out of scope: the source lists are IPv4 only and the generated set
  is `type ipv4_addr`.
- The three pages are a Top-25 sample, not an exhaustive threat feed.
  `fail2ban` (including the `sshd-slow` jail) remains the behaviour-based
  defence against the login attacks that motivated this work.
- Reverse DNS can be absent or stale; `--no-ptr` and `--extra-exclude` exist
  for that case, and PTR is used only to EXCLUDE, never to admit.
- http:BL integration is not implemented: it needs an operator-held Access
  Key and must not add a DNS lookup to the request path.
