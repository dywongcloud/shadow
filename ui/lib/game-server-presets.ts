/** Image references only: upstream code/licenses remain with their authors.
 * Pinned source audit and operational prerequisites: docs/game-server-presets.md.
 */
export type GameField = { key: string; label: string; kind: "text" | "secret" | "integer" | "select" | "version" | "ids"; options?: string[]; min?: number; max?: number; help?: string };
export type GameProfile = { id: string; label: string; env: Record<string, string> };
export type GamePresetFields = {
  gameId?: string; fields?: GameField[]; profiles?: GameProfile[]; imageTags?: string[]; platform?: string; runtimeVersionNote?: string;
  extraPorts?: { port: number; protocol: string; label: string }[];
  requiredEnv?: { key: string; minLength: number }[];
  setupNotes?: string[];
};
export type GameServerPreset = GamePresetFields & {
  name: string; desc: string; image: string; port: number; protocol: string;
  memory: string; volumeMountPath: string; tag: string; color: string;
  env: Record<string, string>; requiredEnv: { key: string; minLength: number }[]; setupNotes: string[];
};
const BASE_GAME_PRESETS: GameServerPreset[] = [
{"name":"Minecraft Server","desc":"Java Edition server (itzg/minecraft-server) with a persistent world, raw TCP.","image":"itzg/minecraft-server:latest","port":25565,"protocol":"tcp","env":{"EULA":"TRUE"},"memory":"3g","tag":"MC","color":"#5b8c3e","volumeMountPath":"/data","requiredEnv":[],"setupNotes":["Accept Mojang's EULA before creating. OCI version selects the image/Java environment; VERSION selects the runtime download."]},
{"name":"Terraria Server","desc":"Vanilla Terraria server with persistent worlds (TCP). Pin an image tag before important upgrades.","image":"passivelemon/terraria-docker:terraria-latest","port":7777,"protocol":"tcp","env":{"AUTOCREATE":"2","WORLDNAME":"World"},"volumeMountPath":"/opt/terraria/config","memory":"2g","tag":"TR","color":"#718c45","requiredEnv":[],"setupNotes":["AUTOCREATE must match existing world size; changes can corrupt world appearance. Vanilla only, not tModLoader. Back up worlds first."]},
{"name":"Factorio Server","desc":"Stable headless Factorio server with persistent saves (UDP). Pin a version before upgrading.","image":"factoriotools/factorio:stable","port":34197,"protocol":"udp","env":{},"volumeMountPath":"/factorio","memory":"3g","tag":"FA","color":"#b77b39","requiredEnv":[],"setupNotes":["Prepare /factorio/config/server-settings.json for player limits, passwords and mod portal credentials; these are file settings, not env controls. Verify save/mod compatibility."]},
  {
    name: "Palworld Server", desc: "Palworld dedicated server with persistent saves (UDP). Set a private server password before creating.",
    image: "thijsvanloef/palworld-server-docker:latest", port: 8211, protocol: "udp", memory: "16g",
    volumeMountPath: "/palworld", tag: "PW", color: "#3d9eb0",
    env: { PORT: "8211", PLAYERS: "16", SERVER_NAME: "Palworld", SERVER_PASSWORD: "", COMMUNITY: "false", RCON_ENABLED: "false", REST_API_ENABLED: "false", LOG_LEVEL: "INFO" },
    requiredEnv: [{ key: "SERVER_PASSWORD", minLength: 1 }],
    setupNotes: ["Upstream supports linux/amd64 and linux/arm64. Needs at least 16 GB RAM and 8 GB disk; the platform can clamp memory, so verify node limits first.", "Private direct-connect preset: only 8211/UDP is published. Community listing additionally needs 27015/UDP and the allocated PUBLIC_PORT/IP. REST API and RCON are disabled; never publish admin interfaces or enable DEBUG with secrets.", "The image downloads game files on first start. Pin an image digest/version before production use; latest is mutable."],
  },
  {
    name: "Valheim Server", desc: "Valheim dedicated server with persistent worlds/backups (UDP). Requires a password of at least five characters.",
    image: "ghcr.io/community-valheim-tools/valheim-server:latest", port: 2456, protocol: "udp", memory: "4g",
    extraPorts: [{ port: 2457, protocol: "udp", label: "query" }, { port: 2458, protocol: "udp", label: "additional" }],
    volumeMountPath: "/config", tag: "VH", color: "#6c7f86",
    env: { SERVER_NAME: "Valheim", WORLD_NAME: "Dedicated", SERVER_PORT: "2456", SERVER_PASS: "", SERVER_PUBLIC: "false", CROSSPLAY: "false", SUPERVISOR_HTTP: "false", STATUS_HTTP: "false" },
    requiredEnv: [{ key: "SERVER_PASS", minLength: 5 }],
    setupNotes: ["Upstream image is linux/amd64. Worlds and backups persist under /config; optional /opt/valheim download cache is not persisted by this single-volume preset.", "All three UDP ports are seeded from upstream compose. Supervisor/status HTTP are disabled and not published. Check allocated public ports and firewall; adjacent container ports do not guarantee adjacent public ports. The current mesh only forwards the primary port; secondary ports require verified serving-node ingress, not a mesh-only deployment.", "First startup downloads the dedicated server. Upstream recommends a 120-second graceful stop; configure lifecycle/backup policy before production. Pin an image digest/version before upgrades."],
  },
  {
    name: "Arma 3 Server", desc: "Arma 3 dedicated server (UDP 2302–2306). Requires private Steam login and a prepared server config volume.",
    image: "ghcr.io/brettmayson/arma3server/arma3server:v2", port: 2302, protocol: "udp", memory: "4g",
    extraPorts: [2303, 2304, 2305, 2306].map(port => ({ port, protocol: "udp", label: port === 2303 ? "query" : port === 2306 ? "battleye" : "steam" })),
    volumeMountPath: "/arma3/server", tag: "A3", color: "#727b50",
    env: { PORT: "2302", ARMA_CONFIG: "main.cfg", ARMA_PROFILE: "main", STEAM_USER: "", STEAM_PASSWORD: "", HEADLESS_CLIENTS: "0" },
    requiredEnv: [{ key: "STEAM_USER", minLength: 1 }, { key: "STEAM_PASSWORD", minLength: 1 }],
    setupNotes: ["linux/amd64 only. Before starting, populate /arma3/server/configs/main.cfg, missions and required mods in the persistent volume. This catalog does not create those files; an empty volume is not ready to play.", "Upstream requires a Steam account with Steam Guard disabled (game ownership not required according to its README). Do not weaken security on a primary Steam account; use an operator-approved dedicated account. Credentials are masked in this form and protected by server-side sensitive-env handling.", "All five UDP ports are seeded, not HTTP. The current mesh only forwards the primary port. Verify direct serving-node ingress and public allocation/adjacency for every secondary port; a mesh-only deployment is not ready. The v2 image updates server files on restart; image pinning alone does not pin game depots. Upstream has no explicit LICENSE at the audited revision; this preset references its published image without copying code."],
  },
{"name":"ARK: Survival Evolved Server","desc":"ARK: Survival Evolved (ASE), NOT Survival Ascended. Steam dedicated server with three UDP game/query ports.","image":"hermsi/ark-server:latest","port":7777,"protocol":"udp","memory":"8g","volumeMountPath":"/app","tag":"ARK","color":"#427b83","extraPorts":[{"port":7778,"protocol":"udp","label":"game-plus-one"},{"port":27015,"protocol":"udp","label":"query"}],"env":{"SESSION_NAME":"ARK Survival Evolved","SERVER_MAP":"TheIsland","MAX_PLAYERS":"20","SERVER_PASSWORD":"","ADMIN_PASSWORD":"","GAME_CLIENT_PORT":"7777","UDP_SOCKET_PORT":"7778","SERVER_LIST_PORT":"27015","RCON_PORT":"27020","GAME_MOD_IDS":"","SERVER_MAP_MOD_ID":"","UPDATE_ON_START":"false","VALIDATE_ON_START":"false","BACKUP_ON_STOP":"false","PRE_UPDATE_BACKUP":"true","STEAM_LOGIN":"anonymous"},"requiredEnv":[{"key":"SERVER_PASSWORD","minLength":1},{"key":"ADMIN_PASSWORD","minLength":1}],"setupNotes":["linux/amd64 only. First start downloads a very large ASE server. 8g is a request, not verified capacity; verify disk/memory and graceful-stop readiness.","7778/UDP must equal game port +1; 27015/UDP is query. mesh_raw forwards only the primary port. Verify serving-node ingress for all three.","Upstream enables RCON internally on 27020/TCP; this preset NEVER publishes it. Keep administration private.","Persist /app on WSL Linux filesystem, never a Windows-mounted path. OCI pinning does not pin Steam runtime downloads. No Ascended support."]},
];

