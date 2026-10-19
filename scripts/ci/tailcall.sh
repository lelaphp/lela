#!/usr/bin/env bash
# tailcall.sh — the lane that holds the threaded dispatch, the engine built
# under `cfg(dispatch = "tailcall")` (#2905), on a dated nightly, as Mitchell
# ruled on #2970 (harness-design.md §7; design §5.8). The `match` loop on
# the pinned stable toolchain, rust-toolchain.toml's, is what ships, and no
# build ships the threaded variant: it needs `explicit_tail_calls` and
# `rust_preserve_none_cc`, both gated on every toolchain, so it is built on
# the one nightly TOOLCHAIN names and nowhere else. Without the lane it
# would stop compiling, or stop holding the corpus, with no reading going
# red.
#
#   scripts/ci/tailcall.sh check          per commit (harness.yml): php_engine
#                                         checked under the cfg
#   scripts/ci/tailcall.sh phpt <corpus> <tree>
#                                         nightly (dashboard.yml): the hosts
#                                         <corpus> names built under the cfg
#                                         in release-da, then `Zend/tests`
#                                         and each directory of <tree>'s
#                                         `tests/` with `--engine` on that
#                                         build
#   scripts/ci/tailcall.sh restore        the runs `phpt` keeps put back as
#                                         they stood before `phpt`, where a
#                                         killed run left them set aside
#
# <corpus> and <tree> are the corpus action's outputs: the file `xtask
# status --section corpus --format json` was written to, whose `hosts` are
# the packages built, as the oracle job builds the shipped hosts, and the
# pinned php-src it checked out. $XTASK runs the harness
# (`cargo xtask` where unset), on the pinned stable toolchain: only the
# engine is the nightly's.
#
# Every build is in target/tailcall, a target directory of its own, so the
# cfg's RUSTFLAGS never invalidate the warm build of the shipped engine.
# A subset that fails fails the run once every subset has run; the
# readings are the lane's alone, uploaded apart from what the dashboard
# folds, and the runs `phpt` keeps are put back as they stood, so the
# threaded build's are never read as the shipped engine's.
#
# The nightly is TOOLCHAIN below, and moving it is that one line. A nightly
# that breaks the check is moved past, not followed.
set -euo pipefail

TOOLCHAIN=nightly-2026-10-15

# The workspace root, whatever directory it is run from.
cd "$(dirname "$0")/../.."
target=target/tailcall
read -ra xtask <<<"${XTASK:-cargo xtask}"
export RUSTFLAGS='--cfg dispatch="tailcall"'

# `phpt` keeps a whole subset's run by the tree the engine is built from
# (xtask/src/phpt/against.rs), which the threaded build shares with the
# shipped one, so the kept runs, the targets under the pin among them, are
# set aside before the subsets and put back as they stood after: a base
# `--against` or `done` reads is never the threaded build's. The EXIT trap
# puts them back, and dashboard.yml's step after the subsets runs `restore`
# whatever ended them, a timeout included.
runs=target/harness/phpt-runs
aside=$target/phpt-runs-aside
restore() {
    [ -d "$aside" ] || return 0
    rm -rf "$runs" || return 1
    mv "$aside" "$runs"
}

usage="usage: scripts/ci/tailcall.sh check | phpt <corpus> <tree> | restore"
case ${1:-} in
check) ;;
phpt)
    corpus=${2:?$usage}
    tree=${3:?$usage}
    ;;
restore)
    restore
    exit
    ;;
*)
    echo "$usage" >&2
    exit 2
    ;;
esac

rustup toolchain install "$TOOLCHAIN" --profile minimal

case $1 in
check)
    cargo "+$TOOLCHAIN" check -p php_engine --target-dir "$target"
    ;;
phpt)
    packages=()
    while read -r package; do packages+=(-p "$package"); done < <(jq -r '.hosts[].package' "$corpus")
    [ ${#packages[@]} -gt 0 ] || { echo "::error::xtask status --section corpus names no host"; exit 1; }
    echo "the hosts built under the cfg: ${packages[*]}"
    cargo "+$TOOLCHAIN" build --profile release-da --target-dir "$target" "${packages[@]}"
    # The harness is built on the pinned toolchain with no cfg.
    unset RUSTFLAGS
    # A set-aside a killed run left behind is older than the runs the
    # shipped lane has kept since, so it is dropped, not put back.
    rm -rf "$aside"
    mkdir -p "$target"
    if [ -d "$runs" ]; then
        cp -cR "$runs" "$aside" 2>/dev/null || { rm -rf "$aside"; cp -R "$runs" "$aside"; }
    else
        mkdir -p "$aside"
    fi
    trap restore EXIT
    engine=$target/release-da/php
    failed=0
    "${xtask[@]}" phpt --engine "$engine" --subset Zend/tests || failed=1
    for dir in "$tree/tests/"*/; do
        "${xtask[@]}" phpt --engine "$engine" --subset "tests/$(basename "$dir")" || failed=1
    done
    exit "$failed"
    ;;
esac
