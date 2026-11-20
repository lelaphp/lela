#!/bin/sh
# Every apt step of dist/Dockerfile, which each RUN that installs packages
# bind-mounts at /tmp/apt.sh, so no image keeps it:
#
#   sh /tmp/apt.sh update
#   sh /tmp/apt.sh install <package>...
#
# A package mirror answers 5xx for a moment now and then. apt retries a
# fetch itself (Acquire::Retries) only when it reads the failure as
# transient: a 408, 429, 500, 502, 503, 504 or 599 that carries a body, a
# timeout, a refused connection or a failed lookup. A 502 with no body is
# a hard failure, retried by nothing: `update` reports it as an error on
# InRelease, and then that the repository is not signed, and exits 100,
# and `install` as an error on the .deb. So each step that fetches runs
# again, up to TRIES times. A transient failure that outlasts apt's own
# retries fails `update` too (APT::Update::Error-Mode "any"), where by
# default it would warn, exit 0 and leave `install` to find no packages.
#
# A mirror that stays down fails the step with apt's own messages and its
# exit status, the last try's. `install` fetches with --download-only and
# installs with --no-download, so a failure that is not a fetch, in dpkg
# or a maintainer script, runs once.
set -eu

TRIES=4
# The seconds between tries, the nth wait being n times this.
WAIT=15
OPTIONS="-o Acquire::Retries=5 -o APT::Update::Error-Mode=any"

# Run "$@" until it exits 0, at most TRIES times, and exit with the last
# try's status; $command names the step in what it says.
retry() {
    try=1
    while :; do
        status=0
        "$@" || status=$?
        [ "$status" -ne 0 ] || return 0
        if [ "$try" -ge "$TRIES" ]; then
            echo "apt.sh: $command failed $try times, the last with exit $status" >&2
            exit "$status"
        fi
        echo "apt.sh: $command exited $status, try $try of $TRIES; trying again in $((try * WAIT))s" >&2
        sleep "$((try * WAIT))"
        try=$((try + 1))
    done
}

[ "$#" -ge 1 ] || { echo "usage: apt.sh update | apt.sh install <package>..." >&2; exit 2; }
command="$1"
shift
# shellcheck disable=SC2086 # OPTIONS is words, split on purpose.
case "$command" in
    update)
        [ "$#" -eq 0 ] || { echo "apt.sh: update takes no arguments" >&2; exit 2; }
        retry apt-get $OPTIONS update -qq
        ;;
    install)
        [ "$#" -ge 1 ] || { echo "apt.sh: install names its packages" >&2; exit 2; }
        export DEBIAN_FRONTEND=noninteractive
        retry apt-get $OPTIONS install -y -qq --no-install-recommends --download-only "$@"
        apt-get $OPTIONS install -y -qq --no-install-recommends --no-download "$@"
        ;;
    *)
        echo "apt.sh: no step $command; update or install" >&2
        exit 2
        ;;
esac
