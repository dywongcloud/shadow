const kClone = Symbol('messaging_clone_symbol');
const kDeserialize = Symbol('messaging_deserialize_symbol');
const kTransfer = Symbol('messaging_transfer_symbol');
const kTransferList = Symbol('messaging_transfer_list_symbol');

function markTransferMode(_obj, _cloneable = false, _transferable = false) {
  // no-op: cross-worker transfer of these host objects is unsupported here.
}

function setup() {}

function structuredClone(value, options) {
  return globalThis.structuredClone(value, options);
}

export { markTransferMode, setup, structuredClone, kClone, kDeserialize, kTransfer, kTransferList };

export default {
  markTransferMode,
  setup,
  structuredClone,
  kClone,
  kDeserialize,
  kTransfer,
  kTransferList,
};
