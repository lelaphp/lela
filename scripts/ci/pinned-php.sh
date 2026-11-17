#!/usr/bin/env bash
# pinned-php.sh — the pinned php and the kegs it loads on an Apple-silicon
# Mac, poured the way they were poured on the machine that pinned them
# (#2253, #2657; harness §4.3): each from homebrew-core's own formula at the
# commit that bottled the recorded version, installed from a local tap, so
# brew fetches that bottle from homebrew/core's packages and relocates and
# signs it for /opt/homebrew as it did there.
#
#   scripts/ci/pinned-php.sh <oracle.json>
#
# <oracle.json> is `xtask status --section oracle --format json`: its
# `binary` names the Homebrew formula php is poured from, the bottle of it,
# its keg path and md5, and its `closure` each keg php loads and the version
# recorded for it; oracle.toml is read by xtask alone and this script spells
# none of them. Each keg is poured first, where Homebrew's `opt` link points
# at another version, and `brew pin`ned, so no later `brew upgrade` moves
# it; a keg already at its version is pinned and left. Then php, from the
# formula the pin names: `php` while that is Homebrew's formula of the
# pinned line, and `php@8.5` once Homebrew's `php` has moved to the next
# (#4383), whose keg is `Cellar/php@8.5`; xtask renders no pin whose formula
# and keg path name two kegs. The bottle is the keg the path names, php's
# version with the formula's revision where Homebrew bottled that version
# again: `Cellar/php@8.5/8.5.13_1` is poured from `php@8.5: update 8.5.13_1
# bottle.`, where the pin's version says 8.5.13, and xtask renders no pin
# whose path names a bottle of another version. A keg already at the path
# with the pin's md5 is left as it is, so the oracle runner pours once and
# every job after it checks; a pour whose md5 is not the pin's, or a keg
# that lands at another version, exits 1 before any lane, and xtask would
# refuse the run anyway. php's keg is `brew pin`ned as each keg it loads
# is, poured here or found already poured, so a `brew upgrade` leaves it
# where the pin's path names it, and xtask refuses a run over one that is
# not (#4383).
#
# A keg is named to brew so that no alias of homebrew-core's stands in for
# it (#4423): `php@8.5` is an alias of `php` there until Homebrew makes it
# a formula, and brew resolves a bare name through those aliases before
# the tap's formula wherever no keg of that name stands. So a pin names the
# formula a keg was poured from as brew itself names it, `dax/pin/php@8.5`
# for one poured here and `zstd` for one of homebrew-core's, by the tap its
# receipt records, as xtask's remedy for an unpinned keg names it; the
# unpin before a pour names the tap's formula; and the uninstall, which
# takes every keg of the name whichever tap poured it, is asked only where
# a keg of that name stands, where brew finds it before any alias. brew's
# own unpin after that uninstall still goes through the alias, so a pin it
# drops on another keg is put back.
#
# DRY_RUN=1 reads the pin and the machine and says what it would pour, each
# keg and php by its formula and its bottle, and pours, pins and fetches
# nothing.
#
# Used by .github/actions/pinned-php in CI and by scripts/ci/runner.sh when
# the Mac is set up. GH_TOKEN, where set, raises GitHub's API rate limit for
# the queries that find the bottling commits.
set -euo pipefail

pin=${1:?usage: pinned-php.sh <oracle.json from xtask status --section oracle --format json>}
formula=$(jq -er .binary.formula "$pin")
bottle=$(jq -er .binary.bottle "$pin")
path=$(jq -er .binary.path "$pin")
md5=$(jq -er .binary.md5 "$pin")
kegs=$(jq -r '.closure // {} | to_entries[] | "\(.key) \(.value)"' "$pin")
dry=${DRY_RUN:-}

export HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_CLEANUP=1
export HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK=1 HOMEBREW_NO_ANALYTICS=1
prefix=$(brew --prefix)

auth=()
[ -z "${GH_TOKEN:-}" ] || auth=(-H "Authorization: Bearer $GH_TOKEN")

