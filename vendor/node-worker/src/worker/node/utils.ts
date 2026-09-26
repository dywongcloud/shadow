import nodeBuffer from "./buffer";
import nodeStream from "./stream";

export function streamToBuffer(
	stream: InstanceType<typeof nodeStream.Readable>
): Promise<Buffer> {
	return new Promise((res, rej) => {
		let buf = nodeBuffer.Buffer.alloc(0);
		stream.on("data", (data) => {
			buf = nodeBuffer.Buffer.concat([buf, data]);
		});
		stream.on("end", () => {
			res(buf);
		});
		stream.on("error", (e) => rej(e));
	});
}

type Promisified = (...args: any[]) => Promise<any>;
// The output type uses `(...args: any[]) => void` for the callable, which
// is assignable to every node `fs.X` overload set. Trying to mirror node's
// overloads in a single generated signature doesn't work — node's callback
// fs is heavily overloaded (`copyFile(src, dest, cb) | copyFile(src, dest,
// mode, cb)`, similar for rm/mkdir/readdir/...), and a single
// `(args..., cb)` signature can't satisfy two-overload positional
// alternatives. The runtime impl pops the last arg as the callback anyway,
// so internal types are already `any[]`.
//
// Each function carries a `__promisify__` namespace pointing back at the
// original promise version — that's required by node's namespace-merged
// fs typings (`function copyFile(...)` + `namespace copyFile { function
// __promisify__(...) }`). Without it the deep `satisfies` check silently
// falls back to the elision and hides real divergences underneath.
type Depromisified<T extends Promisified> = ((...args: any[]) => void) & {
	// `(...args: any[]) => Promise<any>` for the same reason as the call
	// signature: node's `fs.X.__promisify__` predates `node:fs/promises`
	// and accepts file descriptors / returns numbers, while our promise
	// impl uses `PathLike | FileHandle` / returns `FileHandle`. The widest
	// assignable shape is the bottom callable. The runtime value carries
	// the actual `T` so anyone that reaches through `__promisify__` sees
	// our real signature.
	__promisify__: (...args: any[]) => Promise<any>;
};
type DepromisifiedObject<T extends Record<string, Promisified>> = {
	[K in keyof T]: Depromisified<T[K]>;
};

export function depromisify<T extends Record<string, Promisified>>(
	obj: T
): DepromisifiedObject<T> {
	return Object.fromEntries(
		Object.entries(obj).map(([k, v]) => {
			// Bind to the source object so methods that reach siblings via `this`
			// (e.g. appendFile -> this.readFile) keep working in callback form.
			const bound = (v as Function).bind(obj) as (
				...args: any[]
			) => Promise<any>;
			const cb = (...args: any[]) => {
				let cb = args.pop();
				bound(...args)
					.then((r) => cb(null, r))
					.catch((e) => cb(e));
			};
			(cb as any).__promisify__ = bound;
			return [k, cb];
		})
	) as any;
}
