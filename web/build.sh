#!/bin/sh
# Build the self-contained web page: web/dist/xxf.html.
#
# Compiles the xxf-web crate to WebAssembly, then fills web/index.html's
# placeholders: the engine (base64), version, commit, date and engine hash.
# Rerun after any change to the Rust code. Needs the wasm target once:
#   rustup target add wasm32-unknown-unknown
set -eu
cd "$(dirname "$0")/.."

cargo build --release -p xxf-web --target wasm32-unknown-unknown
wasm=target/wasm32-unknown-unknown/release/xxf_web.wasm

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
commit=$(git rev-parse --short HEAD 2>/dev/null || echo unknown)
git diff --quiet HEAD 2>/dev/null || commit="$commit+modified"
date=$(date -u +%Y-%m-%d)
sha=$(shasum -a 256 "$wasm" | cut -d' ' -f1)

mkdir -p web/dist
out=web/dist/xxf.html
fill() { sed -e "s/{{VERSION}}/$version/" -e "s/{{COMMIT}}/$commit/" \
             -e "s/{{BUILD_DATE}}/$date/" -e "s/{{WASM_SHA256}}/$sha/"; }
{
  sed '/{{WASM_BASE64}}/,$d' web/index.html | fill
  base64 < "$wasm" | tr -d '\n'
  echo
  sed '1,/{{WASM_BASE64}}/d' web/index.html | fill
} > "$out"

echo "built $out ($(wc -c < "$out" | tr -d ' ') bytes)"
echo "engine sha256 $sha"
echo "page   sha256 $(shasum -a 256 "$out" | cut -d' ' -f1)"
