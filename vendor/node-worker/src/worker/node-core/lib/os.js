import os from '../../node/os';

export const EOL = os.EOL;
export const constants = os.constants;
export const devNull = os.devNull;
export const platform = os.platform;
export const type = os.type;
export const release = os.release;
export const version = os.version;
export const arch = os.arch;
export const endianness = os.endianness;
export const hostname = os.hostname;
export const homedir = os.homedir;
export const tmpdir = os.tmpdir;
export const uptime = os.uptime;
export const freemem = os.freemem;
export const totalmem = os.totalmem;
export const loadavg = os.loadavg;
export const cpus = os.cpus;
export const availableParallelism = os.availableParallelism;
export const networkInterfaces = os.networkInterfaces;
export const userInfo = os.userInfo;
export const machine = os.machine;
export const getPriority = os.getPriority;
export const setPriority = os.setPriority;

export default os;
