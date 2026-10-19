---
name: frontier
description: A track's chart at one row of third-engine-design.md §11 and its batches of tickets; `/frontier chart <track> <days>` lays the chart down and files the first batch, `/frontier work <track>` files the next when the frontier empties; the track is a `track:` label, or `main`.
disable-model-invocation: true
---

A track works a frontier: the open, unblocked tickets `xtask track next --track <track>` names. This skill is what fills it, one batch per session, from a **chart** on the tracker. `chart <track> <days>` lays the chart down at the §11 row the engine stands at and files the first batch. `work <track>` files the next batch when the frontier empties: it indexes the closes since the last batch on the chart and graduates the fog they made sharp. A session runs one mode once: it files, prints what it filed and ends. Nothing here measures the engine and nothing here decides for the design of record.

## Where you stand

- The track is the first argument, `<track>`: the tickets that carry the label `track:<track>`, or `main`, the tickets that carry no `track:` label (CONTEXT.md, Track; ADR-0009). A track has no branch, clone or store, so nothing here reads it from where you run. Every issue filed for a named track carries `track:<track>`; one filed for `main` carries no `track:` label, which makes it `main`'s.
- The state is the store's renderings and nothing re-measured: `.claude/harness.local.md` (`budget`, `phpt`, `witness`, `divergence`). A rendering that is stale is what you have; running an instrument to refresh it is another session's act.
- The row is the `Days` cell of one row of §11's table, `1–2`, `3–7`, `8–14`, `15–21`, `22–30` or `31`, and its `Work` and `Exit bar` cells are the whole source of the batch. The design carries no numbers of its own; a ceiling or an interface it names lives in `seams.toml` and a lock.

## Labels

Four labels, each created when `gh label list --json name --jq '.[].name'` lacks it, and described so a person reads them cold:

- `frontier:chart`: The chart of one track's frontier; its tickets are the ones that carry its track: label
- `frontier:research`: A fact outside the tree that a decision waits on; resolved by `/research`
- `frontier:decision`: A decision the design of record does not make; resolved by `/grill-design`, a person's
- `track:<name>`: Only the <name> track works this ticket; a ticket with no track: label is main's

`gh label create <name> --description "<text>" --color <hex>`; a label that exists is left as it is.

## The chart

One issue, labelled `frontier:chart` (and `track:<track>` for a named track), titled `Chart: <track> at §11 days <row>`. Its body is the whole chart at low resolution, loaded once per session; open tickets are not listed on it, they are the track's open tickets:

```markdown
## Destination

<the row's exit bar in one or two lines, naming the row; every session orients to it before choosing a ticket>

## Notes

<the row's Work cell as it bears on this track; the design sections and skills a session consults; standing preferences>

## Decisions so far

<!-- one line per closed ticket: [<title>](url), a one-line gist; the chart indexes, the ticket holds the detail -->

## Not yet specified

<!-- the fog: in-scope work of the row not yet sharp enough to ticket; it graduates into tickets as closes make it sharp -->

## Out of scope

<!-- work ruled past the destination; closed, never graduates -->
```

The chart indexes and never restates: a decision lives in its ticket, the chart gists it and links. In everything a person reads, a ticket goes by its title with the link inside the name, never by a bare number.

The chart takes no sub-issues, and there is one per track per row for as long as the row stands. It was parented once and GitHub caps a parent at 100: the cap was reached nine times at this one row, twice within a working day and once inside eighty minutes, and every rollover is a person's act that fell behind the filing it was meant to serve (#619). What groups a track's tickets was never the parent. It is the `track:` label and the tracker's blocking relation, which is what `xtask track next` and `xtask track chains` read and all they read; nothing in the harness has ever read the parent. So a ticket belongs to the chart by its label, the chart is never continued, and the sheets already filed are left where they are.

## A ticket

One concern of the row, sized to one session, filed with the track's label, none for `main`, and no parent (`gh issue create --label track:<track>`) in the body shape every ticket in this repository has, since `xtask ticket read` holds a ticket to the boxes under `## Acceptance criteria`. Before filing, search the open tickets for the construct, `gh issue list --state open --search "<construct>"`, and extend one that covers it rather than file a second; a batch cut from a subset's failures is one ticket per cause, naming the tests each explains, never one per failing test:

```markdown
## What to build

<what and why, citing the design section or ADR>

## Acceptance criteria

- [ ] <one condition an instrument or a reading shows: a reading named and green, a lock line in the diff, a witness pinned, a file that exists>

## Blocked by

- <the blocking tickets by title, or "None, can start immediately">
```

