#!/usr/bin/env bash
# Release helpers. Each component is released on its own, from a tag:
#
#   component  version lives in                 tag              artifacts
#   publisher  crates/tq-publisher/Cargo.toml  publisher-vX.Y.Z  tarballs, .debs, install.sh
#   relay      crates/tq-relay/Cargo.toml      relay-vX.Y.Z     ghcr.io/<owner>/tq-relay image
#   app        crates/tq-app/Cargo.toml        app-vX.Y.Z       .dmg, .zip (self-update)
#   web        web/package.json                (none: ships inside the relay image)
#
#   dev/release.sh bump <component> <version>   # edit the version (+ Cargo.lock); no commit
#   dev/release.sh check <component> <version>  # CI: the tag matches the source
#   dev/release.sh notes <component> <tag>      # CI: commits since the previous tag
set -euo pipefail
cd "$(dirname "$0")/.."

file_of() {
  case "$1" in
    publisher | relay | app) echo "crates/tq-$1/Cargo.toml" ;;
    web) echo web/package.json ;;
    *) echo "unknown component $1 (publisher, relay, app, web)" >&2; exit 2 ;;
  esac
}

version_of() {
  local f
  f=$(file_of "$1")
  case "$f" in
    *.toml) sed -n 's/^version = "\(.*\)"/\1/p' "$f" | head -n1 ;;
    *.json) sed -n 's/^  "version": "\(.*\)",/\1/p' "$f" | head -n1 ;;
  esac
}

# What a component's release contains, for its notes.
paths_of() {
  case "$1" in
    publisher) echo crates/tq-publisher crates/tq-proto crates/tq-transport crates/tq-media build packaging/publisher config ;;
    relay) echo crates/tq-relay crates/tq-proto crates/tq-transport build packaging/relay web ;;
    app) echo crates/tq-app crates/tq-proto crates/tq-transport crates/tq-media build packaging/app ;;
  esac
}

cmd=${1:-}
comp=${2:-}
arg=${3:-}
[ -n "$cmd" ] && [ -n "$comp" ] && [ -n "$arg" ] || { sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'; exit 2; }

case "$cmd" in
  bump)
    echo "$arg" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$' || { echo "not a semver: $arg" >&2; exit 2; }
    f=$(file_of "$comp")
    old=$(version_of "$comp")
    case "$f" in
      *.toml)
        perl -0pi -e "s/\nversion = \"\Q$old\E\"/\nversion = \"$arg\"/" "$f"
        cargo update -p "tq-$comp" --offline --quiet # the lock records member versions
        ;;
      *.json) perl -0pi -e "s/\n  \"version\": \"\Q$old\E\",/\n  \"version\": \"$arg\",/" "$f" ;;
    esac
    echo "$comp $old -> $(version_of "$comp") ($f)"
    if [ "$comp" = web ]; then
      echo "web ships in the relay image: release the relay to deploy it."
    else
      echo "then: git commit -am \"$comp $arg\" && git tag $comp-v$arg && git push origin HEAD $comp-v$arg"
    fi
    ;;
  check)
    have=$(version_of "$comp")
    if [ "$have" != "$arg" ]; then
      echo "::error::tag says $comp $arg but $(file_of "$comp") says $have"
      exit 1
    fi
    echo "$comp $arg matches $(file_of "$comp")"
    ;;
  notes)
    prev=$(git describe --tags --abbrev=0 --match "$comp-v*" "$arg^" 2>/dev/null || true)
    range=${prev:+$prev..}$arg
    echo "## Changes${prev:+ since $prev}"
    echo
    # shellcheck disable=SC2046
    git log --no-merges --format='- %s (%h)' "$range" -- $(paths_of "$comp") | head -n 200
    ;;
  *) echo "unknown command $cmd" >&2; exit 2 ;;
esac
