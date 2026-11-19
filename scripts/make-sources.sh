#!/usr/bin/env bash
# Creates the package sources in dist/sources/, all named after the commit:
#   <srcid>.tar.gz          git archive of HEAD (commit first: it archives HEAD)
#   <srcid>-vendor.tar.gz   every crate the build needs, so packages build offline
#   <srcid>.licenses        SPDX license of the shipped binary, its crates included
#   <srcid>.bundled         "name version" for each crate compiled into the binary
# Needs network once, to vendor the crates.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

srcid="$(scripts/version.sh --srcid)"
out="$root/dist/sources"
mkdir -p "$out"

if [[ -n "$(git status --porcelain --untracked-files=no)" ]]; then
  echo "warning: uncommitted changes are NOT in the source tarball (it archives HEAD)" >&2
fi

if [[ -s "$out/$srcid.tar.gz" && -s "$out/$srcid-vendor.tar.gz" && -s "$out/$srcid.licenses" \
      && "${1:-}" != "--force" ]]; then
  echo "==> sources for $srcid are up to date"
  exit 0
fi

echo "==> source tarball ($srcid)"
git archive --format=tar.gz --prefix="$srcid/" -o "$out/$srcid.tar.gz" HEAD

work="$(mktemp -d -p "$root/dist")"
trap 'rm -rf "$work"' EXIT
mkdir "$work/src"
git archive HEAD | tar -x -C "$work/src"

echo "==> vendored crates"
(cd "$work/src" && cargo vendor --locked --versioned-dirs vendor >/dev/null)
tar -C "$work/src" -czf "$out/$srcid-vendor.tar.gz" vendor

echo "==> licenses"
mkdir -p "$work/src/.cargo"
cat >"$work/src/.cargo/config.toml" <<'TOML'
[source.crates-io]
replace-with = "vendored-sources"
[source.vendored-sources]
directory = "vendor"
TOML
# Only crates compiled into the shipped binary count: not dev or build dependencies, and not
# crates for other platforms.
(cd "$work/src" && cargo tree --locked --offline -p sparkle -e normal --prefix none \
  --format '{p}|{l}') | sort -u >"$work/tree"
python3 -I - "$work/tree" "$out/$srcid.licenses" "$out/$srcid.bundled" <<'PY'
import re, sys
tree, licenses_out, bundled_out = sys.argv[1:]
licenses, bundled = set(), set()
for line in open(tree):
    package, _, license = line.rstrip("\n").partition("|")
    name, version = package.split()[:2]
    license = re.sub(r"\s*/\s*", " OR ", license.strip()) or "LicenseRef-unknown"
    if license == "LicenseRef-unknown":
        print(f"warning: {name} declares no license expression", file=sys.stderr)
    licenses.add(f"({license})" if " OR " in license or " AND " in license else license)
    if "(" not in package:  # workspace crates are listed with their path
        bundled.add(f"{name} {version.lstrip('v')}")
own = "Apache-2.0"
rest = sorted(licenses - {own})
open(licenses_out, "w").write(" AND ".join([own] + rest) + "\n")
open(bundled_out, "w").write("".join(f"{entry}\n" for entry in sorted(bundled)))
PY
echo "    $(wc -l <"$out/$srcid.bundled") bundled crates: $(cat "$out/$srcid.licenses")"
ls -l "$out/$srcid"*
