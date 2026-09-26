import nodePath from '../../../../../node/path';
import { unsupported } from '../_unsupported.js';

export function isSrcSubdir(src, dest) {
  const from = nodePath.resolve(String(src));
  const to = nodePath.resolve(String(dest));
  return to.startsWith(from.endsWith(nodePath.sep) ? from : `${from}${nodePath.sep}`);
}

export const cpFn = unsupported('internal/fs/cp/cp.cpFn');

export default {
  cpFn,
  isSrcSubdir,
};
