#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
# Keep the CLI version aligned with the pinned wasm-bindgen crate.
if [[ "$(wasm-bindgen --version)" != "wasm-bindgen 0.2.108" ]]; then
  echo 'Use nix develop: the browser build requires wasm-bindgen 0.2.108.' >&2
  exit 1
fi
cargo build --locked --release --lib --target wasm32-unknown-unknown
wasm-bindgen --target web --out-dir web/pkg --out-name p2skat target/wasm32-unknown-unknown/release/p2skat.wasm
