// Runtime override. Upstream `internal/worker` is the worker_threads subsystem
// (MessagePort, v8 serialization via error_serdes, the esm loader hooks, ...) —
// a massive subtree we don't support. `internal/crypto` reaches it only through
// `crypto.setFips()` -> `require('internal/worker').ownsProcessState`, and
// `internal/process/per_thread` reads a few main-thread flags. We run as the
// owning main thread, so provide those statically and stub the Worker classes.

const kIsOnline = Symbol('kIsOnline');
const SHARE_ENV = Symbol.for('nodejs.worker_threads.SHARE_ENV');

const ownsProcessState = true;
const isMainThread = true;
const isInternalThread = false;
const threadId = 0;
const threadName = 'MainThread';
const resourceLimits = {};

function setEnvironmentData(_key, _value) {}
function getEnvironmentData(_key) {
  return undefined;
}
function assignEnvironmentData(_data) {}

class Worker {
  constructor() {
    throw new Error('worker_threads is not supported in this runtime');
  }
}
class InternalWorker {
  constructor() {
    throw new Error('worker_threads is not supported in this runtime');
  }
}

export {
  ownsProcessState,
  kIsOnline,
  isMainThread,
  isInternalThread,
  SHARE_ENV,
  resourceLimits,
  setEnvironmentData,
  getEnvironmentData,
  assignEnvironmentData,
  threadId,
  threadName,
  InternalWorker,
  Worker,
};

export default {
  ownsProcessState,
  kIsOnline,
  isMainThread,
  isInternalThread,
  SHARE_ENV,
  resourceLimits,
  setEnvironmentData,
  getEnvironmentData,
  assignEnvironmentData,
  threadId,
  threadName,
  InternalWorker,
  Worker,
};
