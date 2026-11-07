---
name: specs
description: Coordinate the builders, three slots kept full of specs cut from the corpus, and merge what they finish into main through the merge check. `/specs`, from main's checkout.
disable-model-invocation: true
---

The coordinator keeps the builder **slots** full and merges what they finish. A slot is one builder agent in its own worktree; three run at once and never more, whatever layer dispatches them (an orchestrator between this loop and its builders counts its builders against the same three), each building under the `CARGO_BUILD_JOBS` `.claude/settings.json` sets, so the three and a merge's hook fit the machine's 12 cores. Five lanes on 2026-10-18 held the machine at a load of 32 at the median and 100 at the most, and every build and test ran slower for it (#2982); the building instruments now also wait for one of three machine-wide seats (`XTASK_SEATS`), so a `waiting for one of 3 seats` line is the machine full, not a hang; a builder's commit hook runs at `nice` 10 on its own, since its checkout is a linked worktree, and yields to the merge's (harness revision 140). A slot works one **spec**: a ticket of about 150–700 tests' worth of work, cut from where the corpus fails rather than from the design's fog, so every merge moves a measured count. The coordinator's own edits are merge resolutions. What it owns is the cut, the dispatch, the merge and the **merge check**, the proof that a merge regressed nothing.

## Where you stand

- Main's checkout (`~/Projects/dax-php`, on the branch `main`) holds the one store, its `target/harness`, and moves only by `cargo xtask worktree land` (§6); every merge is made in the merge worktree, `_merge` on `staging` (§5). Builders work in worktrees `cargo xtask worktree add` lays beside it, each on a branch `t<n>` cut from `main`, so `done` measures each since `main`; `cargo xtask track` in a worktree says so. A track is a label, never a branch (ADR-0009). Start each batch from the pushed tip: `git pull --ff-only`.
- Main's checkout and `_merge` are the coordinator's while `/specs` runs, each by its owner record (#3621): `cargo xtask worktree own` in main's, `cargo xtask worktree queue` for `_merge`. Other sessions work in worktrees, since a merge commit takes the whole index. A person's uncommitted edit in it (a `.gitignore` line) stays theirs: nothing here stages it, and every reset is `--keep`, which refuses where it would overwrite one.
- `git worktree list` and `git config --get-regexp 'branch\..*\.ticket'` show the slots laid and each one's ticket. On resumption, reconcile each checkout with runtime tasks, command handles and actual process ownership; a builder absent from session memory may still be running. Reclaim its files or retire its checkout only after confirming completion or termination and preserving its work and evidence.
- The route is [phpt parity](https://github.com/mitchellvanw/dax-php/issues/1160): parity is the engine's verdict equal to the oracle's on every test the oracle runs, worked in six tracks, each a chart labelled `frontier:chart` with a `track:parity-<name>` label on its tickets (instruments, compiler, runtime, hosts, streams, extensions). A track has no branch, clone or store; this loop is what works them.
- `gh issue list --label ready-for-agent --state open` holds the open specs and blockers, and `--label for-a-person` the person's tickets; `--label track:parity-<track>` narrows to one track. A ticket labelled `superseded` was replaced by the parity route and is no spec, no blocker and no coverage. `df -h` shows the disk.
- `$S` is your scratchpad. Runs and message files there are named for their commit, branch or ticket.

## The loop

The coordinator owns three phases, recorded in the existing checkpoint:

- **Building:** dispatch independent specs within the shared capacity. Require /build's preflight in each actual allocated checkout before implementation. Runtime completion delivers the builder's exact final handoff; the ready record, not an acknowledgment, establishes merge readiness. The handoff carries a fixed `## Found work` section, one item per line and `None.` when empty, each item verified inside /build at source final and ended as one of its six outcomes before the ready record, so a batch sees only decided branches. A handoff with no ready record because an item became a `build` box gets one fresh builder in the same worktree, briefed that the box is its work, handed the verified lines, and told that its own found work is filed unverified; a ticket gets no second fresh builder. For the first 20 handoffs with an item after the verifying step lands, the brief's spec line asks for a second verifier on Opus beside the advisor: show a person every disagreement a handoff names, and keep the count in the checkpoint. `ticket ready` is the builder's hold: it refuses while `done` in the builder's worktree is unmet, and `SubagentStop` warns with the same list.
- **Integrating:** select a bounded batch from branches already ready and integrate it serially. A single ready branch can be a batch; running builders need not finish first. Where ready branches share a dependency, integrate the shared engine change first where practical, then the extension changes, before refreshing the combined tree.
- **Publishing:** save the comparison base and candidate head, freeze that source, run §6 and push as soon as its comparisons and `done --base origin/main` pass. Broader production qualification continues separately. Finish publication before starting another integration batch.

During publication, let active commands finish and give final verification the next available seats: defer launching new expensive builder checks until the publication checks have acquired capacity. Builders can continue independent investigation, edits and review. Keep the existing three-seat limit and capture seat waits in command logs; the checkpoint references the commands and owns no second scheduler. Run no merge check alongside a merge hook (§6).

A new safety fix or regression can reopen the frozen candidate. Record the reason, old and new heads, and which dependency-closure readings or comparisons became stale; integrate the fix and rerun the requirements over the new tree. Reuse only evidence whose source, recipe, profile and subset identity still match.

After publication, reassess and fill the next slots. Builder and command completions arrive through the runtime; yield using its supported mechanism. Preserve resumable command handles and log paths in the checkpoint, and recover failed or interrupted calls through that runtime. Never hold a resource waiting for an acknowledgment or block in a foreground sleep/poll loop (#2982). A final handoff records completed commands and released resources; a still-live command retains its actual owner and handle.

## 1. Choose what fills the slot

**Blocker first.** A crash, a regression the merge check found, or a harness defect that stops a merge takes the next slot ahead of any spec. A regression goes back to a builder on its branch, a fresh one when the first has ended, with the test names. Harness work fills a slot only when a merge waits on it (#653).

Otherwise the slot gets a **spec**, drawn from the parity tracks in this order: a track's open, unblocked ticket that is already a spec's size; several of one track's smaller tickets on one code path bundled into one spec (the first compiler batch, #1187, #1188 and #1189, is 91 tests over one emit path); then a spec cut from a chart's Not yet specified, as step 2 cuts. A ready ticket that isn't a spec joins the spec that shares its code path (#667 bundled five compiler refusals; #668 absorbed #661, #529 and #533), or waits for one. Every slot holds a spec or a blocker.

## 2. Cut a spec

1. On a fresh build of the lanes' profile, `release-da`, at `main`'s tip, take the per-commit lane with its census and `Zend/tests`, each into `$S`. The merge check's head runs are these runs; reuse them while the tip is unchanged.
   ```
   cargo build --profile release-da -p php_sapi_cli -p php_sapi_cgi -p php_sapi_fpm -p php_sapi_phpdbg
   cargo xtask phpt --census > $S/lane-<tip>.out
   cargo xtask phpt --subset Zend/tests > $S/zend-<tip>.out
   cargo xtask phpt --subset tests > $S/tests-<tip>.out
   cargo xtask phpt --subset sapi/cli > $S/cli-<tip>.out
   ```
   The per-commit lane holds only crates that exist, so an extension the bundle lacks has no subset to fail in; its count is the nightly `--full` reading of `ext/<name>/tests` (the dashboard's `oracle-main` artifact), and its spec is the extensions track's ticket for it.
2. Group the failing tests by **first failure**, the first line the engine printed that php didn't. `cat $S/lane-<tip>.out $S/tests-<tip>.out $S/cli-<tip>.out | awk -f .claude/skills/specs/first-failures.awk | sort | uniq -c | sort -rn | head -40`; add `cut -d: -f1-2` before the `sort` for families (`php only: Fatal error` is a compile or link fatal php raises and the engine doesn't). The `phpt census` lines list the unread constructs.
3. Gather causes into groups over one code path: one extension's classes, the natives of one php-src file (`grep -rlE 'PHP_FUNCTION\(<name>\)' <php-src>/ext`, the tree at `oracle.toml`'s `[source].path`), one compiler refusal family. Take the widest group that no open ticket covers (`gh issue list --search "<construct> -label:superseded"`, since a superseded ticket covers nothing) and that collides with no slot. Split a group wider than about 700 tests along its php-src files; bundle one narrower than about 150 with its neighbour.

**Collisions.** Slots split by track first, then by extension, by php-src file within ext/standard, or compiler against natives; two specs never share an emit path at once. A spec that would collide with a branch that is finished but not merged waits for that merge, and the slot takes the next spec that collides with nothing; only where none is left is it laid on that branch (#742 and #751 were laid on #667's). A spec laid on an unmerged branch waits on that branch's merge to be ready itself, and a branch rewriting the same items under it is a merge conflict for both: in #2982's session, builders stacked so sat idle for hours.

The cut is done when you can name the group, its track, its test count and the commit it was measured at, its php-src files or constructs, the tickets it absorbs, and where its worktree is laid. The frontier skill cuts its batch the same way (#653).

## 3. Write the spec

The body is the house shape every ticket here has, synthesised from the cut: `## What to build`, `## Acceptance criteria` as boxes, which `xtask ticket read` holds, and `## Blocked by`.

- What to build carries the measured counts and the commit, the group's causes and php-src files, the neighbouring spec it stops at, and the branch the spec is laid on when it isn't `main`.
- The boxes cover every cause of the group, each met and witnessed or named and filed; the before and after counts in the closing comment; and `cargo xtask ticket ready <n>` accepted. Regressions are the merge check's, so no box asks the builder for them.
- File it with the labels `ready-for-agent` and its track's `track:parity-<name>`, and no parent: a chart takes no sub-issues (#619), and the label is what groups the track. A spec that absorbs a track's tickets names them, and each gets a comment linking the spec.
- Anything a person must rule on first (a manifest row, a ceiling, a porting key, a design line) is its own `for-a-person` ticket, wired with `gh issue edit <spec> --add-blocked-by <n>`. A blocked spec waits, and the slot takes the next cut.

The spec is written when `cargo xtask ticket read <n>` prints its boxes.

## 4. Lay the worktree and dispatch

- Laid on `main`: `cargo xtask worktree add t<n> --ticket <n>`, after `git pull --ff-only` so it branches from the pushed tip. While the checkout holds a staged merge the workspace doesn't build, so run its built `target/release/xtask --root <checkout> worktree add …` instead.
- Laid on an unmerged branch, which `worktree add` can't do because it branches only from `main`:
  ```
  git worktree add -b t<n> ~/Projects/dax-php-worktrees/t<n> t<m>
  git config branch.t<n>.ticket <n>
  ```

Recording the ticket on the branch also keeps `xtask track next` from naming the spec to another session. Then dispatch one background builder agent with the prompt in [builder-prompt.md](builder-prompt.md), filled for this spec. The slot is full when the agent is running and `git config branch.t<n>.ticket` prints `<n>`.

## 5. Integrate

In the merge worktree, one branch at a time. Merges run in a local merge queue (#3618 M2): `_merge`, a worktree beside main's checkout on the branch `staging`, takes every merge and the merge check, and `main` moves only by a fast-forward to a commit that passed, so a red check never touches it. A blocker's branch (a crash or a regression fix) merges on its own as soon as its builder finishes, ahead of the specs waiting: #723's fix waited on its branch while the same bug was found again as #771 and fixed a second time.

Claim the queue once a session with `cargo xtask worktree queue`: it lays `~/Projects/dax-php-worktrees/_merge` down on `staging`, cut from `main` where the branch is missing, owned by this session, its `debug`, `release` and `release-da` builds seeded copy-on-write from main's; where it stands, the call claims it, and another session's is refused. Start each batch at the pushed tip: `git pull --ff-only` in main's checkout, then `git merge --ff-only main` in `_merge`. Its commits run at full priority, since pre-commit renices every linked worktree but this one.

Each step is the one GitHub's merge queue takes, so moving there replaces the runner and not the protocol:

| here | GitHub's merge queue |
|---|---|
| `staging` in `_merge`, the batch merged `--no-ff` | a merge group, on a `gh-readonly-queue/main/…` branch |
| the merge check (§6, steps 1–4) | the checks required on the `merge_group` event |
| `cargo xtask worktree land` | the queue's fast-forward of `main` to a group that passed |
| `git reset --keep main` on a red check | the group dropped, `main` untouched |
| `main` merged into `staging` after it moved | the group rebuilt on the new tip |
| post-merge closing the tickets at the fast-forward | the same hook at the `git pull --ff-only` that brings it |

1. In `_merge`, `git merge --no-ff --no-commit <branch>`. Resolve every conflict, then `cargo build -p php_sapi_cli -p php_sapi_cgi` until it builds, since the build is where the conflicts git can't see surface. [merging.md](merging.md) holds the ones that recur.
2. Commit through the hook with `git commit -F $S/msg-<branch>.txt` and no pathspec: a merge commit takes the whole index, which is why `_merge` is the coordinator's alone. The subject is `Merge <branch>: <the spec's change in one sentence> (#<n>)`, naming the branch's ticket, and first parents stay on `staging`, which becomes main's line, so #787's chain is unchanged. The commit closes nothing, since `ticket merged` closes nothing off `main`; the fast-forward that lands it (§6, step 5) runs post-merge, which closes each open ticket the merges name with the successors its branch's ready record names, through `xtask ticket ready`'s record and `xtask ticket close` (#2270, #2311). A hook refusal is a conflict not yet resolved; a ticket the hook names as having no ready record is the builder's to finish, its branch sent back. A merge that lands part of a ticket names it the same way and types nothing more: a ticket with no ready record, a box unticked and a blocker that still blocks it at the tracker, as the build skill leaves one whose last box waits on another, is a partial landing, and the hook says it left it open and what blocks it, and `done` does not name it (#2394). Where the blocker is wired after the fast-forward, run `cargo xtask ticket merged --since <the merge>^1` on `main` once it is, and the refusal becomes that.

The merge is in when its commit exists on `staging` and `git status` in `_merge` is clean. Every branch a builder finishes is merged or sent back: a builder ends at its ticket's ready record and the merge closes the ticket, so a ready branch left standing is an open ticket whose work waits on the coordinator (before #2270, tickets closed on their branches and twelve such branches were found at a681389; #811 lists them). Branches over separate areas merge one after another and share one merge check, since a regression's test names point at its branch.

## 6. The merge check

In `_merge`, after the last merge of a batch, before the land, and never while a merge commit's hook is running: the check and the hook share the target directory and the cores, and a hook starved of either is what fails witnesses and tests under load (#1312).

1. `cargo xtask port-map` when `crates/php_ext` changed.
2. `cargo xtask witness --unmeasured`: every crate without a passing reading over its tree now. The hook runs no witness on a merge commit and no crosscheck at all (harness revision 140), so this is where the batch's merged tree is witnessed, and `cargo xtask crosscheck` beside it renders every extension the binary carries over that tree once per batch, the two sharing the one build of the linked engine; a `fail crosscheck` on an extension the batch touched is what `done` refuses at the push. The ones measured over the tree that landed stand and are not run again (#1312), and before revision 140 the hook ran only staged crates' witnesses, which is how #681 broke ext/reflection unseen until #733.
3. Establish the baseline before launching candidate comparisons. Use `cargo xtask phpt --base <base> --per-commit` (#3605) to collect missing per-commit subsets with one base checkout and host build under the caller's current manifest, registry and recipe. Recover the additional `Zend/tests`, `tests` and `sapi/cli` runs below with explicit `--base <base> --subset <subset>` only when matching runs are absent. Use the retained-run mechanism; running the old checkout's harness directly loads its old recipe and cannot establish this comparison. A missing or mismatched baseline is unresolved evidence, so recover it before spending capacity on a comparison that can only skip.
4. Build all four correctness hosts (CLI, CGI, FPM and phpdbg) in `release-da` before PHPT; freshness applies to every host the lane can serve through. On that fresh build, the profile CI's lanes run (harness revision 82), the lane and `Zend/tests`, each held test by test against the merge base, the tip before the batch, whose runs the cut's first step kept (#664). `xtask phpt` runs `target/release-da/php` ahead of a fresh `release` one, so a base kept on `release` compares across profiles: a test met there and missed here is an overflow check or a debug assertion firing, a bug to file and not the batch's regression. `target/release/xtask` is the command hook's harness, not a lane's, and stays a release build. The witness and the four runs read the tree and not each other, and each run works in its own copy of the pinned tree, so they run at once; the lane is the longest:
   ```
   cargo build --profile release-da -p php_sapi_cli -p php_sapi_cgi -p php_sapi_fpm -p php_sapi_phpdbg
   cargo build --release -p xtask
   cargo xtask witness --unmeasured > $S/witness-<head>.out 2>&1 &
   cargo xtask crosscheck > $S/crosscheck-<head>.out 2>&1 &
   cargo xtask phpt --census --against <base> > $S/lane-<head>.out &
   cargo xtask phpt --subset Zend/tests --against <base> > $S/zend-<head>.out &
   cargo xtask phpt --subset tests --against <base> > $S/tests-<head>.out &
   cargo xtask phpt --subset sapi/cli --against <base> > $S/cli-<head>.out &
   wait
   grep -E '^(fail|skip) witness' $S/witness-<head>.out
   grep -E '^fail crosscheck' $S/crosscheck-<head>.out
   grep -E '^regressed |@against' $S/lane-<head>.out $S/zend-<head>.out $S/tests-<head>.out $S/cli-<head>.out
   git fetch origin && cargo xtask done --base origin/main
   ```
   No regression means every `@against` reading passes. A skip is no verdict (a stale binary, no run kept at the base, different subsets) and never reads as no regression. Baselines from step 3 use the existing carry and cleanup path; `--against` looks in every worktree the repository lists, and a separate clone's runs are not found. `done` at the end names what pre-push would refuse (a stale `port-map`, a new subset with no reading), so it is taken before the push rather than after a refusal.
5. Land and push only on no regressions. In main's checkout, `git fetch origin` and `git pull --ff-only` where origin moved, then `cargo xtask worktree land`: it asks `done` in `_merge` since main's tip, lands `_merge`'s readings in main's store without retiring it, so `done`, `status` and pre-push on `main` find them, and fast-forwards `main` to the commit judged, whose post-merge closes the batch's tickets. While anything is unmet it refuses and `main` is untouched. Then `git push origin main`. A `main` that moved past `staging`, a person's commit or another session's, refuses the land: merge it into `staging` in `_merge` with `--no-commit`, commit it, and run the check again against the moved tip.

A regression sends its branch back as a blocker (step 1), and the rest of the batch goes on without it: return `_merge` to the base with `git reset --keep main` (the merges are on `staging` alone and `main` never moved, each branch keeps its own commits, and `--keep` leaves a person's uncommitted edit standing where `--hard` would discard it), merge the others again and run the check again.

**What stops a merge and goes to a person.** A ceiling (`api_items`, a crate's line budget), a `done` refusal on a touched crate's phpt subset, or a design line for a cross-extension export. File it `for-a-person` with the exact refusal and the saved resolution, as #684 did, and merge the rest of the batch.

## 7. Free the disk

Retire a worktree with `cargo xtask worktree retire <branch>` as soon as its branch has merged, or once its builder has finished and the branch waits to be merged; the branch stays, and a branch sent back gets a worktree again. A finished worktree left standing is gigabytes of debug and release build that nothing reads. #787's ancestry rule is on `main`, so a retired worktree's appended readings never stand in for the tip's, and a retire waits on no newer run. Much of a `target/` is shared copy-on-write, so retiring frees less than `du` suggests: read `df` after each retire.

Lay no worktree while `df -h` shows less than 20 GB free. Below that, in this order until it does:

1. Retire every worktree whose builder has finished.
2. Remove `target/debug/incremental` in main's checkout and in every worktree no cargo or rustc is building in. `lsof -a -d cwd -c cargo -c rustc -Fn | sed -n 's/^n//p' | sort -u` names the directories they run in; an incremental cache is 5 GB or more, and a build that finds it gone compiles from scratch and is otherwise unharmed.

A slot still empty after both is the loop at rest for `df`. Never give two worktrees one `CARGO_TARGET_DIR` to save space: the binaries, the store and `permit` each belong to one checkout (`xtask/src/worktree.rs`, #251).
