const AbortControllerImpl = globalThis.AbortController;
const AbortSignalImpl = globalThis.AbortSignal;

if (!AbortControllerImpl || !AbortSignalImpl) {
  throw new Error('AbortController is required in the runtime');
}

if (typeof AbortSignalImpl.any !== 'function') {
  AbortSignalImpl.any = function any(signals) {
    const controller = new AbortControllerImpl();
    const onAbort = (event) => {
      controller.abort(event?.target?.reason);
      cleanup();
    };
    const cleanup = () => {
      for (const signal of signals) {
        signal.removeEventListener?.('abort', onAbort);
      }
    };

    for (const signal of signals) {
      if (!signal) {
        continue;
      }
      if (signal.aborted) {
        controller.abort(signal.reason);
        return controller.signal;
      }
      signal.addEventListener?.('abort', onAbort, { once: true });
    }

    return controller.signal;
  };
}

if (typeof AbortSignalImpl.timeout !== 'function') {
  AbortSignalImpl.timeout = function timeout(delay) {
    const controller = new AbortControllerImpl();
    setTimeout(() => controller.abort(new Error('TimeoutError')), delay);
    return controller.signal;
  };
}

const AbortController = AbortControllerImpl;
const AbortSignal = AbortSignalImpl;

export { AbortController, AbortSignal };

export default {
  AbortController,
  AbortSignal,
};
