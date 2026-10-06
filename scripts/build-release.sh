#!/usr/bin/env bash
# Reproducible release build:
#   1. Build the Rust→WASM UI into crates/vortex-server/ui-dist
#   2. Build the native server (embeds ui-dist via rust-embed)
# Output: target/release/vortex-server (single executable, assets embedded)
set -euo pipefail
cd "$(dirname "$0")/.."

./scripts/build-ui.sh
cargo build --release --package vortex-server

echo
echo "Release binary: $(pwd)/target/release/vortex-server"
echo "Run:            ./target/release/vortex-server   (binds 127.0.0.1:8417 by default)"