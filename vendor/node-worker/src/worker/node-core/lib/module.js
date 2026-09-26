const createRequireSymbol = Symbol.for('puter.node-worker.createRequire');

export function createRequire(filename) {
  const createRequireImpl = globalThis[createRequireSymbol];
  if (typeof createRequireImpl !== 'function') {
    throw new Error('createRequire is not initialized');
  }

  const normalized = typeof filename === 'string' ? filename : String(filename);
  const slash = Math.max(normalized.lastIndexOf('/'), normalized.lastIndexOf('\\'));
  const basedir = slash === -1 ? '.' : normalized.slice(0, slash);
  return createRequireImpl(basedir);
}

export default {
  createRequire,
};
