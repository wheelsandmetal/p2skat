#!/usr/bin/env bash
# Build the static p2skat-room binary for the OpenWrt router (aarch64, musl).
# Run inside `nix develop`: build scripts still need the host cc, and the
# flake's toolchain carries the aarch64-unknown-linux-musl standard library.
set -euo pipefail
cd "$(dirname "$0")/.."
target=aarch64-unknown-linux-musl
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
cargo build --locked -p p2skat-room --profile router --target "$target"
binary="target/$target/router/p2skat-room"
echo "$binary: $(stat -c %s "$binary") bytes"
