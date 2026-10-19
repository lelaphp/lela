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

target=target/tailcall
read -ra xtask <<<"${XTASK:-cargo xtask}"
export RUSTFLAGS='--cfg dispatch="tailcall"'

case ${1:-} in
check | phpt) ;;
*)
    echo "usage: scripts/ci/tailcall.sh check | phpt <corpus> <tree>" >&2
    exit 2
    ;;
esac

rustup toolchain install "$TOOLCHAIN" --profile minimal

case $1 in
check)
    cargo "+$TOOLCHAIN" check -p php_engine --target-dir "$target"
    ;;
phpt)
    corpus=${2:?the file the corpus action wrote the corpus section to}
    tree=${3:?the pinned php-src, as the corpus action checked it out}
    packages=()
    while read -r package; do packages+=(-p "$package"); done < <(jq -er '.hosts[].package' "$corpus")
    [ ${#packages[@]} -gt 0 ] || { echo "::error::xtask status --section corpus names no host"; exit 1; }
    echo "the hosts built under the cfg: ${packages[*]}"
    cargo "+$TOOLCHAIN" build --profile release-da --target-dir "$target" "${packages[@]}"
    # The harness is built on the pinned toolchain with no cfg.
    unset RUSTFLAGS
    # `phpt` keeps a whole subset's run by the tree the engine is built
    # from (xtask/src/phpt/against.rs), which the threaded build shares
    # with the shipped one, so the kept runs are put back as they stood
    # once the lane has run: a base `--against` or `done` reads is never
    # the threaded build's. The targets under the pin are kept there too.
    runs=target/harness/phpt-runs
    aside=$(mktemp -d)
    if [ -d "$runs" ]; then cp -cR "$runs" "$aside/" 2>/dev/null || cp -R "$runs" "$aside/"; fi
    trap 'rm -rf "$runs"; if [ -d "$aside/phpt-runs" ]; then mv "$aside/phpt-runs" "$runs"; fi; rm -rf "$aside"' EXIT
    engine=$target/release-da/php
    failed=0
    "${xtask[@]}" phpt --engine "$engine" --subset Zend/tests || failed=1
    for dir in "$tree/tests/"*/; do
        "${xtask[@]}" phpt --engine "$engine" --subset "tests/$(basename "$dir")" || failed=1
    done
    exit "$failed"
    ;;
esac
