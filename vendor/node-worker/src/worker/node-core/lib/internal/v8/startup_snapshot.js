// startup_snapshot is the V8 snapshot bootstrap. The worker doesn't deal in
// snapshots; this stub keeps the consumers (dns/utils, internal/worker,
// crypto/util, ...) loadable without dragging in the real
// `internal/process/pre_execution.js` chain (which transitively requires
// undici, dns, dgram, child_process, ...).
const noop = () => {};
const isBuildingSnapshot = () => false;
const throwIfBuildingSnapshot = () => {};

const namespace = {
	addDeserializeCallback: noop,
	addSerializeCallback: noop,
	setDeserializeMainFunction: noop,
	isBuildingSnapshot,
};

export const runDeserializeCallbacks = noop;
export { throwIfBuildingSnapshot, namespace };
export const addAfterUserSerializeCallback = noop;

export default {
	runDeserializeCallbacks,
	throwIfBuildingSnapshot,
	namespace,
	addAfterUserSerializeCallback,
};
