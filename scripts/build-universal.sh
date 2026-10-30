#!/usr/bin/env bash
set -euo pipefail

echo "==> Building taert release universal binary (aarch64 + x86_64)..."

# Ensure rust targets are installed
rustup target add aarch64-apple-darwin x86_64-apple-darwin

# Build both targets in release mode with LTO
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin

# Combine using lipo
mkdir -p dist
lipo -create \
  target/aarch64-apple-darwin/release/taert \
  target/x86_64-apple-darwin/release/taert \
  -output dist/taert

chmod +x dist/taert

echo "==> Universal binary created at dist/taert"
file dist/taert
