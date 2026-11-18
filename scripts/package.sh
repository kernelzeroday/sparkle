#!/usr/bin/env bash
# Builds release .deb and .rpm packages into dist/.
# Requires: cargo install cargo-deb cargo-generate-rpm
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --release --locked -p sparkle

assets=dist/assets
mkdir -p "$assets"
binary=target/release/sparkle
"$binary" man > "$assets/sparkle.1"
"$binary" completions bash > "$assets/sparkle.bash"
"$binary" completions zsh > "$assets/_sparkle"
"$binary" completions fish > "$assets/sparkle.fish"

cargo generate-rpm -p crates/sparkle --output dist/
cargo deb -p sparkle --no-build --no-strip --output dist/

ls -l dist/*.rpm dist/*.deb
