#!/usr/bin/env node
"use strict";

/**
 * Dispatcher shim (the esbuild/swc/turbo pattern). This package (`shadw`)
 * carries NO binary of its own — it declares one `optionalDependency` per
 * platform (`shadw-cli-<os>-<cpu>`), npm's own os/cpu matching installs only
 * the one for the current machine, and this script's whole job is to find
 * that package and exec its real binary with the original argv and stdio.
 *
 * Kept as plain CommonJS with zero dependencies on purpose: this file runs
 * before anything else in the package graph is guaranteed usable, so it must
 * not itself require an install step, a bundler, or a specific Node feature
 * newer than what `engines` promises below.
 */

const { spawnSync } = require("child_process");

const PLATFORM_MAP = {
  darwin: { arm64: "shadw-cli-darwin-arm64", x64: "shadw-cli-darwin-x64" },
  linux: { arm64: "shadw-cli-linux-arm64", x64: "shadw-cli-linux-x64" },
};

function resolveBinary() {
  const byArch = PLATFORM_MAP[process.platform];
  const pkgName = byArch && byArch[process.arch];
  if (!pkgName) {
    fail(
      `shadw has no prebuilt binary for ${process.platform}/${process.arch}.\n` +
        `Supported: ${Object.entries(PLATFORM_MAP)
          .flatMap(([os, byCpu]) => Object.keys(byCpu).map((cpu) => `${os}/${cpu}`))
          .join(", ")}.`
    );
  }
  let pkgJsonPath;
  try {
    pkgJsonPath = require.resolve(`${pkgName}/package.json`);
  } catch {
    // The platform package genuinely failed to install (a network hiccup, or
    // — honestly, right now — a platform whose package has not been published
    // yet; only darwin-arm64 and darwin-x64 are live as of this release).
    // `optionalDependencies` means npm did NOT fail the overall install for
    // this, so the failure has to surface HERE, loudly, with the fix.
    fail(
      `The "${pkgName}" optional dependency did not install.\n` +
        `Try: npm install ${pkgName} --no-save\n` +
        `If that 404s, this platform is not published yet — see the shadw README.`
    );
  }
  const pkg = require(pkgJsonPath);
  const binRelPath = typeof pkg.bin === "string" ? pkg.bin : pkg.bin[Object.keys(pkg.bin)[0]];
  return require("path").join(require("path").dirname(pkgJsonPath), binRelPath);
}

function fail(message) {
  process.stderr.write(`shadw: ${message}\n`);
  process.exit(1);
}

const binPath = resolveBinary();
const result = spawnSync(binPath, process.argv.slice(2), { stdio: "inherit" });
if (result.error) fail(`could not exec ${binPath}: ${result.error.message}`);
process.exit(result.status == null ? 1 : result.status);
