// `isolated-systemjs` is a virtual module produced by the isolatedSystemJs()
// rollup plugin (see rollup.config.js). It loads systemjs/s.js inside a private
// sandbox and re-exports `sbx.System` as its default export. That default export
// is a SystemJS instance whose public shape is described by the ambient `System`
// global from @types/systemjs, plus the lower-level prototype hooks below that
// @types/systemjs omits (signatures follow systemjs/dist/s.js).
declare module "isolated-systemjs" {
	/** A `System.register` registration tuple: [deps, declare, metas?]. */
	export type Registration = [
		dependencies: string[],
		declare: System.DeclareFn,
		metas?: unknown,
	];

	interface SystemJSHooks {
		/** Resolve + load a module, returning its System.register registration. */
		instantiate(url: string, firstParentUrl?: string, meta?: unknown): Promise<Registration>;
		/** Build the `import.meta` context object for a module. */
		createContext(parentId: string): System.Context;
		/** Retrieve (and clear) the last anonymous System.register call. */
		getRegister(url?: string): Registration | undefined;
		/** Load-completion hook, for tracing / hot-reloading. */
		onload(err: unknown, id: string, deps: string[] | undefined, isErrSource: boolean): void;
		/** Runs before an import; drives the import-map extra. */
		prepareImport(isTopLevel?: boolean): Promise<void>;
		/** Whether `instantiate` should fetch() the URL instead of script-injecting it. */
		shouldFetch(url: string, parent?: string, meta?: unknown): boolean;
		/** fetch() implementation used when `shouldFetch` returns true. */
		fetch(url: string, options?: RequestInit & { passThrough?: boolean; meta?: unknown }): Promise<Response>;
	}

	const system: typeof System & SystemJSHooks;
	export default system;
}
