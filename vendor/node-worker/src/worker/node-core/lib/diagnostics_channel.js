class Channel {
	constructor(name) {
		this.name = name;
		this.subscribers = [];
	}
	get hasSubscribers() {
		return this.subscribers.length > 0;
	}
	publish(_data) {}
	subscribe(fn) {
		this.subscribers.push(fn);
	}
	unsubscribe(fn) {
		const i = this.subscribers.indexOf(fn);
		if (i >= 0) this.subscribers.splice(i, 1);
		return i >= 0;
	}
}

const channels = new Map();

export function channel(name) {
	let c = channels.get(name);
	if (!c) {
		c = new Channel(name);
		channels.set(name, c);
	}
	return c;
}

export function hasSubscribers(name) {
	const c = channels.get(name);
	return c ? c.hasSubscribers : false;
}

export function subscribe(name, fn) {
	channel(name).subscribe(fn);
}

export function unsubscribe(name, fn) {
	return channel(name).unsubscribe(fn);
}

export function tracingChannel(name) {
	return {
		start: channel(`tracing:${name}:start`),
		end: channel(`tracing:${name}:end`),
		asyncStart: channel(`tracing:${name}:asyncStart`),
		asyncEnd: channel(`tracing:${name}:asyncEnd`),
		error: channel(`tracing:${name}:error`),
		traceSync(fn, _ctx, thisArg, ...args) {
			return Reflect.apply(fn, thisArg, args);
		},
		tracePromise(fn, _ctx, thisArg, ...args) {
			return Reflect.apply(fn, thisArg, args);
		},
		traceCallback(fn, _position, _ctx, thisArg, ...args) {
			return Reflect.apply(fn, thisArg, args);
		},
	};
}

export default {
	channel,
	hasSubscribers,
	subscribe,
	unsubscribe,
	tracingChannel,
	Channel,
};
