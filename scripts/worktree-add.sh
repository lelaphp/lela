#!/usr/bin/env bash
# worktree-add.sh — `xtask worktree add <branch>` by the name harness-design.md
# §5 gives the bootstrap: the branch cut from `main`, `xtask` built in the
# worktree, the local file laid and rendered, the `computed trust` line
# printed as a person's, and what the clone carries reported. The clone's
# built harness where one is, release before debug as the command hook
# tries them, `cargo xtask` otherwise.
#
# Exit: 0 ready, 1 refused with nothing laid down, 2 a usage error, 3
# (`NOT_READY`, xtask/src/exit.rs) laid down but not ready to be worked in,
# which the last line says. 4, `UNFINISHED` beside it, is an instrument's
# that could not run to its end; `worktree add` never exits with it.
set -u
root=$(git rev-parse --show-toplevel) || exit 1
for x in "$root/target/release/xtask" "$root/target/debug/xtask"; do
    [ -x "$x" ] && exec "$x" --root "$root" worktree add "$@"
done
cd "$root" && exec cargo xtask worktree add "$@"
