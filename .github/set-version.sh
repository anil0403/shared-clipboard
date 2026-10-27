#!/usr/bin/env bash
# Sets every app's version (1.2.3, or a beta like 1.2.3-beta.4; a leading "v" is fine) and the app store build number
# made from it (VERSIONING.md, "Build numbers for the app stores"): in Cargo.toml and Cargo.lock, and in the Android
# and Apple projects once they exist. Only CI runs it (version.yml, release.yml).
#   set-version.sh 1.2.3            sets it
#   set-version.sh --check 1.2.3    only checks it and prints the build number
# Runs with GNU sed (Linux) and BSD sed (macOS).
set -euo pipefail
check=
if [[ ${1:-} == --check ]]; then check=1; shift; fi
v=${1:?usage: set-version.sh [--check] <version>}
v=${v#v}
n='(0|[1-9][0-9]*)'
[[ $v =~ ^$n\.$n\.$n(-beta\.([1-9][0-9]*))?$ ]] || { echo "not a version: $1 (want 1.2.3 or 1.2.3-beta.N)" >&2; exit 1; }
major=${BASH_REMATCH[1]} minor=${BASH_REMATCH[2]} patch=${BASH_REMATCH[3]} beta=${BASH_REMATCH[5]}

# MAJOR×1,000,000 + MINOR×10,000 + PATCH×100 + N keeps the version order only while MINOR, PATCH and the beta number
# fit in two digits (N = 99 is the release), and Android allows up to 2,100,000,000.
build=0
if (( ${#major} <= 4 && ${#minor} <= 2 && ${#patch} <= 2 && ${#beta} <= 2 )) && [[ $beta != 99 ]]; then
  build=$((major * 1000000 + minor * 10000 + patch * 100 + ${beta:-99}))
fi
if (( build == 0 || build > 2100000000 )); then
  echo "$v has no build number: at most 98 betas, MINOR and PATCH below 100, MAJOR up to 2099" >&2
  exit 1
fi
if [[ -n $check ]]; then echo "$build"; exit 0; fi

cd "$(dirname "$0")/.."
# Edits a file with a sed -E script, then checks that the new line is there (if the file's format changed, it isn't).
put() {
  sed -E -i.bak "$2" "$1"
  rm "$1.bak"
  grep -qF -- "$3" "$1" || { echo "$1: found nothing to set to $3" >&2; exit 1; }
}
put Cargo.toml "/^\[workspace\.package\]/,/^\[/s/^version = .*/version = \"$v\"/" "version = \"$v\""
put Cargo.lock "/^name = \"sharedclip-[^\"]*\"$/{n;s/^version = .*/version = \"$v\"/;}" "version = \"$v\""
f=android/app/build.gradle.kts
if [[ -f $f ]]; then
  put "$f" "s/^([[:space:]]*versionCode = )[0-9]+/\1$build/" "versionCode = $build"
  put "$f" "s/^([[:space:]]*versionName = )\"[^\"]*\"/\1\"$v\"/" "versionName = \"$v\""
fi
f=apple/project.yml
if [[ -f $f ]]; then
  put "$f" "s/(MARKETING_VERSION: )\"[^\"]*\"/\1\"$v\"/" "MARKETING_VERSION: \"$v\""
  put "$f" "s/(CURRENT_PROJECT_VERSION: )\"[^\"]*\"/\1\"$build\"/" "CURRENT_PROJECT_VERSION: \"$build\""
fi
echo "version $v, build $build"
