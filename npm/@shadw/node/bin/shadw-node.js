#!/usr/bin/env node
"use strict";

/**
 * `npx @shadw/node` — boot a real shadw platform node locally, as easily as
 * any other npx dev tool. This is the actual `hive-cloud` server binary
 * (the same one every fleet node runs), not a simulation of one.
 *
 * Defaults are chosen for a LOCAL DEV / DEMO node, not a production fleet
 * member — production nodes are provisioned via this repo's ansible roles,
 * which do real hardware/KVM detection, mesh bootstrap, TLS, etc. This tool
 * mirrors scripts/dev-cluster.sh's single-node shape instead:
 *   - HIVE_FORCE_MOCK=1 unless the caller already set HIVE_FORCE_MOCK: no
 *     assumption of KVM/Firecracker on an arbitrary dev machine. Functions
 *     still really run (as host processes), same isolation tier the fleet's
 *     own mock-backend nodes use.
 *   - HIVE_DATA defaults to ~/.shadw-node (persists across restarts, isolated
 *     from anything else on the machine) unless already set.
 *   - --name/--listen/--admin get sane single-node defaults ONLY when the
 *     caller passes NO arguments at all — pass any argument and you get full
 *     control with zero injected defaults, exactly like the real binary.
 */

const { spawn } = require("child_process");
const path = require("path");
const os = require("os");

const PLATFORM_MAP = {
  darwin: { arm64: "@shadw/node-darwin-arm64", x64: "@shadw/node-darwin-x64" },
  linux: { arm64: "@shadw/node-linux-arm64", x64: "@shadw/node-linux-x64" },
};

function fail(message) {
  process.stderr.write(`shadw-node: ${message}\n`);
  process.exit(1);
}

function resolveBinary() {
  const byArch = PLATFORM_MAP[process.platform];
  const pkgName = byArch && byArch[process.arch];
  if (!pkgName) {
    fail(
      `no prebuilt node binary for ${process.platform}/${process.arch}.\n` +
        `Supported: ${Object.entries(PLATFORM_MAP)
          .flatMap(([o, byCpu]) => Object.keys(byCpu).map((c) => `${o}/${c}`))
          .join(", ")}.`
    );
  }
  let pkgJsonPath;
  try {
    pkgJsonPath = require.resolve(`${pkgName}/package.json`);
  } catch {
    fail(
      `the "${pkgName}" optional dependency did not install.\n` +
        `Try: npm install ${pkgName} --no-save\n` +
        `If that 404s, this platform is not published yet — see the shadw-node README.`
    );
  }
  const pkg = require(pkgJsonPath);
  const binRel = typeof pkg.bin === "string" ? pkg.bin : pkg.bin[Object.keys(pkg.bin)[0]];
  return path.join(path.dirname(pkgJsonPath), binRel);
}

const userArgs = process.argv.slice(2);
if (userArgs.includes("--help") || userArgs.includes("-h")) {
  process.stdout.write(
    [
      "shadw-node — boot a real shadw platform node locally.",
      "",
      "Usage:",
      "  npx @shadw/node                 boot a single local dev node with sane defaults",
      "  npx @shadw/node <hive-cloud args>   full control, no injected defaults",
      "",
      "With no arguments, this runs:",
      "  HIVE_FORCE_MOCK=1 HIVE_DATA=~/.shadw-node hive-cloud \\",
      "    --name local --listen 127.0.0.1:8787 --admin 127.0.0.1:8786",
      "",
      "Then the admin API is at http://127.0.0.1:8786 and shadw (the CLI) will",
      "talk to it by default -- try `npx @shadw/cli auth login` against it, or just",
      "`curl http://127.0.0.1:8786/healthz`.",
      "",
    ].join("\n")
  );
  process.exit(0);
}

const binPath = resolveBinary();
const env = { ...process.env };
if (env.HIVE_FORCE_MOCK === undefined) env.HIVE_FORCE_MOCK = "1";
if (env.HIVE_DATA === undefined) env.HIVE_DATA = path.join(os.homedir(), ".shadw-node");

const args =
  userArgs.length > 0
    ? userArgs
    : ["--name", "local", "--listen", "127.0.0.1:8787", "--admin", "127.0.0.1:8786"];

if (userArgs.length === 0) {
  process.stderr.write(
    `shadw-node: booting a local dev node (mock isolation, data in ${env.HIVE_DATA})\n` +
      `shadw-node: admin API will be at http://127.0.0.1:8786 once ready\n`
  );
}

const child = spawn(binPath, args, { stdio: "inherit", env });
child.on("error", (err) => fail(`could not exec ${binPath}: ${err.message}`));
child.on("exit", (code, signal) => process.exit(signal ? 1 : code == null ? 1 : code));
