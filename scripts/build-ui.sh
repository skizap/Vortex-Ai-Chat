#!/usr/bin/env bash
# Build the Leptos UI into crates/vortex-server/ui-dist (embedded by the server
# at compile time in release mode; read from disk in debug mode).
set -euo pipefail
cd "$(dirname "$0")/.."

# Trunk and wasm-bindgen-cli are the only UI build tools; versions must match
# the wasm-bindgen crate (see Cargo.lock). No Node.js/Python involved.
command -v trunk >/dev/null || { echo "trunk is not installed: cargo install trunk --locked" >&2; exit 1; }
command -v wasm-bindgen >/dev/null || { echo "wasm-bindgen-cli is not installed: cargo install wasm-bindgen-cli --version <version from Cargo.lock>" >&2; exit 1; }

cd crates/vortex-ui
trunk build --release