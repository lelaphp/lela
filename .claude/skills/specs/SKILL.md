---
name: specs
description: Coordinate the builders, three or four slots kept full of specs cut from the corpus, and merge what they finish into main through the merge check. `/specs`, from the coordinator's clone.
disable-model-invocation: true
---

The coordinator keeps the builder **slots** full and merges what they finish. A slot is one builder agent in its own worktree; three or four run at once, as many as the disk and the machine's 12 cores carry. A slot works one **spec**: a ticket of about 150–700 tests' worth of work, cut from where the corpus fails rather than from the design's fog, so every merge moves a measured count. The coordinator's own edits are merge resolutions. What it owns is the cut, the dispatch, the merge and the **merge check**, the proof that a merge regressed nothing.

## Where you stand

- The coordinator's clone, its checkout detached at `main`'s tip, is where every merge happens; builders work under its `.claude/worktrees/t<n>/`, on branches that name no track, so `done` measures each since `main`. `cargo xtask track` in a worktree says so.
- `git worktree list` and `git config --get-regexp 'branch\..*\.ticket'` show the slots laid and each one's ticket. A builder this session didn't dispatch is gone; its branch's commits show how far it got.
- `gh issue list --label ready-for-agent --state open` holds the open specs and blockers, and `--label for-a-person` the person's tickets. `df -h` shows the disk.
- `$S` is your scratchpad. Runs and message files there are named for their commit, branch or ticket.

## The loop

Whenever a slot frees (a builder ends, a branch merges, a branch goes back), work these in order: integrate what finished, then fill the slot. The loop is at rest when every slot holds a running builder, or `df` is the reason one doesn't. Builder completions arrive as notifications, so wait on those.

## 1. Choose what fills the slot

**Blocker first.** A crash, a regression the merge check found, or a harness defect that stops a merge takes the next slot ahead of any spec. A regression goes back to a builder on its branch, a fresh one when the first has ended, with the test names. Harness work fills a slot only when a merge waits on it (#653).

Otherwise the slot gets a **spec**. A ready ticket that isn't a spec joins the spec that shares its code path (#667 bundled five compiler refusals; #668 absorbed #661, #529 and #533), or waits for one. Every slot holds a spec or a blocker.

## 2. Cut a spec

1. On a fresh release build at `main`'s tip, take the per-commit lane with its census and `Zend/tests`, each into `$S`. The merge check's head runs are these runs; reuse them while the tip is unchanged.
   ```
   cargo build --release -p php
   cargo xtask phpt --census > $S/lane-<tip>.out
   cargo xtask phpt --subset Zend/tests > $S/zend-<tip>.out
   ```
2. Group the failing tests by **first failure**, the first line the engine printed that php didn't. `awk -f .claude/skills/specs/first-failures.awk $S/lane-<tip>.out | sort | uniq -c | sort -rn | head -40`; add `cut -d: -f1-2` before the `sort` for families (`php only: Fatal error` is a compile or link fatal php raises and the engine doesn't). The `phpt census` lines list the unread constructs.
3. Gather causes into groups over one code path: one extension's classes, the natives of one php-src file (`grep -rlE 'PHP_FUNCTION\(<name>\)' <php-src>/ext`, the tree at `oracle.toml`'s `[source].path`), one compiler refusal family. Take the widest group that no open ticket covers (`gh issue list --search`) and that collides with no slot. Split a group wider than about 700 tests along its php-src files; bundle one narrower than about 150 with its neighbour.

