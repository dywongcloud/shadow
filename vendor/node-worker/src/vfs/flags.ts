// Parsing an `fs.open` flags string.
//
// Shared because both sides need it now: the worker validates what a program passed, and the
// host actually opens the file and has to know whether to create, truncate or append. One copy,
// so `"a+"` cannot mean two different things depending on which side is asking.

import { fsError } from "./errno";

export type OpenFlags = {
	/**
	 * The flags as given, normalized — a canonical string for a string argument, the bitmask
	 * itself for a numeric one.
	 *
	 * This is what the worker sends the host, which parses it again. A number stays a number
	 * because the string shorthands cannot express every bitmask: `O_RDWR|O_CREAT` with no
	 * `O_TRUNC` — what Go's `os.OpenFile` asks for, and a database file's usual open — is
	 * neither `r+` (never creates) nor `w+` (always truncates).
	 */
	flag: string | number;
	read: boolean;
	write: boolean;
	append: boolean;
	create: boolean;
	truncateOnOpen: boolean;
	exclusive: boolean;
};

/**
 * The O_* bits that change what an open *does*, at their Linux values.
 *
 * Node exposes the host platform's numbers as `fs.constants`, so in principle these belong to
 * a platform. In practice the only numbers that reach this runtime are Linux's: that is what
 * `fs.constants` reports in a browser build, and what Go's js/wasm `syscall` package hardcodes.
 */
const O_ACCMODE = 0o3;
const O_RDONLY = 0o0;
const O_WRONLY = 0o1;
const O_RDWR = 0o2;
const O_CREAT = 0o100;
const O_EXCL = 0o200;
const O_TRUNC = 0o1000;
const O_APPEND = 0o2000;

/**
 * A numeric `flags` bitmask, which node accepts everywhere it accepts a string.
 *
 * Every other bit is ignored rather than rejected — `O_CLOEXEC`, `O_NOCTTY`, `O_NONBLOCK`,
 * `O_SYNC` and friends describe how a real descriptor behaves, and there is nothing here for
 * them to mean. Ignoring them is much closer to node than refusing the open.
 */
function parseNumericFlags(flags: number): OpenFlags {
	const access = flags & O_ACCMODE;
	// 3 is the one access mode with no meaning; linux rejects it and so does this.
	if (access !== O_RDONLY && access !== O_WRONLY && access !== O_RDWR) {
		throw fsError("EINVAL", { syscall: "open", message: "invalid flags" });
	}
	return {
		flag: flags,
		read: access === O_RDONLY || access === O_RDWR,
		write: access === O_WRONLY || access === O_RDWR,
		append: (flags & O_APPEND) !== 0,
		create: (flags & O_CREAT) !== 0,
		truncateOnOpen: (flags & O_TRUNC) !== 0,
		exclusive: (flags & O_EXCL) !== 0,
	};
}

// Parses an fs open() flags argument ("r", "w+", "ax", ..., or an O_* bitmask) into the
// booleans the handle implementations care about.
export function parseOpenFlags(flags: string | number | undefined): OpenFlags {
	if (flags === undefined) flags = "r";

	if (typeof flags === "number") return parseNumericFlags(flags);

	const aliases: Record<string, string> = {
		rs: "r",
		"rs+": "r+",
		as: "a",
		"as+": "a+",
	};

	const normalized = aliases[flags] ?? flags;

	const table: Record<string, OpenFlags> = {
		r: {
			flag: "r",
			read: true,
			write: false,
			append: false,
			create: false,
			truncateOnOpen: false,
			exclusive: false,
		},
		"r+": {
			flag: "r+",
			read: true,
			write: true,
			append: false,
			create: false,
			truncateOnOpen: false,
			exclusive: false,
		},
		w: {
			flag: "w",
			read: false,
			write: true,
			append: false,
			create: true,
			truncateOnOpen: true,
			exclusive: false,
		},
		"w+": {
			flag: "w+",
			read: true,
			write: true,
			append: false,
			create: true,
			truncateOnOpen: true,
			exclusive: false,
		},
		wx: {
			flag: "wx",
			read: false,
			write: true,
			append: false,
			create: true,
			truncateOnOpen: true,
			exclusive: true,
		},
		"wx+": {
			flag: "wx+",
			read: true,
			write: true,
			append: false,
			create: true,
			truncateOnOpen: true,
			exclusive: true,
		},
		a: {
			flag: "a",
			read: false,
			write: true,
			append: true,
			create: true,
			truncateOnOpen: false,
			exclusive: false,
		},
		"a+": {
			flag: "a+",
			read: true,
			write: true,
			append: true,
			create: true,
			truncateOnOpen: false,
			exclusive: false,
		},
		ax: {
			flag: "ax",
			read: false,
			write: true,
			append: true,
			create: true,
			truncateOnOpen: false,
			exclusive: true,
		},
		"ax+": {
			flag: "ax+",
			read: true,
			write: true,
			append: true,
			create: true,
			truncateOnOpen: false,
			exclusive: true,
		},
	};

	const parsed = table[normalized];
	if (!parsed)
		throw fsError("EINVAL", { syscall: "open", message: "invalid flags" });
	return parsed;
}
