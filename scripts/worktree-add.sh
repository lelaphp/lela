#!/usr/bin/env bash
# worktree-add.sh — lay one track worktree down ready to be worked in
# (harness-design.md §4.2, §5, §11). A track runs an agent per worktree on
# a branch off its track branch, and three facts about a fresh worktree are
# why this is a command and not a bare `git worktree add`:
#
#   * `computed` trust is keyed by the root path, so a new worktree is
#     untrusted: its regions answer `exec untrusted skipped`, the session
#     hook renders nothing and pre-commit refuses the first commit. The
#     grant is a person's (§11), so the line is printed, never run.
#   * `PreToolUse` denies an `Edit` or a `Write` that no candidate judged,
#     and in a worktree with a cold `target/` the only candidate is `cargo
#     run … permit` inside the hook's 60 s timeout. So `xtask` is built
#     here, before the agent arrives, at the worktree's own HEAD, which is
#     what `built` compares its digests against (§12 item 8).
#   * `core.hooksPath` is repository config and `.claude/settings.json` and
#     `oracle.toml` are tracked, so all three carry over on their own, as
#     the php-src tree the pin names does by being outside the clone. They
#     are reported, not set again.
#
# usage: scripts/worktree-add.sh <branch>
# The branch names its own track: the longest existing branch that is a
# proper path prefix of it, `main` for a branch on none, which is the track
# `xtask done` will judge the worktree against (§4.5). The worktree is laid
# at <clone>-wt/<branch>, beside the clone.
#
# Exit: 0 ready, 1 refused with nothing laid down, 2 a usage error, 3 laid
# down but not ready to be worked in, which the last line says.
set -u

root=$(git rev-parse --show-toplevel) || exit 1
cd "$root" || exit 1

branch=${1:-}
if [ -z "$branch" ] || [ "$#" -gt 1 ]; then
    echo "usage: scripts/worktree-add.sh <branch>" >&2
    exit 2
fi
dir=$(dirname "$root")/$(basename "$root")-wt/$branch

say() { echo "worktree-add: $*"; }
refuse() { echo "worktree-add: $*" >&2; exit 1; }

# Refuse before anything is laid down, as `new-ext` does. `origin`'s branch
# counts: a branch another agent has pushed is one an agent is working on,
# and taking it off the track again would fork it.
if git show-ref --verify --quiet "refs/heads/$branch"; then
    refuse "the branch $branch exists; work in its worktree, retire it, or name another branch"
fi
if git show-ref --verify --quiet "refs/remotes/origin/$branch"; then
    refuse "origin has $branch, so an agent is on it; check it out with \`git worktree add <dir> $branch\`, or name another branch"
fi
if [ -e "$dir" ]; then
    refuse "$dir exists; retire it with scripts/worktree-retire.sh, or name another branch"
fi

# The track, and the ref the branch is taken off, asked of `xtask track`,
# the one statement of the rule (harness-design.md §4.5): the clone's built
# harness where one is, release before debug as the command hook tries
# them, `cargo xtask` otherwise.
xtask='cargo xtask'
for candidate in target/release/xtask target/debug/xtask; do
    [ -x "$candidate" ] && xtask=$candidate && break
done
facts=$($xtask --root "$root" track --branch "$branch") ||
    refuse "xtask track could not say which track $branch is on; the line above says why"
track=$(printf '%s\n' "$facts" | sed -n 's/^track: \([^ ]*\) (\(.*\))$/\1/p')
base=$(printf '%s\n' "$facts" | sed -n 's/^track: \([^ ]*\) (\(.*\))$/\2/p')
[ -n "$track" ] && [ -n "$base" ] ||
    refuse "xtask track printed no track line for $branch: $facts"

git worktree add -q -b "$branch" "$dir" "$base" ||
    refuse "git worktree add refused; nothing was laid down"
say "$branch off the track $track at $base ($(git rev-parse --short "$base"))"
say "worktree $dir"

ready=0

# Built here, so the first Edit is judged rather than waiting on a build.
if (cd "$dir" && cargo build -q -p xtask); then
    say "built target/debug/xtask: the first Edit in it is judged by this build, not by a cold cargo run"
else
    echo "worktree-add: cargo build -p xtask failed; an Edit there is judged by a cold cargo run or denied" >&2
    ready=3
fi

# The local file, laid down by the build just made and rendered by name.
built=
for candidate in "$dir/target/release/xtask" "$dir/target/debug/xtask"; do
    [ -x "$candidate" ] && built=$candidate && break
done
if [ -n "$built" ]; then
    if (cd "$dir" && "$built" status --session >/dev/null); then
        say "laid .claude/harness.local.md down"
    else
        echo "worktree-add: xtask status --session failed; the worktree has no local file" >&2
        ready=3
    fi
else
    echo "worktree-add: no built xtask under $dir/target; the worktree has no local file" >&2
    ready=3
fi

trusted=no
rendered=$(cd "$dir" && computed run .claude/harness.local.md 2>&1 >/dev/null)
case $? in
    0 | 1) echo "$rendered" | grep -q ' untrusted' || trusted=yes ;;
    2) echo "worktree-add: a computed marker in .claude/harness.local.md is broken" >&2 ;;
    *) echo "worktree-add: computed did not run (cargo install computed)" >&2 ;;
esac
if [ "$trusted" = yes ]; then
    say "rendered .claude/harness.local.md"
else
    say "this worktree is untrusted, so its regions are unrendered; a person runs, once (§11):"
    echo "    computed trust $dir"
fi

# What the clone already has, reported rather than set again.
hooks=$(git -C "$dir" config core.hooksPath || true)
if [ -n "$hooks" ]; then
    say "carried over: core.hooksPath=$hooks, which is repository config"
else
    say "core.hooksPath is unset, so pre-commit and pre-push are not wired; the session hook points it at scripts/hooks"
fi
if [ -f "$dir/.claude/settings.json" ]; then
    say "carried over: .claude/settings.json, which is tracked, so the plugin settings are the clone's"
else
    say ".claude/settings.json is missing, so this worktree loads no harness plugin"
fi
if [ -f "$dir/oracle.toml" ]; then
    say "carried over: oracle.toml, which is tracked, and the php-src tree it pins, which is outside the clone; \`xtask phpt\` says if that tree is gone"
else
    say "oracle.toml is missing, so this worktree has no oracle and no pinned php-src"
fi

if [ "$ready" = 0 ]; then
    say "ready: cd $dir"
else
    echo "worktree-add: $dir is laid down but not ready to be worked in; the lines above say what failed" >&2
fi
exit "$ready"