# pour <formula> <bottle>: the formula from the homebrew-core commit whose
# message bottles that version, its revision with it, installed in place of
# what stands. A formula's first bottle is added and each later one updates
# it: homebrew-core has `php@8.4: add 8.4.15 bottle.` from the day it made
# that formula, and `php@8.4: update 8.4.16 bottle.` after it.
pour() {
    local name=$1 at=$2 shard sha file
    case $name in lib*) shard=lib ;; *) shard=${name:0:1} ;; esac
    sha=$(curl -sSfL ${auth[@]+"${auth[@]}"} \
        "https://api.github.com/repos/Homebrew/homebrew-core/commits?path=Formula/$shard/$name.rb&per_page=100" |
        jq -r --arg update "$name: update $at bottle." --arg add "$name: add $at bottle." \
            '[.[] | select(.commit.message | startswith($update) or startswith($add))][0].sha // empty')
    [ -n "$sha" ] || { echo "::error::no homebrew-core commit bottles $name $at"; exit 1; }
    echo "$name $at from homebrew-core $sha"
    # The tap's formula is written first, so the unpin names it.
    brew tap | grep -qx dax/pin || brew tap-new --no-git dax/pin >/dev/null
    file="$(brew --repository dax/pin)/Formula/$name.rb"
    curl -sSfL -o "$file" "https://raw.githubusercontent.com/Homebrew/homebrew-core/$sha/Formula/$shard/$name.rb"
    # A tap's bottles are the tap's own packages by default; these are homebrew/core's.
    sed -i '' 's|^  bottle do$|  bottle do\n    root_url "https://ghcr.io/v2/homebrew/core"|' "$file"
    brew unpin "dax/pin/$name" >/dev/null 2>&1 || true
    if [ -d "$prefix/Cellar/$name" ]; then
        # Once the rack is gone, the uninstall unpins the formula the bare
        # name resolves to (`Uninstall.rm_pin`), another formula's while the
        # name is an alias of homebrew-core's; each pin it drops beside the
        # keg's own is put back, by the name a pin gives it.
        local pins=$prefix/var/homebrew/pinned before other
        before=$(ls "$pins" 2>/dev/null || true)
        brew uninstall --ignore-dependencies --force "$name" >/dev/null 2>&1 || true
        for other in $before; do
            [ "$other" != "$name" ] && [ ! -L "$pins/$other" ] || continue
            brew pin "$(named "$prefix/opt/$other" "$other")" ||
                echo "::warning::brew dropped the pin of $other with $name, and pinning it again failed"
        done
    fi
    # Every keg php loads is poured here at its record, so none is brew's to
    # resolve: --ignore-dependencies keeps brew from upgrading one to its
    # newest, and from refusing a pour over a pinned one it would call old.
    brew install --ignore-dependencies "dax/pin/$name"
}

# named <keg> <formula>: the formula the keg at that directory was poured
# from, as brew names a formula (`Formula#full_name`): `<tap>/<formula>`
# for a tap's, `dax/pin/php@8.5`, and the bare name for homebrew-core's or
# a keg whose receipt names no tap.
named() {
    local tap
    tap=$(jq -r '.source.tap // empty' "$1/INSTALL_RECEIPT.json" 2>/dev/null) || tap=
    case $tap in
    "" | homebrew/core) echo "$2" ;;
    *) echo "$tap/$2" ;;
    esac
}

# The version Homebrew's `opt` link points a keg at, empty where none is.
linked() {
    local target
    target=$(readlink "$prefix/opt/$1") || return 0
    basename "$target"
}

# The list is on a descriptor of its own, so nothing a pour runs can read a
# keg's line off standard input and end the loop before the kegs after it.
while read -r keg at <&3; do
    [ -n "$keg" ] || continue
    now=$(linked "$keg")
    if [ "$now" = "$at" ]; then
        echo "$keg $at: the record's; already poured"
    elif [ -n "$dry" ]; then
        echo "$keg: would pour $at in place of ${now:-nothing}"
        continue
    else
        pour "$keg" "$at"
        now=$(linked "$keg")
        [ "$now" = "$at" ] || { echo "::error::$keg is ${now:-not installed}; the record is $at"; exit 1; }
    fi
    [ -n "$dry" ] || brew pin "$(named "$prefix/opt/$keg" "$keg")"
done 3<<<"$kegs"

if [ -f "$path" ] && [ "$(md5 -q "$path")" = "$md5" ]; then
    echo "$path: md5 $md5, the pin's; already poured"
elif [ -n "$dry" ]; then
    echo "$formula: would pour $bottle at $path"
    exit 0
else
    pour "$formula" "$bottle"
    got=$(md5 -q "$path")
    [ "$got" = "$md5" ] || { echo "::error::$path has md5 $got; the pin is $md5"; exit 1; }
    echo "$path: md5 $got, the pin's"
fi
# The keg stands at the pin's md5 here, poured now or before, as a keg of
# the closure stands at its record where the loop above pins it.
[ -n "$dry" ] || brew pin "$(named "${path%%/Cellar/*}/Cellar/$formula/$bottle" "$formula")"
