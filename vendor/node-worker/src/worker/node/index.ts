import fs from "./fs";
import net from "./net";
import http from "./http";
import process from "./process";
import events from "./events";
import stream from "./stream";
import streamPromises from "./stream-promises";
import streamConsumers from "./stream-consumers";
import streamWeb from "./stream-web";
import buffer from "./buffer";
import path from "./path";
import pathPosix from "./path-posix";
import pathWin32 from "./path-win32";
import util from "./util";
import zlib from "./zlib";
import readline from "./readline";
import readlinePromises from "./readline-promises";
import childProcess from "./child_process";
import os from "./os";
import dns from "./dns";
import timersPromises from "./timers-promises";
import tls from "./tls";
import https from "./https";
import crypto from "./crypto";
import url from "./url";
import stringDecoder from "./string_decoder";
import querystring from "./querystring";
import assert from "./assert";
import diagnosticsChannel from "./diagnostics_channel";
import workerThreads from "./worker_threads";
import vm from "./vm";
import constants from "./constants";
import http2 from "./http2";
import asyncHooks from "./async_hooks";
import console from "./console";
import timers from "./timers";
import utilTypes from "./util-types";
import perfHooks from "./perf_hooks";
import tty from "./tty";
import repl from "./repl";
import v8 from "./v8";
import { createRequire } from "../module/cjs";
import { call, callSync, channel } from "../channels";
export { depromisify, streamToBuffer } from "./utils";

// TODO
(performance as any).markResourceTiming = () => {};

let internalModules = {
	events,
	stream,
	"stream/promises": streamPromises,
	// Real node builtins in their own right, not aliases: `require("stream/consumers")` never
	// consults `stream`, so leaving them out sent the specifier to `node_modules` resolution and
	// then to an ENOENT on a path that was never going to exist.
	"stream/consumers": streamConsumers,
	"stream/web": streamWeb,
	buffer,
	path,
	"path/posix": pathPosix,
	"path/win32": pathWin32,
	util,
	"util/types": utilTypes,
	zlib,
	fs,
	net,
	http,
	"fs/promises": fs.promises,
	process,
	readline,
	repl,
	"readline/promises": readlinePromises,
	child_process: childProcess,
	os,
	dns,
	"dns/promises": dns.promises,
	timers,
	"timers/promises": timersPromises,

	tls,
	crypto,
	url,
	string_decoder: stringDecoder,
	querystring,
	assert,
	"assert/strict": assert.strict,
	diagnostics_channel: diagnosticsChannel,
	worker_threads: workerThreads,
	https,
	vm,
	constants,

	perf_hooks: perfHooks,
	// `builtinModules` and the rest are filled in below: they need the finished registry, which does
	// not exist until this object literal closes.
	module: {
		createRequire: createRequire,
		builtinModules: null as any,
		// Loader hooks and ESM export syncing have nothing to act on here — there is one CJS
		// resolver and no ESM graph — but they are called opportunistically, so they accept and
		// do nothing rather than throw.
		register() {},
		syncBuiltinESMExports() {},
	} as any,
	tty,
	v8,
	http2,
	async_hooks: asyncHooks,
	console,
};

/*
 * Snapshotted *before* the non-node module below is added, so `module.builtinModules` and
 * `module.isBuiltin` keep telling the truth about node. A program checking whether something is
 * a builtin is usually deciding whether to look in node_modules, and an answer of "yes" for a
 * name node has never had sends it somewhere that does not exist.
 */
const builtinNames = Object.keys(internalModules);
const isBuiltin = (name: string) =>
	builtinNames.includes(String(name).replace(/^node:/, ""));

/*
 * Not a node builtin, and deliberately spelled so nobody could think it is: the host's way of
 * handing a `MessagePort` to a program it started, and the two ways a program asks its host a
 * question by name. See ../channels.ts.
 *
 *   const port = await require("node-worker/channel").channel("shell");
 *   const answer = await require("node-worker/channel").call("phx.suite", results);
 *   const answer = require("node-worker/channel").callSync("phx.suite", results);
 */
(internalModules as Record<string, unknown>)["node-worker/channel"] = {
	channel,
	call,
	callSync,
};

internalModules["module"].builtinModules = builtinNames;
internalModules["module"].isBuiltin = isBuiltin;

/**
 * `module.Module`, enough of it to be useful.
 *
 * Not a real CJS Module class — there is one resolver and it is not this — but the statics are what
 * callers actually reach for: `Module.createRequire`, `Module.builtinModules`, and
 * `Module._nodeModulePaths`, which bundlers and test runners use to reconstruct a resolution chain
 * without doing the path walk themselves.
 */
function Module(): void {}
Object.assign(Module, {
	createRequire,
	builtinModules: builtinNames,
	isBuiltin,
	_nodeModulePaths(from: string): string[] {
		const out: string[] = [];
		let dir = path.resolve(from);
		for (;;) {
			out.push(path.join(dir, "node_modules"));
			const up = path.dirname(dir);
			if (up === dir) break;
			dir = up;
		}
		return out;
	},
});
internalModules["module"].Module = Module;
internalModules["module"]._nodeModulePaths = (
	Module as unknown as { _nodeModulePaths: unknown }
)._nodeModulePaths;

// Node 22+, and a feature-detection helper before it is anything else: it returns undefined for an
// unknown id rather than throwing, which is the whole reason callers prefer it to a bare `require`.
// Lives here rather than in ./process.ts because it needs this registry, and reaching for it from
// there would cycle back through this file.
(process as unknown as Record<string, unknown>).getBuiltinModule = (
	id: string
) => {
	const name = String(id).replace(/^node:/, "");
	return Object.prototype.hasOwnProperty.call(internalModules, name)
		? (internalModules as Record<string, unknown>)[name]
		: undefined;
};

export default internalModules;
