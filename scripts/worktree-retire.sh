#!/usr/bin/env bash
# worktree-retire.sh — `xtask worktree retire <branch>` by the name
# harness-design.md §5 gives the other half: the worktree's log appended
# into the main checkout's store, the worktree removed, the branch kept and its
# delete line printed as a person's. The clone's built harness where one
# is, release before debug as the command hook tries them, `cargo xtask`
# otherwise.
#
# Exit: 0 retired, 1 refused, 2 a usage error. A worktree with uncommitted
# work is a refusal after the append: commit or discard it and call again,
# which cannot count the same readings twice.
set -u
root=$(git rev-parse --show-toplevel) || exit 1
for x in "$root/target/release/xtask" "$root/target/debug/xtask"; do
    [ -x "$x" ] && exec "$x" --root "$root" worktree retire "$@"
done
cd "$root" && exec cargo xtask worktree retire "$@"
