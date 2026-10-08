import { readFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { GAME_SERVER_PRESETS, gamePresetEnvErrors, isSensitiveGameEnv } from '../lib/game-server-presets.ts';
// Offline catalog/schema checks only. Does not pull images or start game servers.
const page = readFileSync(new URL('../app/new/page.tsx', import.meta.url), 'utf8');
let source = page.slice(page.indexOf('const TEMPLATES: Template[] = [') + 'const TEMPLATES: Template[] = '.length);
source = source.slice(0, source.indexOf('];') + 2);
const templates = Function('GAME_SERVER_PRESETS', 'return ' + source)(GAME_SERVER_PRESETS);
let checks = 0;
function check(ok, message) { checks++; if (!ok) throw new Error(message); }
const expected = {
 'Terraria Server': [7777, 'tcp', '/opt/terraria/config', []],
 'Factorio Server': [34197, 'udp', '/factorio', []],
 'Palworld Server': [8211, 'udp', '/palworld', []],
 'Valheim Server': [2456, 'udp', '/config', [2457,2458]],
 'Arma 3 Server': [2302, 'udp', '/arma3/server', [2303,2304,2305,2306]],
};
check(new Set(templates.map(t => t.name)).size === templates.length, 'duplicate template names');
check(new Set(templates.filter(t=>t.image).map(t=>t.image)).size === templates.filter(t=>t.image).length, 'duplicate image presets');
for (const [name, [port, protocol, path, extras]] of Object.entries(expected)) {
 const t = templates.find(t => t.name === name);
 check(!!t, name + ' missing');
 check(t.port === port && t.protocol === protocol && !t.repo, name + ' wrong transport/engine');
 check(t.volumeMountPath === path, name + ' wrong persistence');
 check(JSON.stringify((t.extraPorts ?? []).map(p=>p.port)) === JSON.stringify(extras), name + ' missing extra ports');
 check((t.extraPorts ?? []).every(p=>p.protocol === 'udp'), name + ' unexpected extra protocol');
 check(new Set([t.port, ...(t.extraPorts ?? []).map(p=>p.port)]).size === 1+extras.length, name + ' duplicate ports');
}
for (const t of GAME_SERVER_PRESETS) {
 check(gamePresetEnvErrors(t,t.env).length === t.requiredEnv.length, t.name+' insecure defaults');
 const env={...t.env};
 for(const rule of t.requiredEnv) {
  check(env[rule.key] === '', 'hardcoded credential '+rule.key);
  check(isSensitiveGameEnv(rule.key), 'unmasked credential '+rule.key);
  env[rule.key]='x'.repeat(rule.minLength);
 }
 check(gamePresetEnvErrors(t,env).length === 0, t.name+' valid env rejected');
 for(const rule of t.requiredEnv) {
  const missing={...env}; delete missing[rule.key];
  check(gamePresetEnvErrors(t,missing).length>0, rule.key+' removal bypass');
  check(gamePresetEnvErrors(t,{...env,[rule.key]:' '.repeat(rule.minLength)}).length>0, rule.key+' whitespace bypass');
  if(rule.minLength>1) check(gamePresetEnvErrors(t,{...env,[rule.key]:'x'.repeat(rule.minLength-1)}).length>0, rule.key+' short value bypass');
 }
}
const pal=GAME_SERVER_PRESETS.find(t=>t.name==='Palworld Server');
check(pal.env.RCON_ENABLED==='false' && pal.env.REST_API_ENABLED==='false' && pal.env.LOG_LEVEL==='INFO', 'Palworld admin/log exposure');
const val=GAME_SERVER_PRESETS.find(t=>t.name==='Valheim Server');
check(val.env.SUPERVISOR_HTTP==='false' && val.env.STATUS_HTTP==='false', 'Valheim admin exposure');
check(page.includes('volume_mount_path: opts.volumeMountPath') && page.includes('volumeMountPath: template.volumeMountPath'), 'volume path not sent');
check(page.includes('(template.extraPorts ?? []).map') && page.includes('container_port: p'), 'multiport defaults not wired');
check(page.includes('disabled={deploying || envErrors.length > 0}') && page.includes('if (gamePresetEnvErrors(template, buildEnv()).length) return;'), 'required env not guarded');
check(page.includes('type={isSensitiveGameEnv(row.key) ? "password" : "text"}'), 'secret input not masked');
const other=templates.filter(t=>!Object.hasOwn(expected,t.name));
check(createHash('sha256').update(JSON.stringify(other)).digest('hex') === '9be223ff660e97a712067f6e72ad352598c657b1b344752ed3d759ab7b680a08', 'Minecraft/other starter metadata changed; review intentionally before updating fingerprint');
const settings=readFileSync(new URL('../../crates/hive-cloud/src/project_settings.rs',import.meta.url),'utf8');
check(settings.includes('game_secret || looks_like_secret(&value.value)'), 'server credential storage protection missing');
for(const key of ['STEAM_USER','STEAM_PASSWORD','SERVER_PASS','SERVER_PASSWORD']) check(settings.includes('"'+key+'"'), 'server secret key missing '+key);
const admin=readFileSync(new URL('../../crates/hive-cloud/src/admin.rs',import.meta.url),'utf8');
check(admin.includes('volume_mount_path: Option<String>') && admin.includes('image_volume_path: body.volume_mount_path'), 'backend persistence request path missing');
console.log('Game preset catalog/schema regression checks passed: '+checks+'; five presets plus unchanged Minecraft/other starters. No game runtime proof.');
