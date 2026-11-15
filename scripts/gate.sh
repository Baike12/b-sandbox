#!/bin/sh
# b-sandbox test gate: the bar every change must clear before it is "done".
# Usage: ./scripts/gate.sh
set -e
cd "$(dirname "$0")/.."

cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
