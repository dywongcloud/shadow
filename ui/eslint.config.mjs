import next from "eslint-config-next";

const config = [
  ...next,
  { ignores: [".next*/**", "node_modules/**", "out/**", "public/**", "vendor/**"] },
];

export default config;
