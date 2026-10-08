# Game server presets

These are catalog/configuration presets, not a claim of playable, capacity-approved
or paid deployments. The Games menu unifies Minecraft, Terraria, Factorio, Palworld,
Valheim, Arma 3 and ARK: Survival Evolved. Existing image, port, memory and
environment defaults are preserved; Minecraft now explicitly names /data. No upstream source code, game binaries or passwords are copied.

## Audited upstream revisions (2026-10-08)

The README, Compose, Dockerfile/build pipeline and license at these revisions are
our source of truth; image tags below remain mutable and are NOT digest pins.

- Palworld: [Thijs van Loef / palworld-server-docker](https://github.com/thijsvanloef/palworld-server-docker/tree/5d5893cdfeb8b278ef05cce4fac7ea26d7ff95cc).
  README, compose.yaml, Dockerfile, .env.example and .github/workflows/release.yml.
  MIT, copyright 2024 Thijs van Loef.
- Valheim: [community-valheim-tools / valheim-server-docker](https://github.com/community-valheim-tools/valheim-server-docker/tree/4665ba7fe205cb4f1869abbd38d82b6807f71a19).
  README, docker-compose.yaml, Dockerfile and .github/workflows/docker-build.yml.
  Apache-2.0; Valheim/game branding remains Iron Gate Studio's property.
- Arma 3: [BrettMayson / Arma3Server](https://github.com/BrettMayson/Arma3Server/tree/7fa59aeba2a9425b7aa64a6a4f853e895060ef4d).
  README, docker-compose.yml, Dockerfile, .env.example, launch.py and api.py.
  No explicit LICENSE file found at this revision; only the published image is
  referenced. This does not grant rights to redistribute upstream code or games.
- Terraria: [PassiveLemon / terraria-docker](https://github.com/PassiveLemon/terraria-docker/tree/6b5e2cb002625ed7b34678b71a9e6f101c7fd258).
  README and terraria image build definitions. GPL-3.0; upstream notices remain
  with upstream. Existing AUTOCREATE=2 and WORLDNAME=World are retained.
- Factorio: [factoriotools / factorio-docker](https://github.com/factoriotools/factorio-docker/tree/cffe0730048aaeacc059a9b7c1de56c4d4efe682).
  README and image build definitions. MIT; game-server distribution remains
  subject to Wube's game terms, not the wrapper's MIT license.

- Minecraft: [itzg / docker-minecraft-server](https://github.com/itzg/docker-minecraft-server/tree/70b3f73ebc6110aea1bfb6c918e0afc2c55055e7).
  Dockerfile, scripts/start*, docs/versions/minecraft.md,
  docs/types-and-platforms (Vanilla/Paper), docs/configuration/server-properties.md,
  LICENSE (Apache-2.0). TYPE/VERSION/LEVEL/MAX_PLAYERS/MODE/DIFFICULTY are upstream controls.
- ARK: [Hermsi1337 / docker-ark-server](https://github.com/Hermsi1337/docker-ark-server/tree/984d901311a0ec842203ef0d6c7a0905a3604b02).
  README.md, Dockerfile, bin/docker-entrypoint.sh, bin/steam-entrypoint.sh,
  conf.d/arkmanager-user.cfg, conf.d/arkmanager-sub.cfg.template and LICENSE.
  MIT, copyright 2018 Dennis Hermsmeier. Only image references/derived metadata,
  not upstream scripts. **Survival Evolved, not Survival Ascended.**

## Audited configure controls and profiles

The same metadata drives Games navigation, labeled forms, validation, summary and
image payload in both repositories. Profiles are small supported env overlays,
not claims of game edition compatibility or preinstalled mods.

| Game | Supported controls / profiles | Version semantics |
| --- | --- | --- |
| Minecraft Java | Vanilla/Paper survival; EULA, world, players, mode, difficulty | VERSION runtime LATEST/vanilla SNAPSHOT/exact release syntax; OCI latest or user tag/digest |
| Terraria vanilla | Existing medium/default, new small/large; world, seed, difficulty, players, optional password | terraria-latest OCI channel controls binary; no fake runtime selector |
| Factorio | Load existing or generate save; SAVE_NAME, GENERATE_NEW_SAVE, LOAD_LATEST_SAVE | stable/latest OCI channels; exact custom OCI tag/digest syntax only |
| Palworld | Private direct connect; name, players, join password | latest OCI; Steam runtime independent |
| Valheim | Private Steam/crossplay; name, world, password, listing | latest OCI; downloaded runtime independent |
| Arma 3 | Prepared config volume; profile/startup world, headless count, local Launcher HTML mod preset, dedicated Steam login | v2 OCI; refreshed Steam depots independent |
| ARK ASE | Private TheIsland or custom map/mods; session/map/players, join/admin passwords, numeric Workshop IDs, update/validation/backup switches | latest OCI; ASE runtime download/update independent |

Field ranges are conservative **form bounds**, not certified upstream capacity.
Custom image tags/digests are limited to each audited repository and syntax
validated. They are not fabricated supported releases or verified registry
availability. Real releases/mods/maps must be checked by the operator; TheIsland
is the only seeded ARK map. No Ascended image/variant is offered.

Factorio player limits/passwords/mod portal authentication belong to prepared
server-settings.json, not invented env controls. Arma mission/player/game/admin
settings belong to prepared configs; MODS_PRESET is a Launcher HTML preset path,
not fabricated Workshop ID support. Terraria remains vanilla, no tModLoader.
Advanced env editing remains available for existing non-transport settings and
additional upstream keys; public ports/admin exposure are fixed to safe metadata.
Port changes require a separate audited custom-image flow; game-specific
port/env coupling cannot drift through these screens.

Configure → review → create is explicit, with back navigation and readiness
checkboxes for image/platform/resources/volume/backup/backend policy; multiport
profiles additionally require verified direct serving-node ingress. The entire
summary hides env VALUES, even under unrecognized keys, to prevent cross-key
credential leaks. Secrets are masked in inputs; errors name fields only, never
reflect submitted credentials or raw backend error strings. No config export.

**Local runtime limitation:** the running native backend has not received the
last secret-policy changes. Do not submit real credentials there. Publication
of this UI/backend source does not upgrade running binaries. The form warns
and requires operator confirmation; this task never logs in to Steam, pulls
images/game binaries or restarts mesh/DB/backend services.

## ARK-specific operations

ASE exposes 7777/UDP (game), 7778/UDP (game+1), 27015/UDP (query), persists /app.
Upstream enables internal 27020/TCP RCON; it is deliberately NOT in public ports.
ADMIN_PASSWORD and SERVER_PASSWORD default empty, overriding unsafe sample defaults.
The image is linux/amd64 only; first start is a very large server download. 8g
is only a requested memory budget, not an upstream minimum or verified capacity.
Keep WSL volumes on Linux filesystem, not Windows-mounted storage. Backup/update
switches follow Dockerfile/entrypoint support; VALIDATE_ON_START only matters
with UPDATE_ON_START=true. UPDATE_CRON/RESTART_CRON are explicitly unsupported
upstream and not offered. No arbitrary map/version list or default credential.

## Transport and persistence

All use the prebuilt-image /v1/deploy/image engine, not Git builds or HTTP ingress.
A nonempty ports list REPLACES defaults, so the primary port is included alongside
all seeded extra ports. Container ports below are not promises of public ports.
**Existing engine limitation:** fluid-core documents that mesh_raw only forwards
the PRIMARY port (spec_idx != 0 is unsupported). Secondary raw ports receive
allocations but are not cross-node forwardable. Valheim, Arma and ARK therefore need
verified serving-node raw ingress for all secondary ports; a mesh-only deployment
is NOT a ready-to-play multiport server. This preset task does not change routing.

| Preset | Image | Container ports | Persistent mount |
| --- | --- | --- | --- |
| Minecraft (unchanged) | itzg/minecraft-server:latest | 25565/TCP | /data (existing default) |
| Terraria | passivelemon/terraria-docker:terraria-latest | 7777/TCP | /opt/terraria/config |
| Factorio | factoriotools/factorio:stable | 34197/UDP | /factorio |
| Palworld | thijsvanloef/palworld-server-docker:latest | 8211/UDP | /palworld |
| Valheim | ghcr.io/community-valheim-tools/valheim-server:latest | 2456–2458/UDP | /config |
| Arma 3 | ghcr.io/brettmayson/arma3server/arma3server:v2 | 2302–2306/UDP | /arma3/server |
| ARK: Survival Evolved | hermsi/ark-server:latest | 7777,7778,27015/UDP; no public RCON | /app |

Palworld upstream publishes amd64/arm64 images; Valheim's build and Arma's Compose
specify linux/amd64. Terraria/Factorio retain existing image architecture behavior;
this catalog does not implement architecture-aware placement. Verify the selected
node and registry image before deploying; no platform emulation is promised.

World persistence uses the engine's existing project volume path. Valheim's
optional /opt/valheim cache is not persisted: the engine accepts a single mount,
and /config is the essential world/backup directory. Image defaults without a
volume_mount_path still use existing /data behavior. Shadow includes the minimal
request/fanout/settings/mount plumbing needed to honor non-/data game paths.

## Safe configuration and operational gates

- Palworld requires a user-supplied SERVER_PASSWORD here. COMMUNITY=false;
  no 27015/query port is exposed unless an operator adds it for community listing
  and configures PUBLIC_PORT/IP to match the real allocation. RCON and REST are
  disabled. Never publish 8212 REST or enable DEBUG with credentials. Upstream
  minimum is 16 GB RAM, 4 cores and 8 GB disk (larger recommended); the platform
  can clamp resource limits. The preset requests 16g, not verified capacity.
- Valheim requires SERVER_PASS of at least five characters. SERVER_PUBLIC=false
  and CROSSPLAY=false. Supervisor HTTP (9001) and status HTTP are disabled and
  unpublished. The third UDP port follows upstream Compose/EXPOSE, although the
  basic vanilla README example uses only 2456–2457. CAP_SYS_NICE is optional and
  not requested. Upstream recommends 120 seconds for graceful shutdown; this
  preset does not add lifecycle policy. Back up worlds before upgrades.
- Arma requires private STEAM_USER and STEAM_PASSWORD. Upstream says the account
  need not own Arma 3 but Steam Guard must be disabled. Do not disable Steam Guard
  on a primary account: use an operator-approved dedicated account or do not
  deploy this image. No credentials are invented, stored in preset metadata or
  included in errors. Upstream itself logs the Steam account display name; do
  not share raw upstream logs without review.
- **Arma is not ready on an empty volume.** Operators must prepare
  /arma3/server/configs/main.cfg, missions and needed mods before starting. The
  published Dockerfile does not copy the repository configs into the image.
  The catalog does not bootstrap those files or configure admin/game passwords.
  Keep config credentials private, not in public template definitions.
- Container port fields are editable; keep image PORT/SERVER_PORT and every
  dependent port consistent. Confirm firewall and actual public allocation for
  multiport/adjacent-port games. HTTP exposure does not provide UDP support.
  The mesh's reliable ordered UDP framing can add head-of-line latency; packet
  reachability, game discovery and acceptable latency need real client tests.
- All new secret values default empty. The configure screen masks credential
  inputs and prevents creation with missing required env. Server settings force
  game credential keys into existing encrypted-at-rest/masked env storage even
  when an image request cannot supply a sensitive flag. This change must reach
  the backend to protect arbitrary Steam/password values; updating UI alone is
  insufficient. Runtime operators and upstream game processes still receive
  credentials by design. No secrets/provider config are changed by this task.
- Pin an audited image digest/version before production. v2/stable/latest are
  moving tags; Arma also refreshes game depots on startup, independent of OCI pin.
  Actual first-start downloads, game launches, Steam login and client join tests
  are intentionally not part of this catalog change.

## Verification

Run from ui: node scripts/check-game-presets.mjs (Node 24's TypeScript stripping).
This is an offline catalog/schema regression validator, not a mock game server.
It checks all seven identities, transport/persistence, multiports, typed fields,
profiles, OCI/runtime version validation, missing/short credentials, masked
summary values, backend policy and accessibility/navigation source guards.
These source guards are NOT DOM/browser interaction proof.

Also run the repository frontend type/build checks and applicable Rust compilation
before deployment. Browser interaction and game-runtime verification are separate
claims; a green catalog script/build cannot establish either. No browser policy
restriction should be bypassed to perform this work.
