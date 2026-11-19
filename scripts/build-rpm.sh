#!/usr/bin/env bash
# Builds the sparkle SRPM and RPM into dist/rpmbuild/.
#   scripts/build-rpm.sh                    build, running the test suite in %check
#   scripts/build-rpm.sh --without check    skip the tests
# Requires: rpm-build, plus the BuildRequires in packaging/rpm/sparkle.spec.in
#   (sudo dnf install rpm-build rust cargo gcc cmake)
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

rpmver="$(scripts/version.sh --rpm)"
srcid="$(scripts/version.sh --srcid)"
sources="$root/dist/sources"
top="$root/dist/rpmbuild"

scripts/make-sources.sh
mkdir -p "$top"/{SOURCES,SPECS,BUILD,RPMS,SRPMS}
ln -sf "$sources/$srcid.tar.gz" "$sources/$srcid-vendor.tar.gz" "$top/SOURCES/"

bundled="$(sed 's/^\(.*\) \(.*\)$/Provides:       bundled(crate(\1)) = \2/' "$sources/$srcid.bundled")"
subject="$(git log -1 --format=%s)"
python3 -I - packaging/rpm/sparkle.spec.in "$top/SPECS/sparkle.spec" <<PY
import sys
spec = open(sys.argv[1]).read()
for key, value in {
    "@VERSION@": """$rpmver""",
    "@SRCID@": """$srcid""",
    "@LICENSE@": open("$sources/$srcid.licenses").read().strip(),
    "@BUNDLED@": """$bundled""",
    "@DATE@": """$(LC_ALL=C date '+%a %b %d %Y')""",
    "@CHANGE@": """Build of $(git rev-parse --short=9 HEAD): ${subject//\"/\\\"}""",
}.items():
    spec = spec.replace(key, value)
open(sys.argv[2], "w").write(spec)
PY

# A system-only PATH, so the build uses the distribution toolchain the spec requires rather than
# a rustup or Homebrew one that happens to come first.
env PATH=/usr/sbin:/usr/bin:/bin rpmbuild --define "_topdir $top" --clean -ba \
  "$top/SPECS/sparkle.spec" "$@"

echo
find "$top/RPMS" "$top/SRPMS" -name "sparkle-${rpmver}-*.rpm" -printf '%p  %s bytes\n'
