// Promise/AbortSignal-aware wrappers over ./timers. Keeps keepalive
// integration intact: the Timeout/Immediate primitives in ./timers ref by
// default and only contribute to the worker's ref count while refed.

import {
	setTimeoutWrap,
	setIntervalWrap,
	setImmediateWrap,
	clearTimeoutWrap,
	clearIntervalWrap,
	clearImmediateWrap,
} from "./timers";

type Options = { signal?: AbortSignal; ref?: boolean };

function abortError(signal?: AbortSignal): Error {
	const reason = signal?.reason;
	if (reason instanceof Error) return reason;
	const err = new Error("The operation was aborted") as Error & {
		code: string;
		name: string;
	};
	err.name = "AbortError";
	err.code = "ABORT_ERR";
	return err;
}

function setTimeoutPromise<T = void>(
	delay?: number,
	value?: T,
	options: Options = {}
): Promise<T> {
	if (options.signal?.aborted) return Promise.reject(abortError(options.signal));

	return new Promise<T>((resolve, reject) => {
		let onAbort: (() => void) | undefined;
		let timer = setTimeoutWrap(() => {
			if (onAbort && options.signal)
				options.signal.removeEventListener("abort", onAbort);
			resolve(value as T);
		}, delay ?? 0);

		if (options.ref === false) timer.unref();

		if (options.signal) {
			onAbort = () => {
				clearTimeoutWrap(timer);
				reject(abortError(options.signal));
			};
			options.signal.addEventListener("abort", onAbort, { once: true });
		}
	});
}

function setImmediatePromise<T = void>(
	value?: T,
	options: Options = {}
): Promise<T> {
	if (options.signal?.aborted) return Promise.reject(abortError(options.signal));

	return new Promise<T>((resolve, reject) => {
		let onAbort: (() => void) | undefined;
		let timer = setImmediateWrap(() => {
			if (onAbort && options.signal)
				options.signal.removeEventListener("abort", onAbort);
			resolve(value as T);
		});

		if (options.ref === false) timer.unref();

		if (options.signal) {
			onAbort = () => {
				clearImmediateWrap(timer);
				reject(abortError(options.signal));
			};
			options.signal.addEventListener("abort", onAbort, { once: true });
		}
	});
}

async function* setIntervalPromise<T = void>(
	delay?: number,
	value?: T,
	options: Options = {}
): AsyncGenerator<T> {
	if (options.signal?.aborted) throw abortError(options.signal);

	let pending: { resolve: () => void; reject: (e: Error) => void } | undefined;
	let queue = 0;
	let aborted = false;

	const onAbort = () => {
		aborted = true;
		if (pending) {
			let p = pending;
			pending = undefined;
			p.reject(abortError(options.signal));
		}
	};

	let timer = setIntervalWrap(() => {
		queue++;
		if (pending) {
			let p = pending;
			pending = undefined;
			p.resolve();
		}
	}, delay ?? 0);

	if (options.ref === false) timer.unref();
	if (options.signal) options.signal.addEventListener("abort", onAbort, { once: true });

	try {
		while (!aborted) {
			if (queue === 0) {
				await new Promise<void>((resolve, reject) => {
					pending = { resolve, reject };
				});
			}
			while (queue > 0 && !aborted) {
				queue--;
				yield value as T;
			}
		}
		throw abortError(options.signal);
	} finally {
		clearIntervalWrap(timer);
		if (options.signal) options.signal.removeEventListener("abort", onAbort);
	}
}

class Scheduler {
	wait(delay?: number, options?: Options): Promise<void> {
		return setTimeoutPromise<void>(delay, undefined, options);
	}
	yield(): Promise<void> {
		return setImmediatePromise<void>(undefined);
	}
}

const scheduler = new Scheduler();

const timersPromises = {
	setTimeout: setTimeoutPromise,
	setImmediate: setImmediatePromise,
	setInterval: setIntervalPromise,
	scheduler,
};

export default timersPromises as unknown as typeof import("node:timers/promises");
