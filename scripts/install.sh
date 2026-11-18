#!/usr/bin/env bash
# Installs sparkle for the current user under ~/.local. No root access is needed.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --release --locked -p sparkle

binary=target/release/sparkle
assets=$(mktemp -d)
trap 'rm -rf "$assets"' EXIT
"$binary" man > "$assets/sparkle.1"
"$binary" completions bash > "$assets/sparkle.bash"

install -Dm755 "$binary" "$HOME/.local/bin/sparkle"
install -Dm644 "$assets/sparkle.1" "$HOME/.local/share/man/man1/sparkle.1"
install -Dm644 "$assets/sparkle.bash" "$HOME/.local/share/bash-completion/completions/sparkle"

"$HOME/.local/bin/sparkle" --version
