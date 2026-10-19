#!/usr/bin/env bash
# Builds the page into dist/. Stamps the version from package.json and the
# commit (TQ_GIT_SHA, else git) into the bundle.
#   ./build.sh            # once
#   ./build.sh --watch
# TQ_RELAY_HTTP=https://relay.example.com bakes in the relay to use when the
# page is not served by the relay itself (empty: the page's own origin).
set -euo pipefail
cd "$(dirname "$0")"
VERSION=$(bun -e 'console.log(require("./package.json").version)')
SHA=${TQ_GIT_SHA:-}
if [[ -z "$SHA" ]]; then
  SHA=$(git rev-parse --short=10 HEAD 2>/dev/null || echo unknown)
  if [[ "$SHA" != unknown && -n "$(git status --porcelain --untracked-files=no 2>/dev/null)" ]]; then
    SHA="$SHA-dirty"
  fi
fi
rm -rf dist && mkdir -p dist
cp index.html styles.css dist/
exec bun build src/main.ts src/worker.ts --outdir dist --target browser --sourcemap=linked \
  --define "TQ_WEB_VERSION=\"$VERSION\"" --define "TQ_GIT_SHA=\"$SHA\"" \
  --define "TQ_RELAY_HTTP=\"${TQ_RELAY_HTTP:-}\"" "$@"
