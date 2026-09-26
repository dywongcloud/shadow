export function unsupported(name) {
  return function unsupportedFsFeature() {
    throw new Error(`${name} is not supported in this runtime`);
  };
}

export function unsupportedClass(name) {
  return class UnsupportedFsFeature {
    constructor() {
      throw new Error(`${name} is not supported in this runtime`);
    }
  };
}

export default {
  unsupported,
  unsupportedClass,
};
