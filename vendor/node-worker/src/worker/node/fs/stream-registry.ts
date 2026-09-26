// One-way door between FileHandle and the stream classes.
//
// ./streams.ts genuinely depends on ./handle.ts — a stream is built on a
// FileHandle. The reverse edge is only a convenience (`handle.createReadStream()`),
// and importing ./streams.ts from ./handle.ts drags the stream classes into the fs
// module-init cycle, where `class ReadStream extends Readable` runs before
// ../stream.ts is initialized and throws "Cannot read properties of undefined
// (reading 'Readable')".
//
// Upstream node has the same problem and solves it with
// `lazyLoadStreams() { return require('internal/fs/streams') }` in
// internal/fs/promises.js. ESM imports can't be deferred that way in a rollup
// bundle, so ./streams.ts registers itself here at the end of its own evaluation
// and ./handle.ts reads the slot at call time.

export interface StreamCtors {
	ReadStream?: new (path: any, options?: any) => any;
	WriteStream?: new (path: any, options?: any) => any;
}

export let streamCtors: StreamCtors = {};

export function registerStreamCtors(ctors: Required<StreamCtors>) {
	streamCtors.ReadStream = ctors.ReadStream;
	streamCtors.WriteStream = ctors.WriteStream;
}
