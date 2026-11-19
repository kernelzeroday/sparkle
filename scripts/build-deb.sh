#!/usr/bin/env bash
# Builds the sparkle Debian source and binary packages into dist/debbuild/.
# Run on Debian or Ubuntu with the Build-Depends in packaging/debian/control installed
#   (sudo apt install build-essential devscripts debhelper cargo rustc cmake).
# Releases whose rustc is older than 1.89 can build with a rustup toolchain on PATH and
#   scripts/build-deb.sh -d     (skips the Build-Depends check)
# Extra arguments go to dpkg-buildpackage.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

command -v dpkg-buildpackage >/dev/null || {
  echo "dpkg-buildpackage not found: build the .deb on Debian or Ubuntu (apt install devscripts)" >&2
  exit 1
}

debver="$(scripts/version.sh --deb)"
srcid="$(scripts/version.sh --srcid)"
sources="$root/dist/sources"
work="$root/dist/debbuild"

scripts/make-sources.sh
rm -rf "$work"
mkdir -p "$work"
# Debian names the upstream tarball and its vendored component after the version.
ln -s "$sources/$srcid.tar.gz" "$work/sparkle_$debver.orig.tar.gz"
ln -s "$sources/$srcid-vendor.tar.gz" "$work/sparkle_$debver.orig-vendor.tar.gz"
tree="$work/sparkle-$debver"
mkdir -p "$tree"
tar -xzf "$sources/$srcid.tar.gz" -C "$tree" --strip-components=1
tar -xzf "$sources/$srcid-vendor.tar.gz" -C "$tree"

cp -r packaging/debian "$tree/debian"
license="$(cat "$sources/$srcid.licenses")"
sed "s|@LICENSE@|$license|" packaging/debian/copyright.in >"$tree/debian/copyright"
rm "$tree/debian/copyright.in"
cat >"$tree/debian/changelog" <<CHANGELOG
sparkle ($debver-1) unstable; urgency=medium

  * Build of $(git rev-parse --short=9 HEAD): $(git log -1 --format=%s)

 -- sparkle contributors <noreply@github.com>  $(LC_ALL=C date -R)
CHANGELOG

(cd "$tree" && dpkg-buildpackage -us -uc "$@")

echo
ls -l "$work"/*.deb "$work"/*.dsc 2>/dev/null
