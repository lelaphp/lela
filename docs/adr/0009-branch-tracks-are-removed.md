---
status: accepted
date: 2026-10-03
supersedes: harness-design.md revision 14 (`done`'s default base is the branch's track), revision 16's prefix rule (a branch's track is the longest branch that is a proper path prefix of its name), revision 19's clone-hosted run (a clone recording `track.frontier=auto` fills its own frontier), revision 21 (`xtask track sync`)
---

# Branch tracks are removed; a track is a label on the tracker, worked from main

Tracks came from harness revisions 14, 15-16, 19 and 21, not from an ADR. Together they made a track a branch with its own clone, store and model: `xtask track new` laid the clone down, `track sync` merged `main` into that branch before each ticket, the prefix rule named a branch's track for `done`, and the clone's `track.*` records chose the model and whether the frontier filled itself. The use went away before the machinery did. `t-opus-5` merged into main on 2026-09-25, and nothing ran on a branch track after. The parity tracks (#1177 to #1182) are labels with no branch. Every branch track still costs a row of CI and a view of the dashboard. And #1735 found that nothing says what a track's name is: `track new` accepts any name, CI and the dashboard read only `t-*`, and the prefix rule cannot list tracks from `origin`, where ticket branches never go. Settled by Mitchell in the dashboard architecture review of 2026-10-03, on #1748.

## Decision

- A track is the tickets that carry one `track:<name>` label, worked toward the destination of the `frontier:chart` issue with the same label. It has no branch, clone or store.
- A ticket with no `track:` label is `main`'s, so every ticket is exactly one track's.
- The word stays "track".
- Every ticket is worked on its own branch, `t<n>`, cut from `main`. `done`'s default base is the merge base with `main`.
- `xtask track new`, `track sync`, the prefix rule and the clone's `track.*` records go. The frontier loop stays: `xtask track next`, `chains` and `work`, each taking `--track <name>`.
- When `track work` ends a ticket with it closed, the ticket's branch is left for /specs to merge, and the loop never pushes `main` (#1749).

## Considered options

- **Keep branch tracks for comparing models.** Rejected: nothing has run on one since 2026-09-25, and each still costs its CI and dashboard rows. A run's model can be a flag on `track work` instead (#1754).
- **Keep `track new` and drop `track sync`.** Rejected: a track that never syncs lays its worktrees off a harness older than main's, which is what revision 21 added `sync` to prevent. And `new` alone still costs a clone, a store and the per-track rows.
- **#1735's "require `t-*`".** Rejected: it gives CI and the dashboard a rule for naming tracks, and keeps the branch, clone and store whose cost is the reason for this decision.

## Consequences

- Revisions 14, 16's prefix rule, 19's clone-hosted run and 21 are superseded. The ledger keeps them in its own words as a record. ADR-0009 is cited from revision 68, which lands this decision in CONTEXT.md's Track, Command and Frontier entries and in harness-design §4.5, §5 and §11.
- Two `track work` runs on different tracks never compete for an unlabelled ticket, because that ticket is `main`'s.
- The /specs skill's "lanes" become "tracks" (#1750). "Lane" stays the harness's word for a scheduled run of a corpus.
- The code follows in its own tickets: `done`'s base (#1752), `track new` (#1753), `track work`'s `--track`, `--model` and `--fill` (#1754), `track sync` and the merge back (#1755), the prefix rule and the clone's records (#1756), CI's triggers (#1757), the dashboard (#1751), the skills (#1750), and retiring `t-opus-5` (#1758). Until they land, the code still carries what this ADR removes.
- Passages of harness §4.5 and §5 and of `AGENTS.md` that still describe branch tracks, and that the settled sentences do not reach, wait on a person's wording (#1767).
