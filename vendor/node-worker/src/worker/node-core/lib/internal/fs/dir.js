import { promisesToDepromisify } from '../../../../node/fs/promises';

export const opendir = promisesToDepromisify.opendir;

export default {
  opendir,
};
