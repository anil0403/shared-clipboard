#!/usr/bin/env bash
# Signs each file given with minisign, into file.minisig. The key comes from MINISIGN_SECRET_KEY (the whole secret key
# file) and MINISIGN_PASSWORD (its password; unset for a key made with `minisign -G -W`). Without a key it signs
# nothing and says so. release.yml runs it.
set -euo pipefail
if [[ -z ${MINISIGN_SECRET_KEY:-} ]]; then
  echo "::notice::MINISIGN_SECRET_KEY is not set, so these are not signed: $*"
  exit 0
fi
if ! command -v minisign >/dev/null; then
  if command -v brew >/dev/null; then brew install minisign
  else sudo apt-get update -q && sudo apt-get install -y -q minisign; fi
fi
key=$(mktemp)
trap 'rm -f "$key"' EXIT
printf '%s\n' "$MINISIGN_SECRET_KEY" >"$key"
for file; do
  printf '%s\n' "${MINISIGN_PASSWORD:-}" | minisign -S -s "$key" -m "$file" -t "Shared Clipboard ${TAG:-}: ${file##*/}"
done
