# Firewall path deny index

Present tense. Source of truth for `crates/hive-cloud/src/firewall.rs::sensitive_rule`,
which `edge_gate` calls BEFORE the bypass list, before ACME and before any
canonical/host redirect (`crates/hive-cloud/src/edge.rs:129`). That makes a deny
apply to `shadw.cloud` and every `*.shadw.app` deployment on the shared public
HTTPS router; `dedicated_ipv4_listener.rs` reuses the exact same router, so
dedicated-IP requests get the same deny. Raw TCP/UDP `raw_ports` are not HTTP and
carry no path; they cannot be path-filtered and are out of scope here.

## Corpus

The geniee.site request log (2026-11-19, 151 lines, 150 unique normalized paths).
Classification is done against `normalize_path`, which double-percent-decodes,
collapses `..`, backslashes and matrix params, strips query/fragment and
lowercases. A derived classifier reproduced `sensitive_rule` exactly:
99 of 151 requests denied, 52 missed (51 unique after normalization/lowercase).

| existing rule | denied |
| --- | --- |
| `sensitive-dotfile` (any dot-segment except `.well-known`, `.hive-verify`, `.nojekyll`) | 76 |
| `sensitive-probe` (segment/prefix) | 10 |
| `sensitive-secret-file` | 6 |
| `sensitive-dump` | 5 |
| `sensitive-backup` | 2 |

## Always-bad: hard deny (DX-safe)

These name a private key, a secret-bearing tool config, a framework-internal
installer/debug/log endpoint. No legitimate tenant route needs them. Exact
normalized paths, not stems: a stem deny on `auth` would also kill `/auth.js`,
which is a real asset. Adding these as EXACT paths cannot prohibit a developer
from exposing any route they might reasonably want.

| normalized path | why always bad |
| --- | --- |
| `/service-account-key.json` | cloud service-account private key |
| `/service_account.json` | cloud service-account private key |
| `/sendgrid.env` | mail API-key file |
| `/sentry.properties` | Sentry CLI auth token |
| `/auth.json` | Composer/npm auth credentials |
| `/claude_desktop_config.json` | MCP/AI tool credential config |
| `/aider.conf.yml` | AI tool config carrying API keys |
| `/oai_config_list.json` | LiteLLM model-key list |
| `/oai_config_list` | same, extensionless form (log had `/OAI_CONFIG_LIST`) |
| `/litellm.yaml` | LLM proxy master-key config |
| `/litellm/config.yaml` | same, namespaced |
| `/config/custom-environment-variables.json` | node-config secret mapping |
| `/storage/logs/laravel.log` | runtime log, may hold secrets |
| `/wp-content/debug.log` | framework debug log |
| `/wp-admin/install.php` | framework installer |
| `/server-info` | Apache `mod_info` |
| `/telescope/requests` | Laravel Telescope debug |
| `/actuator/env` | Spring actuator env dump (secrets) |
| `/actuator/heapdump` | Spring actuator heap dump |

Generated patterns the log did not contain but the same scanner families use,
classified always-bad on the same criterion. Dot-segment ones are already denied
by `sensitive-dotfile`; the rest are generated exact paths.

Already covered by the dot-segment rule (do NOT duplicate): `/.git/config`,
`/.git/HEAD`, `/.git/credentials`, `/.git-credentials`, `/.svn/entries`,
`/.hg/hgrc`, `/.bzr/`, `/.DS_Store`, `/.npmrc`, `/.yarnrc`, `/.pypirc`,
`/.dockercfg`, `/.kube/config`, `/.aws/credentials`, `/.ssh/id_rsa`,
`/.bash_history`, `/.zsh_history`, `/.pgpass`, `/.my.cnf`, `/.netrc`,
`/.htpasswd`, `/.env.production`, `/.env.local`, `/.env.bak`, `/.env.old`,
`/.env.save`, `/.env.php`, `/.env.example`, `/.env.sample`, `/.env.swp`,
`/.config/gcloud/application_default_credentials.json`, `/.terraform/terraform.tfstate`.
`/.tfstate`, `/.tfvars`, `/terraform.tfstate.backup` are already denied by
`SENSITIVE_SUFFIXES` + the `terraform.tfstate` branch.

