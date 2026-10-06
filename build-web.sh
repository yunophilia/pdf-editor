#!/usr/bin/env bash
# Build both wasm bundles into web/ for a static deploy.
set -euo pipefail
cd "$(dirname "$0")"

MODE="${1:---release}"

# simd128 is supported by every browser that can run this app, and the CPU
# rasteriser leans on it heavily.
export RUSTFLAGS="${RUSTFLAGS:-} -C target-feature=+simd128"

echo "==> UI"
wasm-pack build crates/ui --target web --out-dir ../../web/pkg --out-name pdf_editor_ui "$MODE" --no-pack

echo "==> engine worker"
wasm-pack build crates/worker --target web --out-dir ../../web/worker-pkg --out-name pdf_editor_worker "$MODE" --no-pack

rm -f web/pkg/.gitignore web/worker-pkg/.gitignore
echo "==> done; serve with: python serve.py"
