import { PUTER_USER } from "../puter";
import { freeMemory, totalMemory } from "./memory";

function unsupported(name: string) {
	return () => {
		throw new Error(`node:os.${name} is not supported in this runtime`);
	};
}

const EOL = "\n";

// Standard Linux values (dumped from a real node's os.constants). There is no
// "native" platform under wasm, so Linux is the sensible canonical choice and
// what most consumers assume. These back both os.constants and node:constants.
const constants = {
	UV_UDP_REUSEADDR: 4,
	dlopen: {
		RTLD_LAZY: 1,
		RTLD_NOW: 2,
		RTLD_GLOBAL: 256,
		RTLD_LOCAL: 0,
		RTLD_DEEPBIND: 8,
	},
	errno: {
		E2BIG: 7, EACCES: 13, EADDRINUSE: 98, EADDRNOTAVAIL: 99,
		EAFNOSUPPORT: 97, EAGAIN: 11, EALREADY: 114, EBADF: 9, EBADMSG: 74,
		EBUSY: 16, ECANCELED: 125, ECHILD: 10, ECONNABORTED: 103,
		ECONNREFUSED: 111, ECONNRESET: 104, EDEADLK: 35, EDESTADDRREQ: 89,
		EDOM: 33, EDQUOT: 122, EEXIST: 17, EFAULT: 14, EFBIG: 27,
		EHOSTUNREACH: 113, EIDRM: 43, EILSEQ: 84, EINPROGRESS: 115, EINTR: 4,
		EINVAL: 22, EIO: 5, EISCONN: 106, EISDIR: 21, ELOOP: 40, EMFILE: 24,
		EMLINK: 31, EMSGSIZE: 90, EMULTIHOP: 72, ENAMETOOLONG: 36,
		ENETDOWN: 100, ENETRESET: 102, ENETUNREACH: 101, ENFILE: 23,
		ENOBUFS: 105, ENODATA: 61, ENODEV: 19, ENOENT: 2, ENOEXEC: 8,
		ENOLCK: 37, ENOLINK: 67, ENOMEM: 12, ENOMSG: 42, ENOPROTOOPT: 92,
		ENOSPC: 28, ENOSR: 63, ENOSTR: 60, ENOSYS: 38, ENOTCONN: 107,
		ENOTDIR: 20, ENOTEMPTY: 39, ENOTSOCK: 88, ENOTSUP: 95, ENOTTY: 25,
		ENXIO: 6, EOPNOTSUPP: 95, EOVERFLOW: 75, EPERM: 1, EPIPE: 32,
		EPROTO: 71, EPROTONOSUPPORT: 93, EPROTOTYPE: 91, ERANGE: 34,
		EROFS: 30, ESPIPE: 29, ESRCH: 3, ESTALE: 116, ETIME: 62,
		ETIMEDOUT: 110, ETXTBSY: 26, EWOULDBLOCK: 11, EXDEV: 18,
	},
	signals: {
		SIGHUP: 1, SIGINT: 2, SIGQUIT: 3, SIGILL: 4, SIGTRAP: 5, SIGABRT: 6,
		SIGIOT: 6, SIGBUS: 7, SIGFPE: 8, SIGKILL: 9, SIGUSR1: 10, SIGSEGV: 11,
		SIGUSR2: 12, SIGPIPE: 13, SIGALRM: 14, SIGTERM: 15, SIGCHLD: 17,
		SIGSTKFLT: 16, SIGCONT: 18, SIGSTOP: 19, SIGTSTP: 20, SIGTTIN: 21,
		SIGTTOU: 22, SIGURG: 23, SIGXCPU: 24, SIGXFSZ: 25, SIGVTALRM: 26,
		SIGPROF: 27, SIGWINCH: 28, SIGIO: 29, SIGPOLL: 29, SIGPWR: 30,
		SIGSYS: 31,
	},
	priority: {
		PRIORITY_LOW: 19,
		PRIORITY_BELOW_NORMAL: 10,
		PRIORITY_NORMAL: 0,
		PRIORITY_ABOVE_NORMAL: -7,
		PRIORITY_HIGH: -14,
		PRIORITY_HIGHEST: -20,
	},
};

const devNull = "/dev/null";

function platform() {
	return "browser" as NodeJS.Platform;
}

function type() {
	return "Browser";
}

function release() {
	return "0.0.0";
}

function version() {
	return "";
}

function arch() {
	return "wasm";
}

function endianness(): "BE" | "LE" {
	const buf = new ArrayBuffer(2);
	new DataView(buf).setInt16(0, 256, true);
	return new Int16Array(buf)[0] === 256 ? "LE" : "BE";
}

function hostname() {
	return "puter";
}

function username(): string {
	return PUTER_USER.username;
}

function homedir(): string {
	// `$HOME` first, as node does on posix — which is also the only answer that is
	// right on an anonymous run, where there is no user directory to name.
	return process.env.HOME || `/${username()}`;
}

function tmpdir() {
	return "/tmp";
}

function uptime() {
	return performance.now() / 1000;
}

// Approximated rather than zero — see ./memory.ts. A zero total is what turns a used-memory
// percentage into NaN or Infinity, after which a low-memory guard comparing against it either
// always fires or never does, with nothing to trace.
function freemem() {
	return freeMemory();
}

function totalmem() {
	return totalMemory();
}

function loadavg() {
	return [0, 0, 0];
}

function cpus() {
	const count =
		typeof navigator !== "undefined" &&
		typeof navigator.hardwareConcurrency === "number"
			? navigator.hardwareConcurrency
			: 1;
	const result = [];
	for (let i = 0; i < count; i++) {
		result.push({
			model: "unknown",
			// Nonzero for the same reason totalmem is: a clock speed of 0 is a divisor in anything
			// estimating work per unit time. The value is arbitrary; being usable is not.
			speed: 2400,
			times: { user: 0, nice: 0, sys: 0, idle: 0, irq: 0 },
		});
	}
	return result;
}

function availableParallelism() {
	if (
		typeof navigator !== "undefined" &&
		typeof navigator.hardwareConcurrency === "number"
	) {
		return navigator.hardwareConcurrency;
	}
	return 1;
}

function networkInterfaces() {
	return {};
}

function userInfo(_options?: any) {
	return {
		uid: -1,
		gid: -1,
		username: username(),
		homedir: homedir(),
		shell: null,
	};
}

function machine() {
	return "wasm";
}

const os = {
	EOL,
	constants,
	devNull,
	platform,
	type,
	release,
	version,
	arch,
	endianness,
	hostname,
	homedir,
	tmpdir,
	uptime,
	freemem,
	totalmem,
	loadavg,
	cpus,
	availableParallelism,
	networkInterfaces,
	userInfo,
	machine,
	getPriority: unsupported("getPriority"),
	setPriority: unsupported("setPriority"),
};

export default os as unknown as typeof import("node:os");
