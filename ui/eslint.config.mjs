// Flat ESLint config (ESLint 9 + eslint-config-next 16). Next 16 removed the
// `next lint` command and the `eslint` next.config option, so lint runs via the
// ESLint CLI (`npm run lint` -> `eslint .`). eslint-config-next 16 ships a
// native flat-config array, spread in directly.
import next from "eslint-config-next";

export default [
  ...next,
  { ignores: [".next/**", "node_modules/**", "out/**", "public/**"] },
];
