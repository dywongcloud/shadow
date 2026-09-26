import { readFile } from "node:fs/promises";
import def, { named as renamed } from "./other.js";
import * as ns from "./ns.js";

export const answer = 42;
export { renamed as reexported };
export * from "./star.js";

const contents = await readFile("./package.json", "utf8");
console.log(def, renamed, ns, contents);
