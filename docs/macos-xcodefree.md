# Xcode-free macOS node builds and deployments

Phase A, local only. No push and no roll: both need the operator's word.

## The problem

The macOS fleet Macs deliberately have no Xcode and no Command Line Tools
(CLT). `docs/macos-nodes-mesh-loss-2026-09-25.md:145` records that invoking
`/usr/bin/otool` or `/usr/bin/python3` on shadw1/shadw2 opens the
"Install Command Line Developer Tools" dialog. `scripts/shadwd.sh install`
still ran a plain host-side `cargo build`, whose compiler, archiver, linker
and cc-rs build scripts all resolve through those shims.

## Why the obvious substitutes are not Xcode-free

Two were refuted by live execution, not by inspection:

- **Homebrew LLVM clang is not Xcode-free.** Its config
  `/opt/homebrew/Cellar/llvm/*/etc/clang/arm64-apple-darwin.cfg` hardcodes
  `-isysroot /Library/Developer/CommandLineTools/SDKs/MacOSX.sdk`. With
  `DEVELOPER_DIR=/nonexistent`, `clang -###` still shows that `-isysroot`.
- **rustc's self-contained rust-lld is not a drop-in.**
  `-C link-self-contained` is refused on `aarch64-apple-darwin`
  ("option not supported on this target"), and zig lld refuses `-Wl,-U`.
- `-Wl,-bind_at_load` is also refused by zig lld, so symbol resolution is
  checked by running the binary with `DYLD_BIND_AT_LAUNCH=1` instead.
- `dyld_info` is itself a CLT shim (`xcrun: error: missing DEVELOPER_DIR`).
- `/usr/bin/codesign` is a base-OS tool and works with Xcode disabled, so
  ad-hoc signing and the SEP keychain remain available.

`zig cc` is genuinely Xcode-free: it ships its own Darwin libc headers under
`<zig lib>/libc/include/any-darwin-any` and links Mach-O without `xcrun`,
`ld64` or an SDK.

## What landed

`scripts/macos-xcodefree.sh` is one shared lane with three modes:

- `--preflight` (build mode) fails closed unless the host is Darwin,
  `/nonexistent` does not exist, and zig, Homebrew `python3`, `git`,
  `llvm-ar` and `llvm-ranlib` are all present outside `/usr/bin`. It also
  asserts `xcode-select -p` no longer names a real developer directory.
- `--verify <binary>` uses ONLY base macOS: `od` for the Mach-O magic,
  `grep -a` for the `Security.framework` load command, and
  `DYLD_BIND_AT_LAUNCH=1` under `DEVELOPER_DIR=/nonexistent` so dyld binds
  every deferred symbol at launch and aborts on a duplicate `LC_LOAD_DYLIB`
  or a missing framework. It deliberately has no zig/LLVM/git/Python
  dependency: a fleet Mac consuming a prebuilt binary has no toolchain.
  It shares only the common preflight (Darwin + sentinel + `xcode-select`).
- `<command> [args...]` runs the build preflight, sets
  `DEVELOPER_DIR=/nonexistent`, unsets `SDKROOT` and
  `MACOSX_DEPLOYMENT_TARGET`, prepends Homebrew to the user's PATH, and
  exports both Darwin target linkers, both `CC_*` vars, `AR` and `RANLIB`
  before exec'ing the command.

