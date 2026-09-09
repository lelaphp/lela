#!/usr/bin/env bash
# worktree-retire.sh — append a worktree's readings to its track's store,
# then remove the worktree (harness-design.md §4.2). A worktree's readings
# live in its own `target/harness/`, so removing the worktree removes them;
# retiring is where a track keeps its evidence, so the append-only log goes
# into the track's store first and a merged agent's denials stay in the
# section the console reads, `status --section permits`.
#
# The track's store is the store of the checkout the worktree was added
# from: the repository's main worktree, which is the track's root. The
# branch is kept — deleting it is a person's line, printed at the end.
#
# usage: scripts/worktree-retire.sh <branch>
# Exit: 0 retired, 1 refused, 2 a usage error. A worktree with uncommitted
# work is a refusal: commit or discard it and call again, which cannot
# count the same readings twice.
set -u

root=$(git rev-parse --show-toplevel) || exit 1
cd "$root" || exit 1

branch=${1:-}
if [ -z "$branch" ] || [ "$#" -gt 1 ]; then
    echo "usage: scripts/worktree-retire.sh <branch>" >&2
    exit 2
fi

say() { echo "worktree-retire: $*"; }
refuse() { echo "worktree-retire: $*" >&2; exit 1; }

list=$(git worktree list --porcelain)
main=$(printf '%s\n' "$list" | sed -n '1s/^worktree //p')
[ -n "$main" ] || refuse "git worktree list names no main worktree"

dir=$(printf '%s\n' "$list" | awk -v want="branch refs/heads/$branch" '
    /^worktree /{ path = substr($0, 10) }
    $0 == want { print path; exit }')
[ -n "$dir" ] || refuse "no worktree is on $branch"
[ "$dir" != "$main" ] || refuse "$dir is the main worktree, which is the track's own root; it is not retired"

# The evidence, before the removal that would take it. The log alone: every
# line of it ends in a newline, so appending one to another is the whole
# join, and the per-key files beside it are each scope's latest run, which
# is the track's own (§4.2).
log=target/harness/log.jsonl
if [ -s "$dir/$log" ]; then
    readings=$(wc -l <"$dir/$log" | tr -d ' ')
    mkdir -p "$main/$(dirname "$log")" || refuse "cannot write the track's store at $main/$log"
    cat "$dir/$log" >>"$main/$log" || refuse "cannot append to $main/$log"
    # Emptied once it is appended, so a removal that refuses and a call that
    # follows it cannot count the same denials twice.
    : >"$dir/$log"
    say "appended $readings readings to $main/$log and emptied the worktree's"
else
    say "$dir/$log is missing or empty: this worktree wrote no readings"
fi

git worktree remove "$dir" ||
    refuse "git worktree remove refused; the readings are appended already — commit or discard what is uncommitted and call again"
say "removed $dir"
say "the branch is kept; a person deletes it once it is merged: git branch -D $branch"
