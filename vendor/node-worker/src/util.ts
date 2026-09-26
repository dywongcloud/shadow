
export type DistributiveOmit<T, K extends PropertyKey> = T extends any
	? Omit<T, K>
	: never;

export let genuid = () => [...Array(16)].reduce(
	(a) => a + Math.random().toString(36),
	""
);
