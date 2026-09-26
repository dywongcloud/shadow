// @ts-ignore
import globalConsole from "node-core:console";
// @ts-ignore
import consoleConstructor from "node-core:internal/console/constructor";
import process from "./process";

globalConsole[consoleConstructor.kBindStreamsLazy](process);

export default globalConsole as typeof import("node:console");
