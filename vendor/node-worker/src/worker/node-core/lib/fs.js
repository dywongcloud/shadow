// Forward to the impl in src/worker/node/fs/index.ts. Named exports mirror
// the keys @rollup/plugin-commonjs needs so that bare `require('fs').statSync`
// from inside node_modules/ (e.g. the `resolve` package) hits the function
// directly rather than the synthetic namespace wrapper.
import fs from '../../node/fs';

// Classes / containers
export const Dir = fs.Dir;
export const Dirent = fs.Dirent;
export const Stats = fs.Stats;
export const StatsFs = fs.StatsFs;
export const constants = fs.constants;
export const promises = fs.promises;
export const glob = fs.glob;

// Sync methods
export const appendFileSync = fs.appendFileSync;
export const copyFileSync = fs.copyFileSync;
export const existsSync = fs.existsSync;
export const mkdirSync = fs.mkdirSync;
export const opendirSync = fs.opendirSync;
export const readdirSync = fs.readdirSync;
export const readFileSync = fs.readFileSync;
export const renameSync = fs.renameSync;
export const rmdirSync = fs.rmdirSync;
export const rmSync = fs.rmSync;
export const statSync = fs.statSync;
export const lstatSync = fs.lstatSync;
export const globSync = fs.globSync;
export const statfsSync = fs.statfsSync;
export const writeFileSync = fs.writeFileSync;
export const unlinkSync = fs.unlinkSync;

// Callback-style async methods (depromisified)
export const appendFile = fs.appendFile;
export const copyFile = fs.copyFile;
export const mkdir = fs.mkdir;
export const opendir = fs.opendir;
export const open = fs.open;
export const readdir = fs.readdir;
export const readFile = fs.readFile;
export const rename = fs.rename;
export const rmdir = fs.rmdir;
export const rm = fs.rm;
export const stat = fs.stat;
export const lstat = fs.lstat;
export const statfs = fs.statfs;
export const writeFile = fs.writeFile;
export const unlink = fs.unlink;

export default fs;
