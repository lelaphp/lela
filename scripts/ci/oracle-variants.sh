#!/usr/bin/env bash
# oracle-variants.sh — each oracle variant the pin names for this machine,
# laid at its path and held to its SHA-256, as pinned-php.sh lays the keg
# (#2298; #1782, #1889): built from the pinned tag by scripts/oracle-build.py,
# the main oracle's configure line plus the variant's flags, wherever the
# binary at the path is absent or has another SHA-256.
#
#   scripts/ci/oracle-variants.sh <oracle.json>
#
# <oracle.json> is `xtask status --section oracle --format json`, rendered
# once the pinned php-src is named to the harness (the corpus action): its
# `variants` name each build this machine's platform pins, by path and SHA-256,
# and `found.path` the tree to build from. oracle.toml is read by xtask and
# the recipe alone, and this script spells none of it. A binary already at
# the path with the pin's SHA-256 is said and left, so a machine that keeps
# one builds once. A build whose SHA-256 is not the pin's is
# laid nowhere and exits 1: this machine's libraries or tools are not the
# ones the pin was built with, and which side is right is a person's to
# settle. The refusal is kept beside the path, `.refused-sha256-<hex>`, so the
# jobs after it say it again without building again until the pin moves or
# a person removes the file. The path's directory is made the runner
# user's with `sudo` where it is not, and nothing else here runs as root.
#
# DRY_RUN=1 says what it would build and builds nothing. ORACLE_BUILD names
# the recipe, `python3 scripts/oracle-build.py` where unset (Python 3.11 or
# later, for tomllib, which the hosted macOS image carries).
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
pin=${1:?usage: oracle-variants.sh <oracle.json from xtask status --section oracle --format json>}
recipe=(python3 "$here/../oracle-build.py")
[ -z "${ORACLE_BUILD:-}" ] || read -ra recipe <<<"$ORACLE_BUILD"
dry=${DRY_RUN:-}
builds=$(jq -r '.variants // {} | to_entries[] | "\(.key) \(.value.platform) \(.value.sha256) \(.value.path)"' "$pin")
[ -n "$builds" ] || { echo "no oracle variant pins a build for this machine"; exit 0; }

# digest <file>: its SHA-256 as the pin spells one, `sha256:<hex>`.
digest() {
    local hex
    if command -v shasum >/dev/null; then hex=$(shasum -a 256 "$1"); else hex=$(sha256sum "$1"); fi
    printf 'sha256:%s\n' "${hex%% *}"
}

while read -r name platform sha256 path; do
    if [ -f "$path" ] && [ "$(digest "$path")" = "$sha256" ]; then
        echo "$name: $path has $sha256, the pin's; already laid"
        continue
    fi
    if [ -n "$dry" ]; then
        echo "$name: would build for $platform and lay it at $path"
        continue
    fi
    tree=$(jq -er .found.path "$pin") ||
        { echo "::error::no pinned php-src to build $name from: $(jq -r .found.error "$pin")"; exit 1; }
    dir=$(dirname "$path")
    # A hosted runner's user is given `sudo` with no password, and a
    # machine whose user is not is laid the directory once, by hand.
    { mkdir -p "$dir" 2>/dev/null && [ -w "$dir" ]; } ||
        sudo -n install -d -o "$(id -un)" "$dir" 2>/dev/null || {
        echo "::error::$dir is not $(id -un)'s to write, and \`sudo\` asks a password here; \`sudo install -d -o $(id -un) $dir\` lays it"
        exit 1
    }
    refused=$dir/.refused-${sha256/:/-}
    if [ -f "$refused" ]; then
        echo "::error::$name: $(cat "$refused"); not built again until the pin moves or $refused is removed (#2298)"
        exit 1
    fi
    now=absent
    [ ! -f "$path" ] || now="$(digest "$path"), not the pin's $sha256"
    echo "$name: $path is $now; building it for $platform from $tree"
    # The recipe's last lines name the binary by each digest, one of them
    # `sha256:<hex>  <binary>`, then the pin's table; the build's own
    # output goes to the log.
    built=$("${recipe[@]}" --tree "$tree" build "$platform" "$name" | tee /dev/stderr |
        awk 'NF == 2 && length($1) == 71 && $1 ~ /^sha256:[0-9a-f]+$/ { print $2 }' | tail -n 1)
    [ -n "$built" ] || { echo "::error::${recipe[*]} named no binary it built for $name"; exit 1; }
    got=$(digest "$built")
    [ "$got" = "$sha256" ] || {
        echo "a build here on $(date -u +%Y-%m-%d) has $got and the pin is $sha256" >"$refused"
        echo "::error::$name: the build at $built has $got and the pin is $sha256, so this machine does not reproduce the pinned build; it is laid nowhere, and the difference is a person's to settle (#2298)"
        exit 1
    }
    cp "$built" "$path.partial"
    mv "$path.partial" "$path"
    echo "$name: $path has $got, the pin's; built and laid"
done <<<"$builds"
