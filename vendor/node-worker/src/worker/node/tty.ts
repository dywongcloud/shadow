// `node:tty`.
//
// The consumers are the supports-color / is-interactive / cli-cursor family. They want three things:
// `isatty(fd)`, `new tty.WriteStream(fd)`, and `WriteStream.prototype.hasColors`.
//
// Nothing here reimplements a stream. ./console.ts already builds stdout/stderr/stdin with `isTTY`,
// `columns`, `rows`, `getColorDepth`, `hasColors`, `clearLine`, `cursorTo` and `moveCursor` — driven
// by the real terminal size the host reports — so the constructors hand those back. A constructor
// returning an existing object is legitimate: `new` yields the returned object when it is one.

function isatty(fd: number): boolean {
	// Only the three standard descriptors can be a terminal here, and only when the host actually
	// attached one. Answering an unconditional `true` (as this module used to) tells a program in a
	// non-interactive run that it may emit colour and cursor motion into something that is really a
	// pipe or a log file.
	if (fd !== 0 && fd !== 1 && fd !== 2) return false;
	const stream =
		fd === 0 ? process.stdin : fd === 1 ? process.stdout : process.stderr;
	return Boolean((stream as { isTTY?: boolean } | undefined)?.isTTY);
}

class WriteStream {
	constructor(fd?: number) {
		return (fd === 2 ? process.stderr : process.stdout) as any;
	}
}

class ReadStream {
	constructor() {
		return process.stdin as any;
	}
}

// On the prototype, for the callers that read them off `WriteStream.prototype` rather than off an
// instance — supports-color does exactly that to decide colour depth without opening anything.
(WriteStream.prototype as Record<string, unknown>).hasColors = function (
	count?: number
): boolean {
	const stdout = process.stdout as unknown as {
		hasColors?: (n?: number) => boolean;
	};
	return stdout.hasColors?.(count) ?? true;
};
(WriteStream.prototype as Record<string, unknown>).getColorDepth = function (): number {
	const stdout = process.stdout as unknown as { getColorDepth?: () => number };
	return stdout.getColorDepth?.() ?? 24;
};

const tty = { isatty, WriteStream, ReadStream };

export default tty as unknown as typeof import("node:tty");