const GAME_METADATA: Record<string, GamePresetFields> = {
  "Minecraft Server": {"gameId":"minecraft","platform":"Verify chosen image architecture / Java compatibility","imageTags":["latest"],"runtimeVersionNote":"VERSION selects a runtime download channel or exact release. Syntax validation is not release availability verification.","fields":[{"key":"EULA","label":"Mojang EULA acceptance","kind":"select","options":["TRUE","FALSE"]},{"key":"TYPE","label":"Java server software","kind":"select","options":["VANILLA","PAPER"]},{"key":"VERSION","label":"Runtime Minecraft version","kind":"version","help":"LATEST, vanilla SNAPSHOT, or exact upstream release."},{"key":"LEVEL","label":"World directory","kind":"text"},{"key":"MAX_PLAYERS","label":"Maximum players","kind":"integer","min":1,"max":1000},{"key":"DIFFICULTY","label":"Difficulty","kind":"select","options":["peaceful","easy","normal","hard"]},{"key":"MODE","label":"Game mode","kind":"select","options":["survival","creative","adventure","spectator"]}],"profiles":[{"id":"vanilla","label":"Vanilla survival","env":{"TYPE":"VANILLA","VERSION":"LATEST","MODE":"survival"}},{"id":"paper","label":"Paper survival","env":{"TYPE":"PAPER","VERSION":"LATEST","MODE":"survival"}}]},
  "Terraria Server": {"gameId":"terraria","platform":"Verify registry/node platform","imageTags":["terraria-latest"],"runtimeVersionNote":"Vanilla release follows the image. No independent runtime version selector.","fields":[{"key":"WORLDNAME","label":"World name","kind":"text"},{"key":"AUTOCREATE","label":"World size (match existing world)","kind":"select","options":["1","2","3"]},{"key":"DIFFICULTY","label":"Difficulty (new world)","kind":"select","options":["0","1","2","3"]},{"key":"MAXPLAYERS","label":"Maximum players","kind":"integer","min":1,"max":1000},{"key":"PASSWORD","label":"Join password (optional)","kind":"secret"},{"key":"SEED","label":"Seed (new world)","kind":"text"}],"profiles":[{"id":"medium","label":"Existing defaults / medium world","env":{"AUTOCREATE":"2"}},{"id":"small","label":"New small world","env":{"AUTOCREATE":"1"}},{"id":"large","label":"New large world","env":{"AUTOCREATE":"3"}}]},
  "Factorio Server": {"gameId":"factorio","platform":"Verify registry/node platform","imageTags":["stable","latest"],"runtimeVersionNote":"Game binary is bundled in the OCI image. latest may be experimental; stable is a moving channel.","fields":[{"key":"SAVE_NAME","label":"Save name (without .zip)","kind":"text"},{"key":"GENERATE_NEW_SAVE","label":"Generate missing save","kind":"select","options":["false","true"]},{"key":"LOAD_LATEST_SAVE","label":"Load latest save","kind":"select","options":["false","true"]}],"profiles":[{"id":"existing","label":"Load existing saves","env":{"GENERATE_NEW_SAVE":"false","LOAD_LATEST_SAVE":"true"}},{"id":"new","label":"Generate new save","env":{"GENERATE_NEW_SAVE":"true","LOAD_LATEST_SAVE":"false","SAVE_NAME":"world"}}]},
  "Palworld Server": {"gameId":"palworld","platform":"linux/amd64 or linux/arm64","imageTags":["latest"],"runtimeVersionNote":"Steam runtime files download independently of the OCI image; no arbitrary runtime version selector.","fields":[{"key":"SERVER_NAME","label":"Server name","kind":"text"},{"key":"PLAYERS","label":"Maximum players","kind":"integer","min":1,"max":32},{"key":"SERVER_PASSWORD","label":"Join password","kind":"secret"}],"profiles":[{"id":"private","label":"Private direct connect","env":{"COMMUNITY":"false","RCON_ENABLED":"false","REST_API_ENABLED":"false"}}]},
  "Valheim Server": {"gameId":"valheim","platform":"linux/amd64","imageTags":["latest"],"runtimeVersionNote":"Dedicated server downloads independently of OCI version; back up worlds before updates.","fields":[{"key":"SERVER_NAME","label":"Server name","kind":"text"},{"key":"WORLD_NAME","label":"World name","kind":"text"},{"key":"SERVER_PASS","label":"Join password (5+ characters)","kind":"secret"},{"key":"SERVER_PUBLIC","label":"Server browser listing","kind":"select","options":["false","true"]},{"key":"CROSSPLAY","label":"Crossplay","kind":"select","options":["false","true"]}],"profiles":[{"id":"private","label":"Private Steam server","env":{"SERVER_PUBLIC":"false","CROSSPLAY":"false"}},{"id":"crossplay","label":"Private crossplay","env":{"SERVER_PUBLIC":"false","CROSSPLAY":"true"}}]},
  "Arma 3 Server": {"gameId":"arma3","platform":"linux/amd64","imageTags":["v2"],"runtimeVersionNote":"v2 refreshes Steam depots on restart; OCI pinning does not pin runtime game files.","fields":[{"key":"STEAM_USER","label":"Dedicated Steam login","kind":"secret"},{"key":"STEAM_PASSWORD","label":"Steam password","kind":"secret"},{"key":"ARMA_CONFIG","label":"Prepared config filename","kind":"text"},{"key":"ARMA_PROFILE","label":"Profile","kind":"text"},{"key":"ARMA_WORLD","label":"Startup world","kind":"text","help":"Default empty; missions and worlds require prepared configs."},{"key":"HEADLESS_CLIENTS","label":"Headless clients","kind":"integer","min":0,"max":32},{"key":"MODS_PRESET","label":"Arma Launcher HTML preset path","kind":"text","help":"Prepared local preset file, not Workshop ID env support. Player limits/maps/passwords live in prepared config files."}],"profiles":[{"id":"prepared","label":"Prepared-volume dedicated server","env":{"ARMA_CONFIG":"main.cfg","ARMA_PROFILE":"main","HEADLESS_CLIENTS":"0"}}]},
  "ARK: Survival Evolved Server": {"gameId":"ark-ase","platform":"linux/amd64","imageTags":["latest"],"runtimeVersionNote":"ASE Steam files download on first start; UPDATE_ON_START updates independently of OCI tags. NOT Ascended.","fields":[{"key":"SESSION_NAME","label":"Session name","kind":"text"},{"key":"SERVER_MAP","label":"ASE map name","kind":"text","help":"TheIsland is the audited default. Custom maps must be valid installed ASE maps."},{"key":"MAX_PLAYERS","label":"Maximum players","kind":"integer","min":1,"max":1000},{"key":"SERVER_PASSWORD","label":"Join password","kind":"secret"},{"key":"ADMIN_PASSWORD","label":"Private admin / RCON password","kind":"secret"},{"key":"GAME_MOD_IDS","label":"Workshop mod IDs (comma-separated)","kind":"ids"},{"key":"SERVER_MAP_MOD_ID","label":"Custom map Workshop ID (optional)","kind":"ids"},{"key":"UPDATE_ON_START","label":"Update on start","kind":"select","options":["false","true"]},{"key":"VALIDATE_ON_START","label":"Validate when updating","kind":"select","options":["false","true"]},{"key":"BACKUP_ON_STOP","label":"Backup on stop","kind":"select","options":["false","true"]},{"key":"PRE_UPDATE_BACKUP","label":"Backup before update","kind":"select","options":["false","true"]}],"profiles":[{"id":"island","label":"Private TheIsland","env":{"SERVER_MAP":"TheIsland","SERVER_MAP_MOD_ID":"","GAME_MOD_IDS":""}},{"id":"custom","label":"Custom ASE map / mods","env":{}}]},
};
export const GAME_SERVER_PRESETS: GameServerPreset[] = BASE_GAME_PRESETS.map(p => ({ ...p, ...GAME_METADATA[p.name] }));

