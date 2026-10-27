#!/usr/bin/env bash
# Makes the next version from the commits since the last release, as VERSIONING.md ("How it is set up") says: tags it,
# pushes it, makes its draft GitHub release and starts release.yml for it.
# version.yml runs it at the top of a full clone of main, with BUMP (auto, major, minor or patch) and
# GITHUB_EVENT_NAME (push, schedule or workflow_dispatch) set and gh logged in. To try it, run it in a scratch clone
# with a fake gh on PATH: it pushes to origin.
set -euo pipefail
bump=${BUMP:-auto}
event=${GITHUB_EVENT_NAME:?}
# Files that change nothing in the apps.
docs='\.md$|^docs/|^\.github/|^\.trail/|^\.claude/|^LICENSE$'

# Prints the subjects of the commits in a range that count for a version: not docs:, ci: or test:, and changing more
# than docs and CI files. Subjects only: bodies can quote other commits.
counted() {
  git log --format='%H %s' "$1" | while read -r hash subject; do
    [[ $subject =~ ^(docs|ci|test)(\(.+\))?: ]] && continue
    # No grep -q: it would stop reading early, and pipefail would count that as no match.
    git diff-tree --root --no-commit-id --name-only -r "$hash" | grep -Ev "$docs" >/dev/null || continue
    echo "$subject"
  done
}

# 1. The newest release, betas and alphas skipped.
last=$(git describe --tags --abbrev=0 --match 'v*' --exclude '*-*' 2>/dev/null || true)
if [[ -z $last && $bump == auto ]]; then
  echo "::notice::No release yet. The first one, 1.0.0, is made by hand: run the Version workflow with bump major."
  exit 0
fi

# 2 and 3. What counts since then; on a push, also since the newest tag, betas included. A run that waited behind a
# newer push, or a re-run on a tagged commit, finds nothing new.
range=${last:+$last..}HEAD
if [[ -z $(git log --format=%h "$range") ]]; then echo "Nothing since $last."; exit 0; fi
log=$(counted "$range")
if [[ -z $log && $bump == auto ]]; then echo "Only docs, CI and tests since $last."; exit 0; fi
newest=$(git describe --tags --abbrev=0 --match 'v*' 2>/dev/null || true)
if [[ $event == push && -z $(counted "${newest:+$newest..}HEAD") ]]; then echo "Nothing new since $newest."; exit 0; fi

# 4. Main must be green. A cancelled run (a newer push replaced it) says nothing about main.
ci=$(gh run list --workflow ci.yml --branch main --status completed --limit 20 --json conclusion \
  --jq 'map(select(.conclusion != "cancelled"))[0].conclusion // "success"')
[[ $ci == success ]] || { echo "::error::The last CI run on main ended with \"$ci\": fix main first."; exit 1; }

# 5. The version.
base=${last:-v0.0.0}
IFS=. read -r major minor patch <<<"${base#v}"
if [[ $bump == auto ]]; then
  if grep -Eq '^[a-z]+(\(.+\))?!:|BREAKING' <<<"$log"; then bump=major
  elif grep -Eq '^feat(\(.+\))?:' <<<"$log"; then bump=minor
  elif [[ $event == push ]]; then bump=beta
  else bump='patch'; fi
fi
case $bump in
  major) v=$((major + 1)).0.0 ;;
  minor) v=$major.$((minor + 1)).0 ;;
  patch) v=$major.$minor.$((patch + 1)) ;;
  beta)
    next=$major.$minor.$((patch + 1))
    n=$(git tag -l "v$next-beta.*" | sed 's/.*-beta\.//' | sort -n | tail -1)
    v=$next-beta.$((${n:-0} + 1))
    ;;
  *) echo "::error::Unknown bump \"$bump\"."; exit 1 ;;
esac

# 6. Versions only go up: above every tag, betas included, in SemVer order. sort -V sorts "~" before everything, so
# with a pre-release's "-" as "~", 1.0.1-beta.2 comes before 1.0.1. And the build number must fit.
top=$(git tag -l 'v*' | sed 's/-/~/' | sort -V | tail -1 | sed 's/~/-/')
printf '%s\n' "$top" "v$v" | sed 's/-/~/' | sort -CuV ||
  { echo "::error::v$v is not above the newest tag, $top: versions only go up."; exit 1; }
build=$(.github/set-version.sh --check "$v" 2>&1) || { echo "::error::$build"; exit 1; }

if [[ $bump == beta ]]; then
  # 8. A beta is only a tag: release.yml sets its version at build time.
  git tag -a "v$v" -m "v$v"
  git push origin "v$v"
  gh release create "v$v" --draft --prerelease --generate-notes --notes-start-tag "$newest"
else
  # 7. A release commits its version.
  .github/set-version.sh "$v"
  git commit -qam "chore: release v$v"
  git tag -a "v$v" -m "v$v"
  # Both or neither: if main moved meanwhile, nothing is pushed, and the next run makes the release.
  git push --atomic origin HEAD:main "v$v" ||
    { echo "::error::main moved during the release, so nothing was pushed; the next run makes it."; exit 1; }
  notes=(--generate-notes)
  [[ -z $last ]] || notes+=(--notes-start-tag "$last")
  gh release create "v$v" --draft "${notes[@]}"
fi
# 9. A tag pushed with the workflow token starts no workflow by itself.
gh workflow run release.yml --ref "v$v"
echo "Made v$v (build $build)."