Generated exact paths to add: `/actuator/configprops`, `/actuator/mappings`,
`/actuator/beans`, `/actuator/httptrace`, `/actuator/threaddump`,
`/actuator/loggers`, `/actuator/jolokia`, `/wp-admin/setup-config.php`,
`/wp-admin/upgrade.php`, `/phpinfo.php`, `/info.php`, `/adminer.php`,
`/phpmyadmin/index.php`, `/trace.axd`, `/debug/vars`, `/etc/passwd`,
`/proc/self/environ`. Generated-but-NOT-denied: `/wp-json/wp/v2/users` — user
enumeration is an operation, not a secret file, and a WordPress-shaped tenant
could want it; record it as a residual rather than a hard deny.

## DX-prohibitive: never hard-deny

A hard deny here would break a real tenant asset or a developer's chosen route.
These stay served, challenged or rate-limited, not 403. The existing
`SENSITIVE_SEGMENTS` comment already says config.js/settings.json/app.js/Dockerfile
are deliberately excluded for exactly this reason.

| family | examples | why not deny |
| --- | --- | --- |
| public runtime-config JS | `/env.js`, `/env-config.js`, `/runtime-config.js`, `/__ENV.js`, `/app.config.js`, `/config.js`, `/js/config.js`, `/config/config.js`, `/DataFiles/env.js`, `/aws-exports.js` | SPAs fetch these at boot for non-secret public settings |
| static bundles | `/index.js`, `/app.js`, `/` | ordinary assets |
| publishable manifests | `/Dockerfile`, `/composer.json`, `/docker-compose.yml`, `/compose.yaml`, `/Jenkinsfile`, `/azure-pipelines.yml`, `/bitbucket-pipelines.yml`, `/netlify.toml`, `/serverless.yml`, `/serverless.yaml`, `/vercel.json`, `/vendor/composer/installed.json` | docs/repo-browser tenants may publish these. `.sql`/`.tfstate` variants stay denied by suffix |
| node-config JSON | `/config/default.json`, `/config/local.json`, `/config/production.json`, `/config/aws.yml` | a tenant may serve public runtime config here; only the secret mapping is denied |
| observability | `/metrics` | Prometheus endpoint |
| ambiguous names | `/config.env`, `/config.php`, `/env`, `/env.txt` | a stem deny would kill legitimate `/config.*` assets; keep out of the deny list and let the challenge/rate-limit path handle scanners.

## Generated request patterns (not in the log)

Operation shapes the log did not show but the same corpus implies:

| shape | decision |
| --- | --- |
| `GET /?file=../../etc/passwd`, `/?path=`, `/?cmd=`, `/?url=` | LFI/RFI probes. `normalize_path` strips the query, so a path-only deny cannot see them; `hive-edge/waf.rs` evaluates `path + query` for SQLi/XSS. Record as a residual: adding query-key deny rules needs a separate change with its own witness. |
| `POST /xmlrpc.php`, `GET /wp-json/wp/v2/users`, `/?author=1` | WordPress user enumeration / brute-force endpoints. A WordPress-shaped tenant could want them; do NOT hard-deny. |
| `/.git/config`, `/.aws/credentials`, `/.kube/config`, `/.env.production` | all dot-segments, already denied by `sensitive-dotfile`; listed so a future refactor does not "re-add" them as stems. |
| `/../../etc/passwd`, `/%2e%2e/etc/passwd`, `/..;/etc/passwd` | `normalize_path` collapses the traversal to `/etc/passwd`, so the exact-path deny below covers all three encodings. |
| `HEAD`/`OPTIONS`/`TRACE` on a denied path | `edge_gate` is method-independent: the deny runs before any method branch. |

Verification gaps to name honestly: raw TCP/UDP `raw_ports` bypass HTTP path
filtering by construction; they carry no path to match. LFI/RFI query probes are
also unmatched (see the table above), and WordPress user enumeration is a
deliberate residual rather than a deny. The deny set and its DX-safe neighbours
are witnessed by executing the real `sensitive_rule` source; see
`docs/PENDING_VALIDATION.md` for the run.

## Decision rule going forward

Add to the exact-path deny list only when ALL hold: (1) the path names a private
key, secret-bearing tool config, or framework-internal installer/debug/log
endpoint; (2) no plausible tenant route needs it; (3) the match is exact, so an
ambiguous neighbour (`/actuator/health`, `/wp-admin/admin-ajax.php`, `/auth.js`)
stays reachable. When in doubt, leave it to the challenge/rate-limit path and
record the residual instead of a hard deny.
