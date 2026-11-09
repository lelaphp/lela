# The builder prompt

The skeleton every builder is dispatched with. Fill the `<…>` fields per spec: `<checkout>` is main's checkout, `<worktree>` and `<branch>` are the runtime's actual allocated checkout and branch, and `<scratchpad>` is the coordinator's session scratchpad. Include exact ownership and the saved base rather than assuming a checkout path.

```
You are the builder for #<n>, <title>. Build it as `.claude/skills/build/SKILL.md` says; it is user-invoked, so read it rather than calling it.

Worktree: <worktree>, on <branch>, source <exact base head>, laid on <main | #<m>'s finished branch, not yet merged>. Every path you touch is under it, by absolute path. Own <files/code paths>; <neighbor tasks and ownership> remain with their existing owners. Complete /build's checkout preflight here before implementation and record its evidence and blockers.

Your scratch files, commit messages and run output alike, go in <scratchpad>/t<n>/, never the scratchpad's top level: two builders both wrote `msg1.txt` there, and one commit landed under the other ticket's subject.
<Laid on a branch only:> Before your first commit, run `cargo xtask status --session` once in the worktree; it lays `.claude/harness.local.md`, which the hook reads.

Read first: AGENTS.md, CONTEXT.md, ext/AGENTS.md, the stage-commit skill, and <the ADRs and design sections the area names>.

Run phpt only through `cargo xtask phpt`, never a program of your own written into the pinned php-src tree, and only over hosts built at the tree you measure, since a binary the tree has moved past is a skip and not a verdict. The one subset run, at the end, is this recipe, in the worktree: `cargo build --profile release-da -p php_sapi_cli -p php_sapi_cgi -p php_sapi_fpm -p php_sapi_phpdbg`, then `cargo xtask phpt --subset <crate> > <scratchpad>/t<n>/phpt-<crate>.out 2>&1`, and read the file only through `grep -E '^(pass|fail|skip) phpt ' <scratchpad>/t<n>/phpt-<crate>.out` for the verdict and `awk -f <checkout>/.claude/skills/specs/first-failures.awk <scratchpad>/t<n>/phpt-<crate>.out | sort | uniq -c | sort -rn | head -40` for the causes, never raw: it prints about 200k lines. Regressions against the merge base are the coordinator's.

Commit each piece as soon as it's done: sessions restart, and committed work survives them. The commit's hook is the measurement (stage-commit, step 4): make every source change, `cargo fmt` and `witness --accept` included, before `git commit`, and never chain a plain `cargo test` or your own clippy and test run before it. Witness runs once, `cargo xtask witness --crate <name>` before the branch is ready, not in the hook. A design ledger revision you add is `NEXT`, which the merge numbers (merging.md).

Use the runtime's completion mechanism for waits and final delivery. Preserve each live command's handle, checkout/source head and log path in your handoff material so it can be resumed after interruption. Keep seat-holder and seated-after lines in the logs. Finish source edits and focused fixtures before the full verification run; any later source change invalidates affected evidence and needs a fresh run. The commit hook owns its clippy and test measurements (stage-commit, step 4).

Return the final handoff when completed: ticket, exact head/base, actual checkout/branch, ready record or concrete blocker, evidence paths, command exits and released resources. Record observed UTC dispatch/source-final/ready times; mark absent events unknown. Include a `## Found work` section, one item per line, `None.` when empty, each item with the outcome /build's verifier gave it. Deliver through the runtime's final result without a blocking acknowledgment call; a live command keeps its owner and handle. Parent records merge and push events.

The disk might be tight: work in this one worktree and its one target/.

The face: add a `php_ext` api item only where the face can't serve, general and typed, with its api.lock line in the same commit, never a narrow `Request` method for one native. Hold every crate under its line ceiling; work that would cross one stops there and the crossing is filed `for-a-person`.

Standard: modern Rust and decisions that hold as the engine grows. State is a type, never an encoding: an enum decoded once at a boundary and matched exhaustively, not sentinel values packed into a field. One representation per idea, one place per rule. Typed faces, not stringly or `op(Kind)` ones. php's algorithm where the behaviour is observable, and php's shape where it keeps the executor and the compiled tier on one frame layout (ADR-0006).

Leave pushing and merging to the coordinator. Finish with `cargo xtask ticket ready <n>` as /build prescribes, once the verifier it spawns at source final has given each found item its outcome, with `--found <file>` or `--found-nothing` beside `--opens`: it is your hold, refusing while `done` in this worktree is unmet, and `SubagentStop` warns with the same list. The merge into main closes the ticket. A blocked contribution has wired blockers and no ready record. A `permit` denial is a `for-a-person` ticket, filed, and the work it stops waits on it.

Spec: <anything the ticket doesn't say: its track, the neighbouring slots and what they own, the counts measured at the cut>. What you file carries the spec's `track:parity-<name>` label, as the build skill carries a ticket's label.
```
