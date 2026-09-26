function unsupported() {
	throw new Error('node:sqlite is not supported in this runtime');
}

export class DatabaseSync {
	constructor() {
		unsupported();
	}
}

export class StatementSync {
	constructor() {
		unsupported();
	}
}

export const constants = {};

export default {
	DatabaseSync,
	StatementSync,
	constants,
};
