#!/usr/bin/env bash
# bound-target.sh — a self-hosted runner's target/ held under the bound
# runner.sh states (#2299). The runner keeps target/ between jobs as its
# warm build, and cargo removes nothing it no longer needs: each dependency
# or toolchain bump and each profile adds to it, and so do the phpt runs
# kept under target/harness/phpt-runs/, until the disk fills and a job fails
# for a reason no reading names.
#
#   scripts/ci/bound-target.sh [<workspace root>]
#
# Every building job runs it, through .github/actions/workspace, once the
# toolchain rust-toolchain.toml names is installed. It says target/'s size,
# which is how the runner's growth is measured, and cleans target/ whole
# but the targets under the pin when either:
#   - target/ is past DAX_TARGET_GIB, the bound runner.sh writes into the
#     runner's environment; unset, as on a hosted runner, which starts
#     empty, nothing is bounded; or
#   - the toolchain moved since the last job, as target/.toolchain records
#     it, when the old toolchain's artifacts are no use to any later build;
#     the old toolchain itself is uninstalled too.
# The targets kept are `xtask status --section corpus`'s `targets`, the
# pinned php's verdicts on each subset under the pin, one directory per
# oracle a subset runs under, taken again only when the pin moves; the
# section is rendered by $XTASK (`cargo xtask` where unset) only when a
# clean is due. A target of an older pin goes with everything else.
set -euo pipefail

root=$(cd "${1:-.}" && pwd)
target=$root/target
read -ra xtask <<<"${XTASK:-cargo xtask}"
bound=${DAX_TARGET_GIB:-}
toolchain=$(rustup show active-toolchain | cut -d' ' -f1)
was=$(cat "$target/.toolchain" 2>/dev/null || true)
kib=$(du -sk "$target" 2>/dev/null | cut -f1 || true)
gib=$(awk -v k="${kib:-0}" 'BEGIN { printf "%.1f", k / 1048576 }')
of="held to a bound of $bound GiB"
[ -n "$bound" ] || of="with no bound, as DAX_TARGET_GIB is unset"
echo "target/ is $gib GiB, $of; the toolchain is $toolchain"

why=""
if [ -n "$was" ] && [ "$was" != "$toolchain" ]; then
    why="the toolchain moved from $was to $toolchain"
elif [ -n "$bound" ] && [ "${kib:-0}" -gt $((bound * 1048576)) ]; then
    why="target/ is past its bound of $bound GiB"
fi

# prune <dir> <kept>...: everything under <dir> removed but each kept path
# and the directories that lead to one.
prune() {
    local dir=$1 entry kept on
    shift
    for entry in "$dir"/* "$dir"/.[!.]*; do
        [ -e "$entry" ] || continue
        on=""
        for kept; do
            [ "$entry" != "$kept" ] || continue 2
            case $kept in "$entry"/*) on=1 ;; esac
        done
        if [ -n "$on" ]; then prune "$entry" "$@"; else rm -rf "$entry"; fi
    done
}

if [ -n "$why" ]; then
    keep=()
    while read -r path; do keep+=("$root/$path"); done < <(
        "${xtask[@]}" status --section corpus --format json | jq -er '.targets[]')
    [ ${#keep[@]} -gt 0 ] || { echo "::error::xtask status --section corpus names no targets"; exit 1; }
    echo "$why: cleaning target/ but the targets under the pin, ${keep[*]#"$root/"}"
    prune "$target" "${keep[@]}"
    if [ -n "$was" ] && [ "$was" != "$toolchain" ] && rustup toolchain list | cut -d' ' -f1 | grep -qxF "$was"; then
        rustup toolchain uninstall "$was"
    fi
    kib=$(du -sk "$target" | cut -f1)
    echo "target/ is $(awk -v k="$kib" 'BEGIN { printf "%.1f", k / 1048576 }') GiB after the clean"
fi
mkdir -p "$target"
echo "$toolchain" >"$target/.toolchain"
