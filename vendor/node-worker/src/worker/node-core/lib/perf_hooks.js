class PerformanceObserver {
	constructor(_cb) {}
	observe() {}
	disconnect() {}
	takeRecords() {
		return [];
	}
}

export const performance = globalThis.performance;
export { PerformanceObserver };

export const constants = {};

export const monitorEventLoopDelay = () => ({
	enable() {},
	disable() {},
	reset() {},
	min: 0,
	max: 0,
	mean: 0,
	stddev: 0,
	percentiles: new Map(),
	exceeds: 0,
	percentile() {
		return 0;
	},
});

export default {
	performance,
	PerformanceObserver,
	constants,
	monitorEventLoopDelay,
};
