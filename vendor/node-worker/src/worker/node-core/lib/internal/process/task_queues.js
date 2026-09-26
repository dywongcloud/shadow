function queueMicrotaskShim(callback) {
  return queueMicrotask(callback);
}

export { queueMicrotaskShim as queueMicrotask };

export default {
  queueMicrotask: queueMicrotaskShim,
};
