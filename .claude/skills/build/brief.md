The brief a spawned builder starts with, rendered by `cargo xtask ticket brief <n> [--scratch <dir>] [--owns <what>]... [--spec <text>]`. Everything above the `## Brief` line is this note and is not rendered; everything below it is, with each `{field}` filled from the ticket, the branch's records and the flags: `ticket`, `title`, `worktree`, `branch`, `base`, `laid_on`, `unmerged_base`, `checkout`, `scratch`, `owns`, `track_label`, `spec` and `stamp`. A line naming a field with no value is left out, so `unmerged_base` keeps its line only for a branch laid on another not yet merged, `track_label` only for a ticket carrying a `track:` label, and `owns`, `scratch` and `spec` only where `--owns`, `--scratch` and `--spec` gave one; a hook that has the number alone renders the rest. A field this list does not name is refused. A clause edited here is a skill edit, and the next brief carries it. No cq line while the census of cq calls against greps is open (Round 2, Q5).

## Brief

Rendered by `xtask ticket brief {ticket}` from `.claude/skills/build/brief.md`, {stamp}.

You are the builder for #{ticket}, "{title}". Build it as `.claude/skills/build/SKILL.md` says; it is user-invoked, so read it rather than calling it. Read the ticket with `cargo xtask ticket view {ticket}`, which prints the body and the comments you may act on, and `cargo xtask ticket read {ticket}`, which holds it to its boxes.

Worktree: {worktree}, on branch {branch}, source {base}, laid on {laid_on}. Every path you touch is under it, by absolute path. Complete /build's checkout preflight here before implementation and record its evidence and blockers.
Own {owns}; the neighbouring slots the spec names remain with their existing owners.
Laid on {unmerged_base}, a branch not yet merged: before your first commit, run `cargo xtask status --session` once in the worktree; it lays `.claude/harness.local.md`, which the hook reads.

Your scratch files, commit messages and run output alike, go in {scratch}, never the scratchpad's top level: two builders both wrote `msg1.txt` there, and one commit landed under the other ticket's subject.

Read first: AGENTS.md, CONTEXT.md, ext/AGENTS.md, the stage-commit skill, and the ADRs and design sections the ticket's area names.

Run phpt only through `cargo xtask phpt`, never a program of your own written into the pinned php-src tree, and only over hosts built at the tree you measure, since a binary the tree has moved past is a skip and not a verdict. The one subset run, at the end, is this recipe, in the worktree, `<scratch>` being your scratch directory: `cargo build --profile release-da -p php_sapi_cli -p php_sapi_cgi -p php_sapi_fpm -p php_sapi_phpdbg`, then `cargo xtask phpt --subset <crate> > <scratch>/phpt-<crate>.out 2>&1`, and read the file only through `grep -E '^(pass|fail|skip) phpt ' <scratch>/phpt-<crate>.out` for the verdict and `awk -f {checkout}/.claude/skills/specs/first-failures.awk <scratch>/phpt-<crate>.out | sort | uniq -c | sort -rn | head -40` for the causes, never raw: it prints about 200k lines. Regressions against the merge base are the coordinator's.

Commit each piece as soon as it's done: sessions restart, and committed work survives them. The commit's hook is the measurement (stage-commit, step 4): make every source change, `cargo fmt` and `witness --accept` included, before `git commit`, and never chain a plain `cargo test` or your own clippy and test run before it. Witness runs once, `cargo xtask witness --crate <name>` before the branch is ready, not in the hook. A design ledger revision you add is `NEXT`, which the merge numbers (merging.md).

Use the runtime's completion mechanism for waits and final delivery. Preserve each live command's handle, checkout/source head and log path in your handoff material so it can be resumed after interruption. Keep seat-holder and seated-after lines in the logs. Finish source edits and focused fixtures before the full verification run; any later source change invalidates affected evidence and needs a fresh run. The commit hook owns its clippy and test measurements (stage-commit, step 4).

Return the final handoff when completed: ticket, exact head/base, actual checkout/branch, ready record or concrete blocker, evidence paths, command exits and released resources. Record observed UTC dispatch/source-final/ready times; mark absent events unknown. Include a `## Found work` section, one item per line, `None.` when empty, each item with the outcome /build's verifier gave it. Deliver through the runtime's final result without a blocking acknowledgment call; a live command keeps its owner and handle. Parent records merge and push events.

The disk might be tight: work in this one worktree and its one target/.

The face: add a `php_ext` api item only where the face can't serve, general and typed, with its api.lock line in the same commit, never a narrow `Request` method for one native. Hold every crate under its line ceiling; work that would cross one stops there and the crossing is filed `for-a-person`.

Standard: modern Rust and decisions that hold as the engine grows. State is a type, never an encoding: an enum decoded once at a boundary and matched exhaustively, not sentinel values packed into a field. One representation per idea, one place per rule. Typed faces, not stringly or `op(Kind)` ones. php's algorithm where the behaviour is observable, and php's shape where it keeps the executor and the compiled tier on one frame layout (ADR-0006).

Leave pushing and merging to the coordinator. Finish with `cargo xtask ticket ready {ticket}` as /build prescribes, once the verifier it spawns at source final has given each found item its outcome, with `--found <file>` or `--found-nothing` beside `--opens`: it is your hold, refusing while `done` in this worktree is unmet, and `SubagentStop` warns with the same list. It also prints the comments written on the ticket since your first commit: read them before you hand back. The merge into main closes the ticket. A blocked contribution has wired blockers and no ready record. A `permit` denial is a `for-a-person` ticket, filed, and the work it stops waits on it.

Spec: {spec}
What you file carries the ticket's `{track_label}` label, as the build skill carries a ticket's label.
