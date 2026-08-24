#!/usr/bin/env bash
# Prepare a Swift release: build the xcframework, zip it, compute its SwiftPM
# checksum, and rewrite the root Package.swift so the tag carries a manifest
# describing its own asset.
#
# The ordering matters. A consumer who pins tag vX.Y.Z gets THAT tag's
# Package.swift, so the url and checksum must already be correct when the tag is
# created — they cannot be patched in afterwards by the release workflow.
#
# Usage:  swift/release/prepare.sh v1.1.0
# Then:   git commit -am "release: vX.Y.Z Swift artifacts" && git tag vX.Y.Z
#         git push && git push --tags, then publish the release on GitHub.
set -euo pipefail

VERSION="${1:?usage: prepare.sh <version-tag>, e.g. v1.1.0}"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
ZIP="$ROOT/swift/Chisel.xcframework.zip"
REPO="https://github.com/pgexperts/chisel"

echo "==> building the xcframework"
"$ROOT/swift/build-xcframework.sh"

echo "==> zipping"
rm -f "$ZIP"
(cd "$ROOT/swift" && zip -qr "$ZIP" Chisel.xcframework)

echo "==> computing checksum"
CHECKSUM="$(cd "$ROOT" && swift package compute-checksum "$ZIP")"
URL="$REPO/releases/download/$VERSION/Chisel.xcframework.zip"

echo "==> rewriting Package.swift"
# Only the two lines inside the consumer branch of the binary target.
/usr/bin/sed -i '' \
  -e "s|url: \"https://github.com/pgexperts/chisel/releases/download/[^\"]*\"|url: \"$URL\"|" \
  -e "s|checksum: \"[0-9a-f]*\"|checksum: \"$CHECKSUM\"|" \
  "$ROOT/Package.swift"

cat <<MSG

Prepared $VERSION
  asset     $ZIP
  url       $URL
  checksum  $CHECKSUM

Next:
  git commit -am "release: $VERSION Swift artifacts"
  git tag $VERSION && git push && git push --tags
  Publish the release on GitHub; swift-release.yml uploads the asset.

The zip is NOT committed — it is built again by the workflow and uploaded there.
MSG
