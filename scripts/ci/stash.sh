#!/usr/bin/env bash
# stash.sh — what one job of a run hands a later one, kept on `dax-build`
# rather than as a GitHub artifact (#3554). GitHub counts artifact storage
# across the account over its billing month, and a run of `main` that hit
# the quota failed at its first upload (run 40316950733), though every job
# that reads a hand-off runs on the server that could have kept it.
#
#   stash.sh put <name> <path> [--exclude <path under it>]... [--days <n>]
#   stash.sh get <dest> <name>... [--merge] [--optional]
#
# `put` keeps <path>, a file or a directory, as <name> of this run
# (GITHUB_RUN_ID), less each `--exclude`, for <n> days (7 where unnamed),
# and makes it `latest/<name>`; a run attempted again keeps the later put.
# A <path> that is not there is a warning and keeps nothing, as
# upload-artifact's `if-no-files-found: warn` was. `get` lays each <name>
# of this run at <dest>, or under <dest>/<name> where several are named
# without `--merge`, as download-artifact did; a name this run kept none of
# fails it, or is a note under `--optional`.
#
# The stash is DAX_STASH: a directory on the server, `~/dax-stash` where
# unset on Linux, or `<user>@<host>:<directory>` from another machine,
# reached by rsync over SSH with the key DAX_STASH_KEY names
# (~/.ssh/dax-stash where unset and present). runner.sh writes both into
# the Mac's runner environment (`runner.sh mac --stash`), and the wizard
# authorizes the key for the server's runner user; the server's own jobs
# need no rsync. Each entry is one gzipped tar, `runs/<run id>/<name>.tar.gz`,
# so a put over rsync replaces it whole and needs no command on the far side; `latest/<name>.tar.gz` is a
# link to the newest, which `xtask phpt --against`'s fetch line reads for
# `phpt-runs` (#2362). Every put and get on the server drops the entries
# past their days, but none a `latest` link names.
set -euo pipefail

die() { printf 'stash.sh: %s\n' "$*" >&2; exit 1; }
# The scratch a put stages in and a get fetches into, gone at exit.
stage="" got=""
trap 'rm -rf "$stage" "$got"' EXIT

stash=${DAX_STASH:-}
if [ -z "$stash" ]; then
    [ "$(uname -s)" = Linux ] || die "DAX_STASH is unset: off the server it names the server's stash, <user>@<host>:dax-stash, which runner.sh mac --stash writes"
    stash=$HOME/dax-stash
fi
run=${GITHUB_RUN_ID:?the stash keys each entry by the run, GITHUB_RUN_ID}
case $stash in
    /*) remote="" ;;
    *:*) remote=1 ;;
    *) die "DAX_STASH is an absolute directory or <user>@<host>:<directory>, not $stash" ;;
esac
if [ -n "$remote" ]; then
    command -v rsync >/dev/null || die "no rsync on this machine, which reaches the stash by it"
    key=${DAX_STASH_KEY:-$HOME/.ssh/dax-stash}
    rsh="ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new"
    [ ! -f "$key" ] || rsh="$rsh -i $key"
    export RSYNC_RSH=$rsh
fi

# The entries past their days, on the server alone: each `.expires` holds
# the second its entry may go.
prune() {
    [ -z "$remote" ] && [ -d "$stash/runs" ] || return 0
    local now kept expires entry
    now=$(date +%s)
    kept=$(for link in "$stash"/latest/*.tar.gz; do [ -L "$link" ] && readlink "$link"; done)
    for expires in "$stash"/runs/*/*.expires; do
        [ -f "$expires" ] && [ "$(cat "$expires")" -lt "$now" ] || continue
        entry=${expires%.expires}.tar.gz
        grep -qxF "../runs/$(basename "$(dirname "$entry")")/$(basename "$entry")" <<<"$kept" && continue
        rm -f "$entry" "$expires"
    done
    find "$stash/runs" -mindepth 1 -maxdepth 1 -type d -empty -delete
}

put() {
    local name=${1:?put names the entry} path=${2:?put names the path it keeps} days=7 excludes=()
    shift 2
    while [ $# -gt 0 ]; do
        case $1 in
            --exclude) excludes+=("--exclude=./${2%/}"); shift 2 ;;
            --days) days=$2; shift 2 ;;
            *) die "unknown argument $1" ;;
        esac
    done
    if [ ! -e "$path" ]; then
        echo "::warning title=nothing stashed::no $path, so this run keeps no $name"
        return 0
    fi
    prune
    stage=$(mktemp -d)
    mkdir -p "$stage/runs/$run" "$stage/latest"
    if [ -d "$path" ]; then
        tar -C "$path" ${excludes[@]+"${excludes[@]}"} -czf "$stage/runs/$run/$name.tar.gz" .
    else
        tar -C "$(dirname "$path")" -czf "$stage/runs/$run/$name.tar.gz" "$(basename "$path")"
    fi
    echo $(($(date +%s) + days * 86400)) > "$stage/runs/$run/$name.expires"
    ln -s "../runs/$run/$name.tar.gz" "$stage/latest/$name.tar.gz"
    echo "keeping $path as $name of run $run in $stash, for $days days ($(du -h "$stage/runs/$run/$name.tar.gz" | cut -f1))"
    if [ -n "$remote" ]; then
        rsync -a "$stage/" "$stash/"
    else
        mkdir -p "$stash/runs/$run" "$stash/latest"
        mv -f "$stage/runs/$run/$name.tar.gz" "$stage/runs/$run/$name.expires" "$stash/runs/$run/"
        ln -sfn "../runs/$run/$name.tar.gz" "$stash/latest/$name.tar.gz"
    fi
}

get() {
    local dest=${1:?get names where it lays the entries} names=() merge="" optional="" missing=0
    shift
    while [ $# -gt 0 ]; do
        case $1 in
            --merge) merge=1; shift ;;
            --optional) optional=1; shift ;;
            *) names+=("$1"); shift ;;
        esac
    done
    [ ${#names[@]} -gt 0 ] || die "get names at least one entry"
    [ ${#names[@]} -gt 1 ] || merge=1
    prune
    got=$(mktemp -d)
    for name in "${names[@]}"; do
        local at=$dest
        [ -n "$merge" ] || at=$dest/$name
        local entry=$stash/runs/$run/$name.tar.gz status=0
        if [ -n "$remote" ]; then
            rsync -a "$entry" "$got/" 2>"$got/err" || status=$?
            # rsync's 23 is a source that is not there; any other failure
            # is the stash's, which no `--optional` passes over.
            if [ "$status" -ne 0 ] && [ "$status" -ne 23 ]; then
                cat "$got/err" >&2
                die "the stash at $stash did not answer (rsync exit $status)"
            fi
            entry=$got/$name.tar.gz
        elif [ ! -f "$entry" ]; then
            status=23
        fi
        if [ "$status" -eq 23 ]; then
            if [ -n "$optional" ]; then
                echo "run $run kept no $name; went on without it"
            else
                echo "::error title=nothing to get::run $run kept no $name in $stash"
                missing=1
            fi
            continue
        fi
        mkdir -p "$at"
        tar -xzf "$entry" -C "$at"
        echo "laid $name of run $run at $at"
    done
    return "$missing"
}

case ${1:-} in
    put | get) "$@" ;;
    *) die "usage: stash.sh put <name> <path> [--exclude <path>]... [--days <n>] | get <dest> <name>... [--merge] [--optional]" ;;
esac
