---
name: drive-to-ready
description: Drive Dax to the launch gates on #3466 as their coordinator; dispatch builders, merge, publish, and keep one checkpoint until every engine gate has its evidence on main. `/drive-to-ready`, from main's checkout.
disable-model-invocation: true
---

# Drive to ready

You have the **mandate**: the `ready-for-agent` tickets are yours to group, order, absorb and reprioritize, and what lands on `main` is yours to answer for. The **board** is #3466. Each gate on it is either an engine gate, with evidence agents can produce, or a person's gate (press, counsel, domain, sign-off, the go/no-go itself). Engine gates are what you move. For a person's gate, you keep its blocker filed and precise.

The loop's mechanics belong to /specs: slots, seats, cuts, collisions, the merge check, the three phases. Read `.claude/skills/specs/SKILL.md` at the start and again after any compaction. This file decides *what* the loop works on; /specs decides *how*. /specs, /build, /frontier and /grill-design are user-invoked, so you and your builders read them by path under `.claude/skills/`.

## Stance

**Scope is the board**: work that moves no gate is filed and left. **Absorb**: a gap found mid-spec, on its path and within its size, is done in that spec; only other paths, person-only acts and work bigger than a spec become tickets. **Critic**: read the design of record for an area before cutting there. When a refactor or rewrite is the right shape, it is the spec: priced with /rust-invoice, reasoned in the Problem Statement, and bounded to one gate's path. **Push back** with evidence, through /grill-design, when a ticket's premise contradicts the code or the design; never build to it.

## The loop

1. **Own the checkpoint.** Open `~/Projects/dax-php-worktrees/_coordinator/ROADMAP.md`. If its `Owner` names another session that is still running, stop and report that; there is one coordinator at a time. Otherwise rewrite it to the shape under *The checkpoint*. If it isn't in that shape yet, first move it aside as `ROADMAP.before-<date>.md`, as earlier roadmaps were. Reconcile the rewrite with `git worktree list`, `gh issue list --label ready-for-agent`, and the runtime's live tasks. An agent you don't remember may still be running: confirm whether it is before replacing its work. Done when every slot line matches a live task or a branch.
2. **Read the board.** Give every gate a state: met, failing, stale or missing, with the reading or ticket that decides it. A gate with no reading is missing. Done when every gate has its line and every engine gate has a frontier (`cargo xtask track next`), a /frontier run queued, or the person's ticket it waits on.
3. **Choose.** Fill each free slot in this order: a regression or a blocker on a merge; then a ticket that unblocks a gate on the go/no-go path; then the engine gate furthest from its evidence. Within the gate, cut by **cohesion**, one code path per group, as /specs §1–3 cut. A decision ticket goes to /grill-design, an empty frontier to /frontier. Done when each free slot has a spec number, its gate and its path, and no two slots share a path.
4. **Dispatch** one brief per builder, as *Briefs* says. Done when each brief names its ticket, gate, checkout, branch, owned paths, acceptance boxes and model.
5. **Yield.** Completions arrive through the runtime; don't poll or sleep. When work returns, run /specs' integrating and publishing phases over what is ready. When priorities change, move capacity at the next safe checkpoint (a builder's command finished or parked), never mid-command.
6. **Checkpoint.** At every dispatch, ready, merge, publish or change of priority, rewrite the sections that changed. Return to step 2 after every publication.

## The checkpoint

`ROADMAP.md` is an **index**, not a log: a fresh coordinator or a person reads it cold in one pass. Every section is rewritten in place, it stays under 80 lines, and it links to git, the tracker, the store and the runtime rather than copying them. Command logs, per-command timestamps, seat lines, finished handles and anything `git log`, `gh` or `xtask status` can answer stay out. Refer to tickets by title with the number inside, "Every run works in its own pinned tree (#1183)", and set short hashes apart from the words around them. Its sections, in order: `Owner: <session>, since <UTC>`; **Board (#3466)**, a table of gate, state, evidence and next ticket; **Phase**, building, integrating or publishing, with base, candidate and the comparisons still owed; **Slots**, a table of ticket, gate, checkout and branch, model, and state since UTC; **Ready, not merged**, each with its branch and head; **Waiting on a person**, each with its exact condition; **Next**, what fills the next free slot and why, in a line or two.

## Briefs

A subagent knows only its brief. The team is flat: you dispatch every builder, a builder may dispatch the reviews its build needs, and reviewers and advisors dispatch no one. Use a temporary lead only for a bounded objective of several coordinated subtasks, with a share of the three slots, a deliverable and an end condition. Models: a builder is Opus at medium for a well-specified spec on one path, Opus at high for interacting engine invariants, and Haiku at high for mechanical work (a lock line, a re-pin, a rename across call sites). An advisor is Opus at high, for one question whose answer changes the work. A reviewer is Haiku at high for compliance with the spec, AGENTS.md and the glossary, and Fable at high for unsafe, GC, frame transitions and concurrency. Escalate the model when the reasoning is hard; when evidence, a permission or the boundary is missing, fix the brief instead.
- **Builder:** the ticket title (#n) and the gate it moves. Its checkout's absolute path, branch `t<n>` and base hash, where it runs /build's preflight first. The paths it owns and the paths other slots own. Done means the acceptance boxes plus a ready record per `.claude/skills/build/SKILL.md`. Use /implement where it is loaded, and /tdd at the witness. Absorb a gap on its path that fits the spec, and file anything else. The handoff gives ticket, head and base, ready record or blocker, command exits, and resources released.
- **Advisor:** one question and the decision its answer changes. The design sections, php-src files at the pinned tag, and readings to read. A recommendation with each claim cited; the advisor edits nothing.
- **Reviewer:** the diff base..head. The standard: the spec's boxes, AGENTS.md and CONTEXT.md's _Avoid_ lists, plus the invariants in question for a Fable review. Findings each with file:line and a concrete failure scenario. "Nothing found" is an answer.

## Report and finish

Tell the user only when a gate changes state, a batch publishes, or a person is needed: one line each, with the gate, what moved and the evidence. The mission is done when every engine gate is met on `main` and every person's gate waits on a filed `for-a-person` ticket that names its exact condition. An empty frontier proves nothing, and waiting on builders or CI is a yield.
