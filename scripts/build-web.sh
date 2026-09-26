#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
if ! command -v wasm-bindgen >/dev/null; then
  echo 'Install wasm-bindgen-cli matching the wasm-bindgen crate version first.' >&2
  exit 1
fi
cargo +nightly -Z build-std=std,panic_abort build --release --lib --target wasm32-unknown-unknown
wasm-bindgen --target web --no-typescript --out-dir web/pkg target/wasm32-unknown-unknown/release/graphite.wasm
python3 scripts/check-web-memory.py web/pkg/graphite.js
printf 'Built web/pkg. Serve web/ with COOP and COEP headers, e.g. python3 scripts/serve-web.py\n'
