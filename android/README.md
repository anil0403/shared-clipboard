# Shared Clipboard for Android

A Compose app around the Rust core (`../core`). Android 10 (API 29) and newer.

## Build

You need a JDK 17 or 21, the Android SDK with NDK `30.0.16248370`, Rust with the Android targets, and cargo-ndk:

```sh
rustup target add aarch64-linux-android x86_64-linux-android
cargo install cargo-ndk
export JAVA_HOME=/path/to/jdk ANDROID_HOME=~/Android/Sdk
./gradlew assembleDebug        # app/build/outputs/apk/debug/app-debug.apk
```

`assembleDebug` also builds the core with cargo-ndk (arm64-v8a and x86_64) and generates its Kotlin bindings with the
core's `uniffi-bindgen`, both into `app/build/generated`. Neither is committed.

`versionCode` and `versionName` in `app/build.gradle.kts` are set by CI; see [`VERSIONING.md`](../VERSIONING.md).
