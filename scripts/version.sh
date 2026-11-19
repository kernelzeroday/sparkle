#!/usr/bin/env bash
# Prints sparkle's version. A commit tagged v<version> is a release; anything else is a
# snapshot named after its commit date and hash.
#   scripts/version.sh          0.1.0
#   scripts/version.sh --rpm    0.1.0 or 0.1.0^git20261119.bb3f917ab
#   scripts/version.sh --deb    0.1.0 or 0.1.0+git20261119.bb3f917ab
#   scripts/version.sh --srcid  sparkle-0.1.0 or sparkle-0.1.0-git20261119.bb3f917ab
set -euo pipefail
cd "$(dirname "$0")/.."

base="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"$/\1/p' Cargo.toml)"
[[ -n "$base" ]] || { echo "no version in Cargo.toml" >&2; exit 1; }

snapshot=""
if ! git describe --exact-match --tags --match "v$base" HEAD >/dev/null 2>&1; then
  snapshot="git$(git log -1 --format=%cd --date=format:%Y%m%d).$(git rev-parse --short=9 HEAD)"
fi

case "${1:-}" in
  "") echo "$base" ;;
  --rpm) echo "$base${snapshot:+^$snapshot}" ;;
  --deb) echo "$base${snapshot:++$snapshot}" ;;
  --srcid) echo "sparkle-$base${snapshot:+-$snapshot}" ;;
  *) echo "usage: $0 [--rpm|--deb|--srcid]" >&2; exit 2 ;;
esac
