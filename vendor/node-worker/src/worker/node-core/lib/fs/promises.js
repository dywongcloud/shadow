// Forward to the merged `promises` object on node/fs's default export. Same
// rationale as ../fs.js: named exports are needed so that bare `require('fs/
// promises').readFile` from inside node_modules/ resolves to the function
// rather than the synthetic namespace wrapper.
import fs from '../../../node/fs';

const promises = fs.promises;

export const constants = promises.constants;
export const glob = promises.glob;

export const appendFile = promises.appendFile;
export const copyFile = promises.copyFile;
export const mkdir = promises.mkdir;
export const opendir = promises.opendir;
export const open = promises.open;
export const readdir = promises.readdir;
export const readFile = promises.readFile;
export const rename = promises.rename;
export const rmdir = promises.rmdir;
export const rm = promises.rm;
export const stat = promises.stat;
export const lstat = promises.lstat;
export const statfs = promises.statfs;
export const writeFile = promises.writeFile;
export const unlink = promises.unlink;

export default promises;
