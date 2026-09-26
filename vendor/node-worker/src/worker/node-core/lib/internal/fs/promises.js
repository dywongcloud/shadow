import { promisesToDepromisify } from '../../../../node/fs/promises';
import { FileHandle } from '../../../../node/fs/handle';

export const kRef = Symbol('kRef');
export const kUnref = Symbol('kUnref');

if (!(kRef in FileHandle.prototype)) {
  FileHandle.prototype[kRef] = function noopRef() {};
}

if (!(kUnref in FileHandle.prototype)) {
  FileHandle.prototype[kUnref] = function noopUnref() {};
}

export const exports = {
  ...promisesToDepromisify,
};

export {
  FileHandle,
};

export default {
  exports,
  FileHandle,
  kRef,
  kUnref,
};
