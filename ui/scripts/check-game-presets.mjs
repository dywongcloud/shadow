import { readFileSync } from 'node:fs';
import { GAME_SERVER_PRESETS, gamePresetEnvErrors, isSensitiveGameEnv, gameImageError, gameSummaryEnv } from '../lib/game-server-presets.ts';
const page=readFileSync(new URL('../app/new/page.tsx',import.meta.url),'utf8');
const form=readFileSync(new URL('../components/game-hosting.tsx',import.meta.url),'utf8');
const settings=readFileSync(new URL('../../crates/hive-cloud/src/project_settings.rs',import.meta.url),'utf8');
let checks=0;
function check(ok,message){checks++;if(!ok)throw new Error(message);}
const expected={minecraft:[25565,'tcp','/data',[]],terraria:[7777,'tcp','/opt/terraria/config',[]],factorio:[34197,'udp','/factorio',[]],palworld:[8211,'udp','/palworld',[]],valheim:[2456,'udp','/config',[2457,2458]],arma3:[2302,'udp','/arma3/server',[2303,2304,2305,2306]],'ark-ase':[7777,'udp','/app',[7778,27015]]};
check(GAME_SERVER_PRESETS.length===7,'seven identities required');
check(new Set(GAME_SERVER_PRESETS.map(p=>p.gameId)).size===7,'duplicate identities');
for(const t of GAME_SERVER_PRESETS){
 const [port,protocol,path,extras]=expected[t.gameId];
 check(t.port===port && t.protocol===protocol && t.volumeMountPath===path,t.name+' wrong transport/persistence');
 check(JSON.stringify((t.extraPorts??[]).map(p=>p.port))===JSON.stringify(extras),t.name+' wrong extra ports');
 check(t.fields?.length>0 && t.profiles?.length>0 && t.imageTags?.length>0,t.name+' missing configure metadata');
 check(new Set(t.fields.map(f=>f.key)).size===t.fields.length,'duplicate fields');
 check(gameImageError(t,t.image)===undefined,'valid default image rejected');
 check(gameImageError(t,t.image.replace(/:[^/:]+$/,'@sha256:'+'a'.repeat(64)))===undefined,'valid digest rejected');
 for(const image of ['',t.image+':bad','other/repo:latest',t.image+'\n']) check(!!gameImageError(t,image),'bad image accepted');
 check(gamePresetEnvErrors(t,t.env).length===t.requiredEnv.length,t.name+' default env regression');
 const env={...t.env};
 for(const rule of t.requiredEnv){check(env[rule.key]==='','credential default not empty');check(isSensitiveGameEnv(rule.key),'credential masking missing');env[rule.key]='sentinel-'+ 'x'.repeat(rule.minLength);}
 check(!gamePresetEnvErrors(t,env).length,'valid env rejected');
 for(const rule of t.requiredEnv){const missing={...env};delete missing[rule.key];check(gamePresetEnvErrors(t,missing).length>0,'missing credential accepted');check(gamePresetEnvErrors(t,{...env,[rule.key]:' '.repeat(rule.minLength)}).length>0,'whitespace credential accepted');if(rule.minLength>1)check(gamePresetEnvErrors(t,{...env,[rule.key]:'x'.repeat(rule.minLength-1)}).length>0,'short credential accepted');}
 for(const f of t.fields){
  if(f.kind==='secret'){check(isSensitiveGameEnv(f.key),'typed secret unmasked');check(settings.includes('"'+f.key+'"'),'backend policy missing '+f.key);}
  if(f.kind==='integer')for(const value of ['-1','1.5','NaN',String(f.max+1)])check(gamePresetEnvErrors(t,{...env,[f.key]:value}).length>0,'bad integer accepted');
  if(f.kind==='select')check(gamePresetEnvErrors(t,{...env,[f.key]:'fabricated-option'}).length>0,'bad select accepted');
  if(f.kind==='ids')for(const value of ['abc','1,,2','-1'])check(gamePresetEnvErrors(t,{...env,[f.key]:value}).length>0,'bad mods accepted');
 }
 for(const p of t.profiles)check(!gamePresetEnvErrors(t,{...env,...p.env}).length,'bad profile '+p.id);
 check(!JSON.stringify(gameSummaryEnv({...env,CUSTOM_KEY:'sentinel-secret'})).includes('sentinel'),'summary leaks values');
 check(!gamePresetEnvErrors(t,{...env,CUSTOM_KEY:'sentinel'}).join(' ').includes('sentinel'),'error leaks values');
}
const mc=GAME_SERVER_PRESETS.find(t=>t.gameId==='minecraft');
for(const VERSION of ['latest','fake-release','1.2.3.4','1.2;echo'])check(gamePresetEnvErrors(mc,{...mc.env,VERSION}).length>0,'bad runtime version accepted');
check(gamePresetEnvErrors(mc,{...mc.env,TYPE:'PAPER',VERSION:'SNAPSHOT'}).length>0,'Paper snapshot accepted');
check(gamePresetEnvErrors(mc,{}).length>0,'missing EULA accepted');
const ark=GAME_SERVER_PRESETS.find(t=>t.gameId==='ark-ase');
check(ark.name.includes('Survival Evolved') && ark.env.STEAM_LOGIN==='anonymous','ARK wrong identity/login');
check(!ark.extraPorts.some(p=>p.port===27020),'RCON publicly exposed');
check(ark.env.UDP_SOCKET_PORT===String(ark.port+1),'ARK adjacent port mismatch');
for(const key of ['PASSWORD','STEAM_LOGIN','BETA_ACCESSCODE','DISCORD_WEBHOOK_URL'])check(settings.includes('"'+key+'"')&&isSensitiveGameEnv(key),'secret policy missing '+key);
check(page.includes('<GamesMenu')&&page.includes('<ConfigureGame')&&page.includes('TEMPLATES.filter(t => !t.gameId)'),'Games grouping not wired');
check(page.includes('volume_mount_path: opts.volumeMountPath')&&page.includes('image: opts.image'),'payload image/volume missing');
for(const fragment of ['aria-label="Games"','htmlFor={"game-" + f.key}','id={"game-" + f.key}','Back to Games','Back to configuration','Review configuration','stage !== "review"','!game.extraPorts?.length || ingress','gameSummaryEnv(env)','type={f.kind === "secret" ? "password"','volumeMountPath: game.volumeMountPath','port: game.port','env, volumeMountPath','raw server errors are not shown'])check(form.includes(fragment),'form source guard missing '+fragment);
check(settings.includes('game_secret || looks_like_secret(&value.value)'),'encrypted policy missing');
console.log('PASS '+checks+' offline catalog/schema/source checks; all seven games. No browser, registry or game-runtime proof.');
