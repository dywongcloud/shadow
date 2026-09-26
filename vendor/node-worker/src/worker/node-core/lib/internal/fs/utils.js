import nodePath from '../../../../node/path';
import { Dirent, Stats, StatsFs } from '../../../../node/fs/classes';
import { fsConstants, normalizeFsEntry } from '../../../../node/fs/util';

const kReadFileUnknownBufferLength = 64 * 1024;
const kReadFileBufferLength = 512 * 1024;
const kWriteFileMaxChunkSize = 512 * 1024;

export const constants = {
  kIoMaxLength: 2 ** 31 - 1,
  kMaxUserId: 2 ** 32 - 1,
  kReadFileBufferLength,
  kReadFileUnknownBufferLength,
  kWriteFileMaxChunkSize,
};

export function assertEncoding(encoding) {
  if (encoding && !Buffer.isEncoding(encoding)) {
    throw new TypeError(`Unknown encoding: ${encoding}`);
  }
}

export function copyObject(source) {
  return { ...source };
}

export class DirentFromStats extends Dirent {
  #stats;

  constructor(name, stats, path) {
    super(name, {
      isDir: stats?.isDirectory?.() ?? false,
      isSymlink: stats?.isSymbolicLink?.() ?? false,
      path: typeof path === 'string' ? nodePath.join(path, String(name)) : String(name),
    });
    this.#stats = stats;
  }

  isBlockDevice() {
    return this.#stats?.isBlockDevice?.() ?? false;
  }

  isCharacterDevice() {
    return this.#stats?.isCharacterDevice?.() ?? false;
  }

  isFIFO() {
    return this.#stats?.isFIFO?.() ?? false;
  }

  isSocket() {
    return this.#stats?.isSocket?.() ?? false;
  }
}

export function getDirent(path, name, type, callback) {
  const dirent = new Dirent(name, {
    isDir: type === 'dir',
    isSymlink: type === 'symlink',
    path: typeof path === 'string' ? nodePath.join(path, String(name)) : String(name),
  });
  if (typeof callback === 'function') {
    callback(null, dirent);
    return;
  }
  return dirent;
}

export function getDirents(path, entries, callback) {
  const names = entries?.[0] ?? [];
  const types = entries?.[1] ?? [];
  const dirents = names.map((name, index) => getDirent(path, name, types[index]));
  if (typeof callback === 'function') {
    callback(null, dirents);
    return;
  }
  return dirents;
}

export function getOptions(options, defaultOptions = {}) {
  if (options == null) {
    return { ...defaultOptions };
  }
  if (typeof options === 'string') {
    return { ...defaultOptions, encoding: options };
  }
  return { ...defaultOptions, ...options };
}

export function getValidatedFd(fd) {
  if (!Number.isInteger(fd) || fd < 0) {
    throw new TypeError('fd must be a non-negative integer');
  }
  return fd;
}

export function validatePath(path, propName = 'path') {
  if (typeof path === 'string' || path instanceof Buffer || path instanceof URL) {
    return path;
  }
  throw new TypeError(`${propName} must be a string, Buffer, or URL`);
}

export function getValidatedPath(path, propName = 'path') {
  return validatePath(path, propName);
}

export function getStatsFromBinding(stats) {
  return new Stats(normalizeFsEntry(stats), false);
}

export function getStatFsFromBinding(stats) {
  return new StatsFs(stats, false);
}

export function handleErrorFromBinding(error) {
  throw error;
}

export function preprocessSymlinkDestination(path) {
  return path;
}

export const realpathCacheKey = Symbol('realpathCacheKey');