/** Errors name keys only; never reflect credential values into UI/logs. */
export function gamePresetEnvErrors(template: GamePresetFields, env: Record<string, string>): string[] {
  const errors = (template.requiredEnv ?? []).filter(rule => !env[rule.key]?.trim() || env[rule.key].length < rule.minLength)
    .map(rule => rule.key + " requires at least " + rule.minLength + " character(s).");
  for (const f of template.fields ?? []) {
    const v = env[f.key];
    if (v === undefined || v === "") continue;
    if (f.kind === "integer" && (!/^\d+$/.test(v) || Number(v) < (f.min ?? 0) || Number(v) > (f.max ?? 1000))) errors.push(f.key + " must be a whole number within the form range.");
    if (f.kind === "select" && !f.options?.includes(v)) errors.push(f.key + " requires a supported option.");
    if (f.kind === "ids" && !/^\d+(,\d+)*$/.test(v)) errors.push(f.key + " requires numeric Workshop IDs separated by commas.");
    if (f.key === "SERVER_MAP_MOD_ID" && v.includes(",")) errors.push("SERVER_MAP_MOD_ID accepts one ID.");
    if (f.kind === "version" && !/^(LATEST|SNAPSHOT|\d+\.\d+(\.\d+)?)$/.test(v)) errors.push(f.key + " requires a channel or exact release syntax.");
    if (/[\r\n\x00]/.test(v)) errors.push(f.key + " must be single-line.");
  }
  if (template.gameId === "minecraft" && env.EULA !== "TRUE") errors.push("Mojang EULA acceptance is required.");
  if (template.gameId === "minecraft" && env.TYPE === "PAPER" && env.VERSION === "SNAPSHOT") errors.push("Paper does not use vanilla SNAPSHOT.");
  for (const key of ["RCON_ENABLED", "REST_API_ENABLED", "SUPERVISOR_HTTP", "STATUS_HTTP", "COMMUNITY"]) if (env[key] && env[key] !== "false") errors.push(key + " must remain disabled in this safe profile.");
  if (env.LOG_LEVEL === "DEBUG") errors.push("DEBUG logs can expose credentials; use INFO.");
  for (const [key,value] of Object.entries(env)) if (["__proto__", "prototype", "constructor"].includes(key) || !/^[A-Z][A-Z0-9_]*$/.test(key) || /[\r\n\x00]/.test(value)) errors.push("Environment keys/values must be valid and single-line.");
  return errors;
}
export function isSensitiveGameEnv(key: string): boolean {
  return /(?:PASSWORD|PASS|TOKEN|SECRET|API_KEY|PRIVATE_KEY)/i.test(key) || /^(STEAM_USER|STEAM_LOGIN|BETA_ACCESSCODE|DISCORD_WEBHOOK_URL)$/i.test(key.trim());
}

/** Only the audited repository, not arbitrary registries. Availability is not inferred. */
export function gameImageError(template: { image?: string }, image: string): string | undefined {
  const repository = template.image?.replace(/:[^/:]+$/, "");
  if (!repository || !(image.startsWith(repository + ":") || image.startsWith(repository + "@sha256:"))) return "Use the audited image repository with a tag or digest.";
  const suffix = image.slice(repository.length);
  if (!/^(:[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}|@sha256:[a-f0-9]{64})$/.test(suffix)) return "Use a valid OCI tag or sha256 digest; registry availability is not verified.";
}
export function gameSummaryEnv(env: Record<string, string>): { key: string; value: string }[] {
  return Object.entries(env).map(([key, value]) => ({ key, value: value ? "Configured (value hidden)" : "Not configured" }));
}
