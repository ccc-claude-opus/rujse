#!/bin/sh
# Unit tests, the wasm build of the engine and (when ~/src/test262 or
# $TEST262 exists) the test262 regression check.
set -e
cd "$(dirname "$0")"
export PATH="$HOME/.cargo/bin:$PATH"
cargo test --workspace --exclude wasmbench-boa --exclude wasmbench-rujse
cargo build -p rujse --no-default-features --target wasm32-unknown-unknown
T262="${TEST262:-$HOME/src/test262}"
if [ -d "$T262/test" ]; then
  cargo run -q --release -p test262 -- --root "$T262" --check tests/test262-expectations.txt
fi