Fog or ticket: a question you can state precisely now is a ticket, blocked or not; one you cannot is a line under Not yet specified, coarser than a ticket, not pre-sliced. Every ticket carries `ready-for-agent` where an agent may take it, `for-a-person` where the act is a person's (harness-design.md §11). A ticket that is a question rather than a build carries its `frontier:` type as well: `frontier:research` when a fact outside the tree decides it, `frontier:decision` when the design of record does not say and a person must; a decision the batch needs is such a ticket, never the planner's own. Build tickets carry no `frontier:` type.

Blocking is the tracker's own relation, wired in a second pass once the numbers exist: `gh issue edit <n> --add-blocked-by <a,b>` for each ticket another ticket of the batch or of the tracker blocks. `xtask track next --track <track>` reads that relation and the `ready-for-agent` label to find the frontier; a heading in a body finds nothing.

## chart <track> <days>

1. Read where you stand, the track and the row named in the arguments and the renderings. Without a track, without a row, or with a row that is no `Days` cell of §11, stop and say so; chart files nothing on a guess.
2. Create the labels the batch needs that are missing.
3. File the chart: Destination from the row's exit bar, Notes, Decisions so far empty, the fog of the row sketched under Not yet specified, Out of scope empty.
4. File the first three to five tickets of the row: the work the renderings show undone that is sharp enough to ticket now, each one concern with acceptance boxes `done` can hold, typed and labelled as above. What is not sharp stays in the fog.
5. Wire blocking in the second pass, then write the `## Blocked by` list of each ticket from the relation as it now is.
6. Print what you filed: the chart by title and number, each ticket by title, number, type and what blocks it. Then end; charting is one session's work and resolves no ticket.

## work <track>

1. Read where you stand, the track named in the argument and the renderings; without a track, stop and say so. `cargo run -q -p xtask -- track next --track <track>` says `none` when the frontier is empty; while it names a ticket, stop and say so: work files nothing on top of a frontier still standing. Find the chart, the one open `frontier:chart` issue carrying `track:<track>`, or no `track:` label for `main` (`gh issue list --label frontier:chart --state open --json number,title,labels`); without one, or with two, stop and name it, since `chart <track> <days>` lays a chart down and work walks one.
2. Load the chart body, the low-resolution view. The closes since the last batch are the track's closed tickets, `gh issue list --label track:<track> --state closed`, or for `main` the closed tickets with no `track:` label (`gh issue list --state closed --limit 1000 --json number,title,labels --jq '.[] | select(all(.labels[]; .name | startswith("track:") | not))'`), carrying no line under Decisions so far and none under Out of scope, and the same of the sheets filed before the parenting went, which the chart's Notes name while any close of one is still unindexed; the chart indexes every close, so a missing line is what marks one new. Zoom each: its body and the closing comment `xtask ticket close` left, naming the commit and what the work opened. Append one line per close to Decisions so far, `[<title>](url), <gist>`, the gist one line and the detail left in the ticket.
3. Rule out of scope what a resolution showed sits past the destination: a ticket of the track, open or closed, whose work lies beyond the row's exit bar. Take it off the frontier, `gh issue edit <n> --remove-label ready-for-agent --add-label for-a-person`, comment on it naming the destination it sits past, and gist it under Out of scope with the link and the reason. The close itself is a person's: `gh issue close` is not available to you (ticket.close) and `xtask ticket close` closes work that met its boxes. A ticket ruled out gets no line under Decisions so far; that section is the route walked.
4. Graduate the fog: each patch of Not yet specified the closes made sharp becomes tickets, filed as in A ticket, three to five in the batch, typed and labelled. Delete the graduated patch from Not yet specified so it lives only as its tickets; a patch that graduated in part stays, rewritten to what is still dim. A patch no close sharpened stays as it is, and one the closes showed past the destination moves to Out of scope. A decision the batch needs is a `frontier:decision` ticket for `/grill-design`, never the planner's own. A ticket on the chart the closes invalidated is edited to what still holds, or ruled out of scope. With the fog empty and nothing to graduate the chart is walked: say so, file nothing; the row's exit bar is measured by the instruments, and the next row is a person's `chart <track> <days>`.
5. Wire blocking in the second pass, then write the `## Blocked by` list of each new ticket from the relation as it now is.
6. Write the chart body back once, `gh issue edit <chart> --body-file <file>`, with Decisions so far, Not yet specified and Out of scope as steps 2 to 4 left them.
7. Print what you filed: each gist appended by title, each ticket ruled out by title and why, each ticket filed by title, number, type and what blocks it. Then end; one batch is one session's work and resolves no ticket.
