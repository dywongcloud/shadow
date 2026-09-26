import { unsupported } from './_unsupported.js';

export const rimraf = unsupported('internal/fs/rimraf.rimraf');
export const rimrafPromises = unsupported('internal/fs/rimraf.rimrafPromises');

export default {
  rimraf,
  rimrafPromises,
};
