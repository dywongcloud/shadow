/** Image references only: upstream code/licenses remain with their authors.
 * Pinned source audit and operational prerequisites: docs/game-server-presets.md.
 */
export type GamePresetFields = {
  extraPorts?: { port: number; protocol: string; label: string }[];
  requiredEnv?: { key: string; minLength: number }[];
  setupNotes?: string[];
};
export type GameServerPreset = GamePresetFields & {
  name: string; desc: string; image: string; port: number; protocol: string;
  memory: string; volumeMountPath: string; tag: string; color: string;
  env: Record<string, string>; requiredEnv: { key: string; minLength: number }[]; setupNotes: string[];
};
export const GAME_SERVER_PRESETS: GameServerPreset[] = [
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
];

/** Errors name keys only; never reflect credential values into UI/logs. */
export function gamePresetEnvErrors(template: GamePresetFields, env: Record<string, string>): string[] {
  return (template.requiredEnv ?? []).filter(rule => !env[rule.key]?.trim() || env[rule.key].length < rule.minLength)
    .map(rule => rule.key + " requires at least " + rule.minLength + " character(s).");
}
export function isSensitiveGameEnv(key: string): boolean {
  return /(?:PASSWORD|PASS|TOKEN|SECRET|API_KEY|PRIVATE_KEY)/i.test(key) || key.trim().toUpperCase() === "STEAM_USER";
}
