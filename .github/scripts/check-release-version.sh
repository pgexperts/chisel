#!/usr/bin/env bash
# Release-path guard shared by wheels.yml, publish-crate.yml and
# swift-release.yml: refuse to publish unless the tag being released names the
# same version every published manifest declares.
#
# Each registry takes its version from a different file — crates.io from the
# root Cargo.toml, PyPI from python/pyproject.toml (with python/Cargo.toml
# building the extension inside it) — and nothing else ties those to the tag.
# A tag pushed before the bump commit, or a bump that missed one file, would
# otherwise ship an artifact whose version disagrees with its tag, and neither
# crates.io nor PyPI lets a published version be replaced, only yanked.
#
# chisel-ffi is deliberately not checked: it is publish = false and versioned
# independently (it ships only inside the Swift xcframework, whose consumers
# pin the git tag, not a crate version).
#
# Usage: check-release-version.sh <tag>      e.g. v1.0.0 (leading "v" optional)
# Needs: cargo, and python3 >= 3.11 (tomllib).
set -euo pipefail

if [ "$#" -ne 1 ] || [ -z "$1" ]; then
  echo "usage: $0 <release-tag>   (e.g. v1.0.0)" >&2
  exit 2
fi

TAG="$1"
VERSION="${TAG#v}"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"

# Reject anything that is not plain SemVer up front, so a typo'd tag reports
# itself as malformed rather than as a confusing mismatch against every file.
SEMVER='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$'
if ! [[ "$VERSION" =~ $SEMVER ]]; then
  echo "::error::Release tag '$TAG' is not of the form vMAJOR.MINOR.PATCH (SemVer)." >&2
  exit 1
fi

PYTHON="${PYTHON:-python3}"
if ! "$PYTHON" -c 'import tomllib' 2>/dev/null; then
  echo "::error::$PYTHON lacks tomllib; this check needs Python 3.11 or newer." >&2
  exit 1
fi

# Cargo versions come from `cargo metadata` rather than grepping Cargo.toml,
# so a `version` key in some other table, or a workspace-inherited version,
# cannot be misread. --no-deps keeps it offline and fast.
METADATA="$(cd "$ROOT" && cargo metadata --no-deps --format-version 1)"

cargo_version() {
  printf '%s' "$METADATA" | "$PYTHON" -c '
import json, sys
name = sys.argv[1]
for pkg in json.load(sys.stdin)["packages"]:
    if pkg["name"] == name:
        print(pkg["version"])
        break
else:
    sys.exit(f"package {name!r} not found in cargo metadata")
' "$1"
}

pyproject_version() {
  "$PYTHON" -c '
import sys, tomllib
with open(sys.argv[1], "rb") as f:
    project = tomllib.load(f).get("project", {})
if "version" not in project:
    sys.exit(f"{sys.argv[1]}: [project].version is missing (dynamic versions are not supported here)")
print(project["version"])
' "$1"
}

declare -a LABELS VERSIONS
LABELS+=("Cargo.toml (chisel-storage)");          VERSIONS+=("$(cargo_version chisel-storage)")
LABELS+=("python/Cargo.toml (chisel-py)");         VERSIONS+=("$(cargo_version chisel-py)")
LABELS+=("python/pyproject.toml (chisel-storage)"); VERSIONS+=("$(pyproject_version "$ROOT/python/pyproject.toml")")

status=0
for i in "${!LABELS[@]}"; do
  if [ "${VERSIONS[$i]}" = "$VERSION" ]; then
    echo "ok        ${LABELS[$i]}: ${VERSIONS[$i]}"
  else
    echo "::error::${LABELS[$i]} declares version ${VERSIONS[$i]}, but the release tag '$TAG' is $VERSION." >&2
    status=1
  fi
done

if [ "$status" -ne 0 ]; then
  echo "Bump every manifest above to $VERSION (or re-tag) before releasing." >&2
fi
exit "$status"
