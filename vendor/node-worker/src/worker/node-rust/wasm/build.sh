#!/usr/bin/env bash
set -euo pipefail
shopt -s inherit_errexit

if ! [ "${RELEASE:-0}" = "1" ]; then
	WASMOPTFLAGS="${WASMOPTFLAGS:-} -g"
else
	: "${WASMOPTFLAGS:=}"
fi

which cargo wasm-bindgen &> /dev/null || {
	echo "Please install cargo and wasm-bindgen-cli (matching the wasm-bindgen crate version)!"
	exit 1
}

WBG="wasm-bindgen 0.2.122"
if ! [[ "$(wasm-bindgen -V)" =~ ^"$WBG" ]]; then
	echo "Incorrect wasm-bindgen-cli version: '$(wasm-bindgen -V)' != '$WBG'"
	exit 1
fi

(
	export RUSTFLAGS='-Zlocation-detail=none -Zfmt-debug=none'
	cargo build --release --target wasm32-unknown-unknown -p wasm \
		-Z build-std=panic_abort,std -Z build-std-features=optimize_for_size
)

mkdir -p out/
# --out-name keeps the emitted files named rewriter.* (rewriter.js, rewriter_bg.wasm,
# ...) even though the crate/artifact is now `wasm`, so prepare-node.js and loader.ts
# don't need to care that the wasm bits live in a separate crate.
wasm-bindgen --target web --out-name rewriter --out-dir out/ \
	../target/wasm32-unknown-unknown/release/wasm.wasm

if [[ "$OSTYPE" == "darwin"* ]] || [[ "$OSTYPE" == "freebsd"* ]] || [[ "$OSTYPE" == "dragonfly"* ]]; then
	sed -i '' 's/import.meta.url/""/g' out/rewriter.js
else
	sed -i 's/import.meta.url/""/g' out/rewriter.js
fi

if [ "${RELEASE:-0}" = "1" ] && command -v wasm-opt &> /dev/null; then
	# shellcheck disable=SC2086
	wasm-opt $WASMOPTFLAGS out/rewriter_bg.wasm -o out/rewriter_bg.wasm -O4
fi

echo "Rewriter build complete."
