#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

WEB_TOOLCHAIN="${GRAPHITE_WEB_TOOLCHAIN:-nightly-2026-09-20}"
WASM_BINDGEN_VERSION="0.2.128"

if ! command -v wasm-bindgen >/dev/null; then
  echo "Install wasm-bindgen-cli ${WASM_BINDGEN_VERSION} first." >&2
  exit 1
fi
installed_version="$(wasm-bindgen --version | awk '{print $2}')"
if [[ "$installed_version" != "$WASM_BINDGEN_VERSION" ]]; then
  echo "wasm-bindgen-cli ${WASM_BINDGEN_VERSION} is required; found ${installed_version}." >&2
  exit 1
fi

cargo +"$WEB_TOOLCHAIN" -Z build-std=std,panic_abort build --release --locked --lib --target wasm32-unknown-unknown
wasm-bindgen --target web --no-typescript --out-dir web/pkg target/wasm32-unknown-unknown/release/graphite.wasm
python3 scripts/check-web-memory.py web/pkg/graphite.js
printf 'Built web/pkg. Serve web/ with COOP and COEP headers, e.g. python3 scripts/serve-web.py\n'
