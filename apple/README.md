# Shared Clipboard for macOS and iOS

A menu bar app for macOS 14+, and an iOS 17+ app with a Share Extension, Shortcuts actions and a Control Center
button (iOS 18+). Both run the Rust core (`core/`) through its Swift bindings.

## Build on a Mac

Needs Xcode 16+, Rust (rustup) and XcodeGen. From the repository root:

```sh
brew install xcodegen
apple/build-core.sh                      # core library, xcframework and Swift bindings
xcodegen generate --spec apple/project.yml
open apple/SharedClip.xcodeproj
```

The Xcode project, the xcframework and the bindings are generated, never committed. Run `apple/build-core.sh` again
after changing the core, and `xcodegen generate` after changing `project.yml` or adding files.

To run on a device, choose your team under Signing for each target. The App Group `group.com.noobsbot.sharedclip`
lets the Share Extension use the app's identity and paired devices.

## CI

After the three setup commands above, without code signing:

```sh
xcodebuild -project apple/SharedClip.xcodeproj -scheme SharedClip-macOS -configuration Release \
  -destination 'generic/platform=macOS' CODE_SIGNING_ALLOWED=NO build
xcodebuild -project apple/SharedClip.xcodeproj -scheme SharedClip-iOS -configuration Debug \
  -destination 'generic/platform=iOS Simulator' CODE_SIGNING_ALLOWED=NO build
```

The iOS Simulator build is for Apple silicon only (the core isn't built for Intel simulators).
