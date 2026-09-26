import { fsConstants } from "./util";
import { globPromise } from "./glob";
import { promisesWatch } from "./watch";

type NodeFs = typeof import("node:fs");
type NodeFsPromises = NodeFs["promises"];

export let promisesRemaining: Pick<
	NodeFsPromises,
	"watch" | "glob" | "constants"
> = {
	constants: { ...fsConstants },
	glob: globPromise,
	watch: promisesWatch,
};
