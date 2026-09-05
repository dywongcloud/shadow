// Flat ESLint config (ESLint 9 + eslint-config-next 16). Next 16 removed the
// `next lint` command and the `eslint` next.config option, so lint runs via the
// ESLint CLI (`npm run lint` -> `eslint .`). eslint-config-next 16 ships a
// native flat-config array, spread in directly.
import next from "eslint-config-next";

const config = [
  ...next,
  // Glob, not literal ".next/**": a stray ".next.old/" (or any other backup/
  // renamed build-output dir) is minified/bundled output, not source — ESLint
  // parsing it as source produced hundreds of false "errors" pointing at
  // single-letter minified identifiers.
  { ignores: [".next*/**", "node_modules/**", "out/**", "public/**", "vendor/**"] },
];

export default config;
