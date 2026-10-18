#!/usr/bin/env bash
# oracle-variants.sh — each oracle variant the pin names for this machine,
# laid at its path and held to its md5, as pinned-php.sh lays the keg
# (#2298; #1782, #1889): built from the pinned tag by scripts/oracle-build.py,
# the main oracle's configure line plus the variant's flags, wherever the
# binary at the path is absent or has another md5.
#
#   scripts/ci/oracle-variants.sh <oracle.json>
#
# <oracle.json> is `xtask status --section oracle --format json`, rendered
# once the pinned php-src is named to the harness (the corpus action): its
# `variants` name each build this machine's platform pins, by path and md5,
# and `found.path` the tree to build from. oracle.toml is read by xtask and
# the recipe alone, and this script spells none of it. A binary already at
# the path with the pin's md5 is said and left, so the oracle runner builds
# once and every job after it checks. A build whose md5 is not the pin's is
# laid nowhere and exits 1: this machine's libraries or tools are not the
# ones the pin was built with, and which side is right is a person's to
# settle. The path's directory is the runner user's, laid by `runner.sh
# mac`, so nothing here runs as root.
#
# DRY_RUN=1 says what it would build and builds nothing. ORACLE_BUILD names
# the recipe, `python3 scripts/oracle-build.py` where unset (Python 3.11 or
# later, for tomllib; `runner.sh mac` installs it with the build's tools).
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
pin=${1:?usage: oracle-variants.sh <oracle.json from xtask status --section oracle --format json>}
recipe=(python3 "$here/../oracle-build.py")
[ -z "${ORACLE_BUILD:-}" ] || recipe=("$ORACLE_BUILD")
dry=${DRY_RUN:-}
builds=$(jq -r '.variants // {} | to_entries[] | "\(.key) \(.value.platform) \(.value.md5) \(.value.path)"' "$pin")
[ -n "$builds" ] || { echo "no oracle variant pins a build for this machine"; exit 0; }

digest() { if command -v md5 >/dev/null; then md5 -q "$1"; else md5sum "$1" | cut -d' ' -f1; fi; }

while read -r name platform md5 path; do
    if [ -f "$path" ] && [ "$(digest "$path")" = "$md5" ]; then
        echo "$name: $path has md5 $md5, the pin's; already laid"
        continue
    fi
    if [ -n "$dry" ]; then
        echo "$name: would build for $platform and lay it at $path"
        continue
    fi
    tree=$(jq -er .found.path "$pin") ||
        { echo "::error::no pinned php-src to build $name from: $(jq -r .found.error "$pin")"; exit 1; }
    dir=$(dirname "$path")
    { mkdir -p "$dir" 2>/dev/null && [ -w "$dir" ]; } ||
        { echo "::error::$dir is not $(id -un)'s to write; scripts/ci/runner.sh mac lays it"; exit 1; }
    now=absent
    [ ! -f "$path" ] || now="md5 $(digest "$path"), not the pin's $md5"
    echo "$name: $path is $now; building it for $platform from $tree"
    # The recipe's last lines are `<md5>  <binary>` and the pin's table; the
    # build's own output goes to the log.
    built=$("${recipe[@]}" --tree "$tree" build "$platform" "$name" | tee /dev/stderr |
        awk 'NF == 2 && length($1) == 32 && $1 ~ /^[0-9a-f]+$/ { print $2 }' | tail -n 1)
    [ -n "$built" ] || { echo "::error::${recipe[*]} named no binary it built for $name"; exit 1; }
    got=$(digest "$built")
    [ "$got" = "$md5" ] || {
        echo "::error::$name: the build at $built has md5 $got and the pin is $md5, so this machine does not reproduce the pinned build; it is laid nowhere, and the difference is a person's to settle (#2298)"
        exit 1
    }
    cp "$built" "$path.partial"
    mv "$path.partial" "$path"
    echo "$name: $path has md5 $got, the pin's; built and laid"
done <<<"$builds"
