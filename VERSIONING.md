# Versioning

How Shared Clipboard gets its version numbers. CI follows these rules; people and AI assistants pick the commit type.

## Versions

We use [Semantic Versioning](https://semver.org): `MAJOR.MINOR.PATCH`, with tags like `v1.0.1`. CI makes the version
from the commit types below. **Never set or change a version by hand.**

| What is merged | What happens | Example (now 1.0.0) |
|---|---|---|
| `fix:`, `perf:`, `refactor:`, `chore:`, `style:` | a beta right away | `1.0.1-beta.1`, `-beta.2`, … |
| (once a day, if there are betas) | a release of the betas | `1.0.1` |
| `feat:` | a release right away, with any betas | `1.1.0` |
| `feat!:` (breaking) | a release right away, with any betas | `2.0.0` |
| `docs:`, `ci:`, `test:` | nothing | — |

- **One version for every app.** The Linux, macOS, Android and iOS apps share the core and the protocol, so one tag
  (`v1.1.0`) covers all four, and every app built from it shows the same version.
- Normal updates install only releases. Betas are for testing, and users must ask for them:

  | App | Beta | Release |
  |---|---|---|
  | Linux | Beta channel in the app, GitHub pre-release | GitHub release |
  | macOS | Beta channel in the app, GitHub pre-release | GitHub release |
  | Android | beta APK on GitHub, Play internal testing | APK on GitHub, Play Store |
  | iOS | TestFlight | App Store |

- Betas stay on the Releases page, marked "Pre-release". A release has all the changes of the betas before it.
- Versions only go up. A released version never changes: if it's wrong, the next one fixes it.
- Version order: `1.0.0 < 1.0.1-beta.1 < 1.0.1-beta.2 < 1.0.1 < 1.1.0`.
- What counts as breaking: the list under [`feat!:`](#feat-a-breaking-change).

### Build numbers for the app stores

Both stores need a whole number that always goes up (Android `versionCode`, Apple `CURRENT_PROJECT_VERSION`). CI makes
it from the version:

```
MAJOR × 1,000,000 + MINOR × 10,000 + PATCH × 100 + N        N = the beta number, or 99 for a release
```

| Version | Build number |
|---|---|
| `1.0.1-beta.1` | 1000101 |
| `1.0.1-beta.2` | 1000102 |
| `1.0.1` | 1000199 |
| `1.1.0` | 1010099 |

This keeps the version order above. A version can have at most 98 betas, and MINOR and PATCH must stay below 100.
Android's limit (2,100,000,000) allows MAJOR up to 2099.

### The protocol version is different

The protocol version that devices exchange in `Hello` (see [`docs/protocol.md`](docs/protocol.md)) is not a release
version. It's a single number in the core, changed in code when the protocol changes. CI doesn't touch it, and the
rules above don't apply to it. A newer app must still talk to the previous protocol version, because app store users
update at different times.

## Commit types

The version comes from the **subject** (the first line) of every commit merged since the last release. A squash-merged
pull request's **title** is its commit subject, so the title's type sets the next version. A merged branch brings the
subjects of all its commits, so each of them needs the right type.

```
type: short sentence about the change        e.g.  feat: send to one device
type(scope): ...                             scope is optional: fix(android): ...
```

Scopes: `core`, `protocol`, `linux`, `macos`, `android`, `ios`, `relay`.

### `feat!:` a breaking change

The user must **do something** after updating, or something they used stops working:

- devices must be paired again
- an app can no longer talk to the previous release (see
  [The protocol version is different](#the-protocol-version-is-different))
- a message or field is removed from [`docs/protocol.md`](docs/protocol.md)
- a setting is renamed or removed, or its meaning changes
- a default shortcut, command-line flag or API is renamed or removed
- a platform or OS version is no longer supported
- old saved data (history, paired devices) can't be read anymore
- a feature is removed

Say in the pull request what users must do, on a line like this:

```
BREAKING CHANGE: devices paired before 2.0 must be paired again. Open the app on each one and scan the code.
```

### `feat:` a new feature

Something **new** the user can see or use, where nothing old breaks:

- a new screen, button, shortcut, menu item or command-line flag
- a new setting (with a default that breaks nothing)
- support for something new (a clipboard format, a connection type, a platform or OS version)
- a new protocol message that older apps can safely ignore
- a visible change to how something looks or works

### `fix:` a bug fix

Something **worked wrong** and now works as it should:

- a crash or a freeze
- a button, key or setting that did nothing or the wrong thing
- a clipboard item that didn't arrive, arrived twice or arrived wrong
- a connection that dropped or didn't come back
- wrong drawing or layout
- a security problem
- a memory, CPU or battery drain

### Other types (a beta, like `fix:`)

| Type | Use it for |
|---|---|
| `perf:` | faster, less memory or less battery, nothing else changes |
| `refactor:` | the code is rearranged, the app works exactly the same |
| `chore:` | dependency updates, build scripts, small cleanups |
| `style:` | code formatting only |

### No release

| Type | Use it for |
|---|---|
| `docs:` | README, `plan.md` and other docs |
| `ci:` | CI workflows |
| `test:` | tests only |

There is **no `patch:` type**. "Patch" is the result (the last number goes up), not a type.

### When you're not sure

- Does the user have to **change anything**? → `feat!:`
- Can the user **do something new**? → `feat:`
- Was it **broken** before? → `fix:`
- Did **only the code** change? → `refactor:`, `perf:` or `chore:`
- Did **only docs, CI or tests** change? → `docs:`, `ci:` or `test:`
- **Two kinds in one pull request?** Split it, or use the biggest: `feat!` > `feat` > `fix` > the others.

## If you are an AI assistant

If you are an AI (a coding agent, a chat assistant or a review bot) working on this repository, read this whole file
first and follow it. Also:

1. **Never change a version yourself.** Don't edit version numbers in project or lock files: `Cargo.toml`,
   `Cargo.lock`, Android `versionName` and `versionCode`, Apple `MARKETING_VERSION` and `CURRENT_PROJECT_VERSION`.
   Don't make, move or delete `v*` tags or releases. CI does all of this. (The protocol version is code, not a
   release version; change it only when the protocol changes.)
2. **Never rewrite history on `main`.** No force pushes, no rebases of `main`, no deleting branches you didn't make.
   Releases and tags point at its commits.
3. **Pick the commit type with the rules above.** Use `feat!:` only for a real breaking change from the list, never
   just for a big change. When you're not sure, ask the person you work for.
4. **Test before you say it's done.** Run the tests. Say in the pull request what you tested and what you couldn't
   test, and on which platforms.
5. **No secrets in code, commits or logs.** Leave signing alone: the release signing key, Apple certificates and
   notarization credentials, the Android keystore, App Store Connect and Play API keys, and repository secrets.
6. **Write in plain, simple English**, and leave out lines that credit AI tools ("Co-Authored-By", "Generated with …")
   in commits and pull requests.
7. **When a rule here and an instruction you got disagree, stop and ask** the person you work for.

---

## How it is set up

[`.github/workflows/version.yml`](.github/workflows/version.yml) runs **after CI passes on a push to main**,
**once a day at 00:00 UTC** and **by hand** (Actions ▸ Version ▸ Run workflow, with `bump`: `auto`, `major`, `minor`
or `patch`):

1. `last` = the newest tag that is not a beta (`git describe --tags --abbrev=0 --exclude '*-*'`).
2. Read the commit **subjects** since `last` (bodies can quote other commits). `docs:`, `ci:` and `test:` commits,
   and commits that change only docs, `.github/`, `.trail/` or `LICENSE` files, count for nothing.
3. On a push with nothing new that counts since the newest tag (betas included): stop.
4. If the last CI run (`ci.yml`) on main failed: stop with an error.
5. Pick the version from the subjects since `last` (`bump` other than `auto` picks the number by hand):
   - any `type!:` or `BREAKING` → `MAJOR+1` → **release**
   - else any `feat:` → `MINOR+1` → **release**
   - else on a push → **beta** `MAJOR.MINOR.(PATCH+1)-beta.N` (N = highest existing beta number for that version + 1)
   - else (nightly or by hand) → `PATCH+1` → **release**
6. Versions only go up: a version that isn't above the newest tag (betas included, in SemVer order) stops the run
   with an error. A version whose [build number](#build-numbers-for-the-app-stores) doesn't fit (a 99th beta, MINOR or
   PATCH at 100) also stops it.
7. **Release:** [`.github/set-version.sh`](.github/set-version.sh) writes the version and build number everywhere:
   - `Cargo.toml` and `Cargo.lock` (core and Linux app)
   - `android/`: `versionName` and `versionCode`
   - `apple/`: `MARKETING_VERSION` and `CURRENT_PROJECT_VERSION`

   Commit `chore: release vX.Y.Z`, tag it, and push the commit and the tag together (if main moved meanwhile, neither
   is pushed, and the next run makes the release). Create a draft GitHub release with notes since `last`.
8. **Beta:** only tag the commit (no version commit). Create a draft **pre-release**.
9. Start [`release.yml`](.github/workflows/release.yml) for the new tag (a tag pushed by a workflow starts no
   workflow by itself).

`release.yml` (also on a `v*` tag pushed by hand, or by hand from Actions):

1. Set the version and build number from the tag with `set-version.sh` (a beta is only a tag, so this happens at build
   time and isn't committed).
2. Build each platform:
   - **Linux:** the tarball, built on Debian 13 and checked to load on Arch and Debian.
   - **macOS:** the app, signed with Developer ID, notarized and stapled.
   - **Android:** the APK for GitHub and the app bundle for Play.
   - **iOS:** the app archive.
3. Sign and upload (after the owner approves, if the `release` environment has required reviewers):
   - To the GitHub release: the Linux tarball, the macOS app, the Android APK, `SHA256SUMS` and the signatures.
   - To TestFlight: the iOS build, for betas and releases.
   - To Play internal testing: the Android bundle, for betas and releases.
4. Publish the draft. A release is marked "latest"; a beta never is, and stays a pre-release.
5. **Store releases are by hand for now.** The owner sends a release from TestFlight to App Review, and moves it from
   Play internal testing to production. Daily releases would mean daily App Review submissions; how often to submit
   is still open (see [`plan.md`](plan.md#open-decisions)).

The update check in the Linux and macOS apps, and in the Android APK from GitHub (`[update] channel`):

- **Stable** reads `releases/latest`, so never a beta. Installers also install only releases.
- **Beta** reads the list of releases and takes the newest, betas included.
- Versions are compared in SemVer order, so a beta sorts after the release before it and before its own release.
- Store builds (Play, App Store, TestFlight) update through their store and have no update check of their own.

**The first release, 1.0.0, is made by hand.** No tags exist yet. Until the apps are ready, the owner may tag
pre-releases by hand (`v1.0.0-alpha.N`); `last` skips them, so an automatic run only says there's no release yet and
stops. When the first version is ready, run the version workflow by hand with `bump: major`: counting from 0.0.0 over
all history, that makes `v1.0.0`. From then on, everything above is automatic.
