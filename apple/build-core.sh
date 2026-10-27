#!/usr/bin/env bash
# Builds the Rust core for macOS and iOS and packages it for Xcode. Run it on a Mac, from anywhere:
#   apple/SharedClipCore.xcframework      the static library and its C header, for each Apple platform
#   apple/Generated/SharedClipCore.swift  the Swift bindings, compiled into each app
set -euo pipefail

cd "$(dirname "$0")/.."
target_dir=${CARGO_TARGET_DIR:-target}
out=apple/build
lib=libsharedclip.a

# Match the apps' deployment targets (apple/project.yml), so the linker doesn't warn about newer objects.
export MACOSX_DEPLOYMENT_TARGET=14.0 IPHONEOS_DEPLOYMENT_TARGET=17.0

targets=(aarch64-apple-ios aarch64-apple-ios-sim aarch64-apple-darwin x86_64-apple-darwin)
rustup target add "${targets[@]}"
for t in "${targets[@]}"; do
  cargo build -p sharedclip-core --lib --release --target "$t"
done

# Swift bindings, read from a debug build of the library for this Mac (UniFFI library mode). Debug, because the
# release profile strips symbols, and the bindings generator reads its metadata from them.
rm -rf "$out" apple/Generated
mkdir -p "$out/headers" "$out/macos" apple/Generated
cargo build -p sharedclip-core --lib --features bindgen
cargo run -p sharedclip-core --features bindgen --bin uniffi-bindgen -- generate \
  --library "$target_dir/debug/libsharedclip.dylib" --language swift --out-dir "$out/bindings"
mv "$out/bindings/SharedClipCore.swift" apple/Generated/
cp "$out/bindings/SharedClipCoreFFI.h" "$out/headers/"
cp "$out/bindings/SharedClipCoreFFI.modulemap" "$out/headers/module.modulemap"

# One macOS library for both Apple silicon and Intel.
lipo -create \
  "$target_dir/aarch64-apple-darwin/release/$lib" \
  "$target_dir/x86_64-apple-darwin/release/$lib" \
  -output "$out/macos/$lib"

rm -rf apple/SharedClipCore.xcframework
xcodebuild -create-xcframework \
  -library "$target_dir/aarch64-apple-ios/release/$lib" -headers "$out/headers" \
  -library "$target_dir/aarch64-apple-ios-sim/release/$lib" -headers "$out/headers" \
  -library "$out/macos/$lib" -headers "$out/headers" \
  -output apple/SharedClipCore.xcframework
