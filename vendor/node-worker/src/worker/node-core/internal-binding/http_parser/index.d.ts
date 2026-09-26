export class ConnectionsList {
	all(): Array<{ socket: any }>;
	idle(): Array<{ socket: any }>;
	active(): Array<{ socket: any }>;
	expired(headersTimeout: number, requestTimeout: number): Array<{ socket: any }>;
}

export class HTTPParser {
	static BOTH: number;
	static REQUEST: number;
	static RESPONSE: number;
	static kOnMessageBegin: number;
	static kOnHeaders: number;
	static kOnHeadersComplete: number;
	static kOnBody: number;
	static kOnMessageComplete: number;
	static kOnExecute: number;
	static kOnTimeout: number;
	static kLenientNone: number;
	static kLenientHeaders: number;
	static kLenientChunkedLength: number;
	static kLenientKeepAlive: number;
	static kLenientTransferEncoding: number;
	static kLenientVersion: number;
	static kLenientDataAfterClose: number;
	static kLenientOptionalLFAfterCR: number;
	static kLenientOptionalCRLFAfterChunk: number;
	static kLenientOptionalCRBeforeLF: number;
	static kLenientSpacesAfterChunkSize: number;
	static kLenientAll: number;

	[key: number]: ((...args: any[]) => any) | undefined;

	initialize(
		type: number,
		resource: unknown,
		maxHeaderSize?: number,
		lenientFlags?: number,
		connectionsList?: ConnectionsList | null,
	): void;
	execute(data: ArrayBuffer | ArrayBufferView): number | Error;
	finish(): Error | undefined;
	pause(): void;
	resume(): void;
	close(): void;
	free(): void;
	remove(): void;
	consume(handle?: unknown): void;
	unconsume(): void;
	getCurrentBuffer(): Uint8Array;
	destroy(): void;
}

export const methods: readonly string[];
export const allMethods: readonly string[];

declare const binding: {
	ConnectionsList: typeof ConnectionsList;
	HTTPParser: typeof HTTPParser;
	methods: typeof methods;
	allMethods: typeof allMethods;
};

export default binding;
