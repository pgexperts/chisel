#!/usr/bin/env bash
# Builds Chisel.xcframework from the chisel-ffi crate and regenerates the
# Swift bindings. Run from the repo root: swift/build-xcframework.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/swift"
BUILD="$OUT/.xcbuild"
GEN="$OUT/Sources/Chisel/Generated"
LIBNAME="libchisel_ffi.a"

TARGETS=(
  aarch64-apple-ios
  aarch64-apple-ios-sim
  x86_64-apple-ios
  aarch64-apple-darwin
  x86_64-apple-darwin
)
for t in "${TARGETS[@]}"; do rustup target add "$t" >/dev/null; done

echo "==> cargo build (release) for all Apple targets"
for t in "${TARGETS[@]}"; do
  cargo build -p chisel-ffi --release --target "$t"
done

rm -rf "$BUILD" && mkdir -p "$BUILD"

# Fat libs: one per platform group.
lipo -create \
  "$ROOT/target/aarch64-apple-ios-sim/release/$LIBNAME" \
  "$ROOT/target/x86_64-apple-ios/release/$LIBNAME" \
  -output "$BUILD/libchisel_ffi-ios-sim.a"
lipo -create \
  "$ROOT/target/aarch64-apple-darwin/release/$LIBNAME" \
  "$ROOT/target/x86_64-apple-darwin/release/$LIBNAME" \
  -output "$BUILD/libchisel_ffi-macos.a"
cp "$ROOT/target/aarch64-apple-ios/release/$LIBNAME" "$BUILD/libchisel_ffi-ios.a"

echo "==> generate Swift bindings"
rm -rf "$GEN" && mkdir -p "$GEN"
# uniffi-bindgen's `--library` mode reads UniFFI's metadata out of the
# built cdylib's dynamic symbol table (it does not understand static-lib
# archives), so this points at the host-target .dylib rather than the
# .a used for the xcframework itself. Any of the 5 target builds carries
# identical metadata since they come from the same source; the macOS
# dylib is picked because it is guaranteed to exist on this build host.
cargo run -p chisel-ffi --bin uniffi-bindgen -- generate \
  --library "$ROOT/target/aarch64-apple-darwin/release/libchisel_ffi.dylib" \
  --language swift --out-dir "$GEN"

# UniFFI names its output after the crate's [lib] name (chisel_ffi), so
# this emits chisel_ffi.swift + chisel_ffiFFI.h + chisel_ffiFFI.modulemap.
# Headers go into the xcframework; the .swift stays in the target. The
# modulemap is renamed to module.modulemap, the filename convention
# `xcodebuild -create-xcframework` (and SPM's clang importer) expects to
# find implicitly in a headers directory.
HEADERS="$BUILD/headers"
mkdir -p "$HEADERS"
mv "$GEN"/*FFI.h "$HEADERS/"
mv "$GEN"/*FFI.modulemap "$HEADERS/module.modulemap"

echo "==> create Chisel.xcframework"
rm -rf "$OUT/Chisel.xcframework"
xcodebuild -create-xcframework \
  -library "$BUILD/libchisel_ffi-ios.a"     -headers "$HEADERS" \
  -library "$BUILD/libchisel_ffi-ios-sim.a" -headers "$HEADERS" \
  -library "$BUILD/libchisel_ffi-macos.a"   -headers "$HEADERS" \
  -output "$OUT/Chisel.xcframework"

echo "==> done: $OUT/Chisel.xcframework"