export function stringToFlags(flags, name = 'flags') {
  if (typeof flags === 'number') return flags;

  const table = {
    r: fsConstants.O_RDONLY,
    'r+': fsConstants.O_RDWR,
    w: fsConstants.O_TRUNC | fsConstants.O_CREAT | fsConstants.O_WRONLY,
    'w+': fsConstants.O_TRUNC | fsConstants.O_CREAT | fsConstants.O_RDWR,
    a: fsConstants.O_APPEND | fsConstants.O_CREAT | fsConstants.O_WRONLY,
    'a+': fsConstants.O_APPEND | fsConstants.O_CREAT | fsConstants.O_RDWR,
    wx: fsConstants.O_TRUNC | fsConstants.O_CREAT | fsConstants.O_WRONLY | fsConstants.O_EXCL,
    'wx+': fsConstants.O_TRUNC | fsConstants.O_CREAT | fsConstants.O_RDWR | fsConstants.O_EXCL,
    ax: fsConstants.O_APPEND | fsConstants.O_CREAT | fsConstants.O_WRONLY | fsConstants.O_EXCL,
    'ax+': fsConstants.O_APPEND | fsConstants.O_CREAT | fsConstants.O_RDWR | fsConstants.O_EXCL,
  };

  if (typeof flags === 'string' && flags in table) {
    return table[flags];
  }

  throw new TypeError(`${name} is invalid`);
}

export function stringToSymlinkType(type) {
  return type;
}

export function toUnixTimestamp(time) {
  if (time instanceof Date) return time.getTime() / 1000;
  const numeric = Number(time);
  if (!Number.isFinite(numeric)) {
    throw new TypeError('time must be a finite number or Date');
  }
  return numeric;
}

export function validateBufferArray(buffers, propName = 'buffers') {
  if (!Array.isArray(buffers) || !buffers.every(ArrayBuffer.isView)) {
    throw new TypeError(`${propName} must be an array of ArrayBufferView values`);
  }
  return buffers;
}

export function validateCpOptions(options = {}) {
  return { ...options };
}

export function validateOffsetLengthRead(offset, length, bufferLength) {
  if (!Number.isInteger(offset) || offset < 0) throw new RangeError('offset is out of range');
  if (!Number.isInteger(length) || length < 0) throw new RangeError('length is out of range');
  if (offset + length > bufferLength) throw new RangeError('offset + length is out of range');
}

export const validateOffsetLengthWrite = validateOffsetLengthRead;

export function validatePosition(position) {
  if (position == null) return position;
  if (!Number.isInteger(position) || position < -1) {
    throw new RangeError('position is out of range');
  }
  return position;
}

export function validateRmOptions(_path, options = {}, _expectDir, cb) {
  const normalized = { force: false, recursive: false, ...options };
  if (typeof cb === 'function') {
    cb(null, normalized);
    return;
  }
  return normalized;
}

export function validateRmOptionsSync(_path, options = {}) {
  return { force: false, recursive: false, ...options };
}

export function validateRmdirOptions(options = {}) {
  return { recursive: false, ...options };
}

export function validateStringAfterArrayBufferView(value, name = 'value') {
  if (typeof value !== 'string' && !ArrayBuffer.isView(value)) {
    throw new TypeError(`${name} must be a string or ArrayBufferView`);
  }
  return value;
}

export function warnOnNonPortableTemplate() {}

export default {
  constants,
  assertEncoding,
  copyObject,
  Dirent,
  DirentFromStats,
  getDirent,
  getDirents,
  getOptions,
  getValidatedFd,
  getValidatedPath,
  getStatFsFromBinding,
  getStatsFromBinding,
  handleErrorFromBinding,
  preprocessSymlinkDestination,
  realpathCacheKey,
  stringToFlags,
  stringToSymlinkType,
  Stats,
  toUnixTimestamp,
  validateBufferArray,
  validateCpOptions,
  validateOffsetLengthRead,
  validateOffsetLengthWrite,
  validatePath,
  validatePosition,
  validateRmOptions,
  validateRmOptionsSync,
  validateRmdirOptions,
  validateStringAfterArrayBufferView,
  warnOnNonPortableTemplate,
};