**Collisions.** Slots split by extension, by php-src file within ext/standard, or compiler against natives; two specs never share an emit path at once. A spec that would collide with a branch that is finished but not merged is laid on that branch (#742 and #751 were laid on #667's).

The cut is done when you can name the group, its test count and the commit it was measured at, its php-src files or constructs, the tickets it absorbs, and where its worktree is laid. The frontier skill cuts its batch the same way (#653).

## 3. Write the spec

The body is the user-level `to-spec` skill's template (Problem Statement, Solution, User Stories, Implementation Decisions, Testing Decisions, Out of Scope, Further Notes), synthesised from the cut, followed by this repository's `## Acceptance criteria` boxes, which `xtask ticket read` holds, and `## Blocked by`. #751 is the model.

- The Problem Statement carries the measured counts and the commit. Out of Scope names the neighbouring spec. Further Notes names the branch the spec is laid on, when it isn't `main`.
- The boxes cover every cause of the group, each met and witnessed or named and filed; the before and after counts in the closing comment; and `cargo xtask done` listing nothing unmet for the crates touched. Regressions are the merge check's, so no box asks the builder for them.
- File it with the label `ready-for-agent` and a parent as the build skill files a ticket.
- Anything a person must rule on first (a manifest row, a ceiling, a porting key, a design line) is its own `for-a-person` ticket, wired with `gh issue edit <spec> --add-blocked-by <n>`. A blocked spec waits, and the slot takes the next cut.

The spec is written when `cargo xtask ticket read <n>` prints its boxes.

## 4. Lay the worktree and dispatch

- Laid on `main`: `cargo xtask worktree add t<n> --ticket <n>`, with the clone's local `main` first set to the pushed one (`git fetch origin && git branch -f main origin/main`). While the checkout holds a staged merge the workspace doesn't build, so run the clone's built `target/release/xtask --root <clone> worktree add …` instead.
- Laid on an unmerged branch, which `worktree add` can't do because it branches only from `main`:
  ```
  git worktree add -b t<n> .claude/worktrees/t<n> t<m>
  git config branch.t<n>.ticket <n>
  ```

Recording the ticket on the branch also keeps `xtask track next` from naming the spec to another session. Then dispatch one background builder agent with the prompt in [builder-prompt.md](builder-prompt.md), filled for this spec. The slot is full when the agent is running and `git config branch.t<n>.ticket` prints `<n>`.

## 5. Integrate

In the clone's checkout, one branch at a time. A blocker's branch (a crash or a regression fix) merges on its own as soon as its builder finishes, ahead of the specs waiting: #723's fix waited on its branch while the same bug was found again as #771 and fixed a second time.

1. `git merge --no-ff --no-commit <branch>`. Resolve every conflict, then `cargo build -p php` until it builds, since the build is where the conflicts git can't see surface. [merging.md](merging.md) holds the ones that recur.
2. Commit through the hook with `git commit -F $S/msg-<branch>.txt` and no pathspec: a merge commit takes the whole index, which is why the checkout is the coordinator's alone. The subject is `Merge <branch>: <the spec's change in one sentence>`. A hook refusal is a conflict not yet resolved.

The merge is in when its commit exists and `git status` is clean. Every branch a builder finishes is merged or sent back: a ticket closes on its branch before the merge, so a branch left standing is work its closed ticket says `main` has (twelve were found so at a681389; #811 lists them). Branches over separate areas merge one after another and share one merge check, since a regression's test names point at its branch.

## 6. The merge check

After the last merge of a batch, before any push:

1. `cargo xtask port-map` when `crates/php_ext` changed.
2. `cargo xtask witness` over every crate. The hook runs only staged crates' witnesses, which is how #681 broke ext/reflection unseen until #733.
3. On a fresh release build, the lane and `Zend/tests`, each held test by test against the merge base, the tip before the batch, whose runs the cut's first step kept (#664):
   ```
   cargo build --release -p php
   cargo xtask phpt --census --against <base> > $S/lane-<head>.out
   cargo xtask phpt --subset Zend/tests --against <base> > $S/zend-<head>.out
   grep -E '^regressed |@against' $S/lane-<head>.out $S/zend-<head>.out
   ```
   No regression means every `@against` reading passes. A skip is no verdict (a stale binary, no run kept at the base, different subsets) and never reads as no regression. With no run kept at the base, take one in a checkout of the base with its engine built.
4. Push only on no regressions and a fast-forward: `git fetch origin`, `git merge-base --is-ancestor origin/main HEAD`, `git push origin HEAD:main`. A `main` another session moved is merged in first, and the check runs again against the moved tip.

A regression sends its branch back as a blocker (step 1), and the rest of the batch goes on without it: return the checkout to the base with `git reset --hard <base>` (the merges are unpushed, and each branch keeps its own commits), merge the others again and run the check again.

**What stops a merge and goes to a person.** A ceiling (`api_items`, a crate's line budget), a `done` refusal on a crate without `porting` whose subset doesn't fully pass (until #725 lands), or a design line for a cross-extension export. File it `for-a-person` with the exact refusal and the saved resolution, as #684 did, and merge the rest of the batch.

## 7. Free the disk

Retire a worktree with `cargo xtask worktree retire <branch>` once its branch has merged, or once its builder has finished and the branch waits to be merged; the branch stays, and a branch sent back gets a worktree again. Much of a `target/` is shared copy-on-write, so retiring frees less than `du` suggests: read `df` after each retire.

Until #787's ancestry rule is on `main`, retire only after a run at the tip has written newer readings. `worktree retire` appends the worktree's log into the clone's store, and `xtask status` and the dashboard take the last reading written per scope, whatever commit it measured: retiring #742's and #751's worktrees, cut behind the tip, made the dashboard read 15.6% where the track stood at 44.6%.
