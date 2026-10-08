#!/usr/bin/env bash
# pinned-php.sh — the pinned php on an Apple-silicon Mac, poured the way it
# was poured on the machine that pinned it (#2253; harness §4.3):
# homebrew-core's own formula at the commit that bottled the pin's version,
# installed from a local tap, so brew fetches that bottle from homebrew/core's
# packages and relocates and signs it for /opt/homebrew as it did there.
#
#   scripts/ci/pinned-php.sh <oracle.json>
#
# <oracle.json> is `xtask status --section oracle --format json`, whose
# `binary` names the version, the keg path and the md5; oracle.toml is read
# by xtask alone and this script spells none of the three. A keg already at
# the path with the pin's md5 is left as it is, so the oracle runner pours
# once and every job after it checks; a pour whose md5 is not the pin's
# exits 1, before any lane, and xtask would refuse it anyway.
#
# Used by .github/actions/pinned-php in CI and by scripts/ci/runner.sh when
# the Mac is set up. GH_TOKEN, where set, raises GitHub's API rate limit for
# the one query that finds the bottling commit.
set -euo pipefail

pin=${1:?usage: pinned-php.sh <oracle.json from xtask status --section oracle --format json>}
version=$(jq -er .binary.version "$pin")
path=$(jq -er .binary.path "$pin")
md5=$(jq -er .binary.md5 "$pin")

if [ -f "$path" ] && [ "$(md5 -q "$path")" = "$md5" ]; then
    echo "$path: md5 $md5, the pin's; already poured"
    exit 0
fi

export HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_CLEANUP=1
export HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK=1 HOMEBREW_NO_ANALYTICS=1

auth=()
[ -z "${GH_TOKEN:-}" ] || auth=(-H "Authorization: Bearer $GH_TOKEN")
sha=$(curl -sSfL ${auth[@]+"${auth[@]}"} \
    "https://api.github.com/repos/Homebrew/homebrew-core/commits?path=Formula/p/php.rb&per_page=100" |
    jq -r --arg m "php: update $version bottle." '[.[] | select(.commit.message | startswith($m))][0].sha // empty')
[ -n "$sha" ] || { echo "::error::no homebrew-core commit bottles php $version"; exit 1; }
echo "php $version from homebrew-core $sha"

brew uninstall --ignore-dependencies --force php >/dev/null 2>&1 || true
brew tap | grep -qx dax/pin || brew tap-new --no-git dax/pin >/dev/null
formula="$(brew --repository dax/pin)/Formula/php.rb"
curl -sSfL -o "$formula" "https://raw.githubusercontent.com/Homebrew/homebrew-core/$sha/Formula/p/php.rb"
# A tap's bottles are the tap's own packages by default; these are homebrew/core's.
sed -i '' 's|^  bottle do$|  bottle do\n    root_url "https://ghcr.io/v2/homebrew/core"|' "$formula"
brew install dax/pin/php

got=$(md5 -q "$path")
[ "$got" = "$md5" ] || { echo "::error::$path has md5 $got; the pin is $md5"; exit 1; }
echo "$path: md5 $got, the pin's"
