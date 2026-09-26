import childProcess from '../../node/child_process';

export const ChildProcess = childProcess.ChildProcess;
export const exec = childProcess.exec;
export const execFile = childProcess.execFile;
export const execFileSync = childProcess.execFileSync;
export const execSync = childProcess.execSync;
export const fork = childProcess.fork;
export const spawn = childProcess.spawn;
export const spawnSync = childProcess.spawnSync;

export default childProcess;
