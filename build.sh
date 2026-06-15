#!/usr/bin/env bash
# Compile the toy server to a fully static musl binary and stage it in data/
# so `truss push` bundles it into the image. Re-run whenever src/main.rs changes.
set -euo pipefail
cd "$(dirname "$0")"

TARGET=x86_64-unknown-linux-musl
rustup target add "$TARGET" >/dev/null 2>&1 || true

cargo build --release --target "$TARGET" --manifest-path server/Cargo.toml

mkdir -p data
cp "server/target/$TARGET/release/toy-openai-server" data/toy-openai-server
chmod +x data/toy-openai-server

echo "Staged static binary:"
file data/toy-openai-server
ls -lh data/toy-openai-server