`scripts/macos-xcodefree/cc` is the zig-only compiler/linker shim. It adds a
checked-in stub sysroot, normalizes Apple target triples to zig's spelling,
drops `-march=` (the six `aws-lc-sys` capability probes all still compile;
zig's `aarch64-macos` default already has `+sha3`), and deduplicates
repeated system `-l`/`-framework` flags. `objc2` and
`system-configuration` each emit `-lobjc`; zig lld writes one
`LC_LOAD_DYLIB` per occurrence and dyld aborts at launch with
"duplicate linked dylib".

`scripts/macos-xcodefree/stubs/` holds install-name-only `.tbd` files for
the libraries and frameworks the Apple-gated crates link. They carry no
Apple SDK data: the real implementations live in the dyld shared cache on
every macOS host, so `-undefined,dynamic_lookup` resolves them at load time.
`Foundation.framework` is `Versions/C`, not `Versions/A` — getting that
wrong produces a binary dyld refuses to load.

**No security gate was weakened.** `ring`, `rustls`, `aws-lc-rs`,
`instant-acme`, `security-framework`, `sep_signer`, `IntegritySigner` and
every entitlement/codesigning path are unchanged. Approach B (feature-gate
the SEP signer off) was refuted: the framework link also comes from
`rustls-platform-verifier` and `rustls-native-certs`, so disabling
`sep_signer` alone would not even remove it.

`scripts/shadwd.sh install` now has two paths:

- Fleet deployment: `HIVE_NODE_BIN` plus `HIVE_NODE_SHA256` install a
  sha256-verified prebuilt Mach-O with a `.old` backup, then `--verify` it.
  No compiler runs on the node. This is the never-again design at
  `docs/macos-nodes-mesh-loss-2026-09-25.md:238`.
- Local build: the shared lane builds `--locked` and verifies the result.

`PATHVAL` no longer derives its prefix from a possibly-missing `node`
(the old `/usr/bin/false` fallback put `/usr/bin` first), and every launchd
plist gets `DEVELOPER_DIR=/nonexistent` so a CLT shim fails loudly instead
of opening a dialog. `scripts/build-fleet-binary.sh` pins Homebrew `python3`.
The host-side `cargo check` in `cmd_fc_daemon` uses the lane; the in-VM
Linux builds are untouched.

## Witness (live, this host, CLT installed but disabled)

A clean target dir, `DEVELOPER_DIR=/nonexistent`, `xcode-select -p` =
`/nonexistent`, driven entirely by the repo's committed scripts:

```
CARGO_TARGET_DIR=/private/tmp/hive-gm-witness/xcode-probe/target-repo \
  ./scripts/macos-xcodefree.sh cargo build -p hive-cloud --features zkauth --locked
./scripts/macos-xcodefree.sh --verify <binary>
```

`BUILD_EXIT=0` (8m 21s), `VERIFY_EXIT=0`, `macho=1`,
`security_framework=1`, `eager_bind=0`, Mach-O 64-bit arm64, `--help`
printed with empty stderr. The load commands include
`Security.framework/Versions/A/Security` and
`Foundation.framework/Versions/C/Foundation`.

Two more witnesses on the final tree. First, a simulated bare fleet Mac
(`HIVE_ZIG=/nonexistent/zig HIVE_PY3=/nonexistent/python3
HIVE_GIT=/nonexistent/git`) still gets `RESULT PASS xcodefree_verify
macho=1 security_framework=1 eager_bind=0` — the prebuilt path needs no
toolchain. Second, two negative binaries fail closed: one with no
`Security.framework` link is refused with `FATAL: Security.framework link
missing`, and one with a duplicate `LC_LOAD_DYLIB` is refused by dyld's
`duplicate linked dylib` abort during eager bind. Both exit 1.

Failures found and fixed at the shared primitive, each by live execution:
`-liconv` unresolved (added a minimal stub), `-lobjc` unresolved (added a
minimal stub), missing `SystemConfiguration`/`IOKit` framework stubs,
duplicate `libobjc.A.dylib` load command (flag dedupe), the `Foundation`
`Versions/A` install-name (corrected to `Versions/C`), and `--verify`
wrongly requiring the full build toolchain (split into a common preflight
plus a build-only preflight; Mach-O checks reduced to base-OS `od`/`grep`).

## Residuals (not witnessed, do not claim otherwise)

- Only `aarch64-apple-darwin` debug was witnessed. The x86_64 lane, the
  release profile, and a real launchd boot on a CLT-free fleet Mac are not.
  `-intel` CI is off at the operator's decision (2026-11-13).
- `scripts/cli-acceptance.sh` still calls bare `python3` in seven places and
  the CI workflows still call bare `python3 -I`. Both are pinned by
  `scripts/contract-bindings.json` canonical hashes, so editing them needs
  a reviewed `--sync` in the same commit; CI never runs `--sync`. Those
  run on GitHub macOS runners that ship developer tools, not on CLT-free
  fleet Macs. Recorded, not silently changed.
- `-undefined,dynamic_lookup` defers symbol resolution to load time; the
  mandatory `--verify` with `DYLD_BIND_AT_LAUNCH=1` is what makes a missing
  symbol fail the build. A symbol reached only on a rarely used code path
  is not exercised by `--help`.
- The stub sysroot is install-name-only. A future crate that links a new
  Apple framework needs one more `.tbd`; the failure is a loud linker
  error naming the missing framework, not a silent misbuild.
- `HIVE_NODE_BIN` / `HIVE_NODE_SHA256` are operator-supplied. No artifact
  source or fleet roll was chosen here; no inventory, vault or hosts.ini
  was edited.
- The project fact learned by live execution (Homebrew LLVM clang's `.cfg`
  hardcodes a CLT SDK; zig cc ships its own Darwin headers) belongs in
  `AGENTS.md` under "Facts found by troubleshooting", but that file is
  30,717 bytes against its 30,720-byte cap. Adding a line requires the
  compaction pass tracked by the existing row
  `agents-md-size-cap-blocks-prose-addition`. This doc is the design record
  in the meantime; the cap was not broken.
