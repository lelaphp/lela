---
name: frontier
description: A track's map at one row of third-engine-design.md §11 and its batches of tickets; `/frontier chart <days>` lays the map down and files the first batch; run from a track's clone or from main.
disable-model-invocation: true
---

A track works a frontier: the open, unblocked tickets `xtask track next` names. This skill is what fills it, one batch per session, from a **map** on the tracker. `chart <days>` lays the map down at the §11 row the engine stands at and files the first batch. `work`, the next batch when the frontier empties, is #84 and is written beside chart when it lands. A session runs one mode once: it files, prints what it filed and ends. Nothing here measures the engine and nothing here decides for the design of record.

## Where you stand

- `cargo run -q -p xtask -- track --json` says where you run from: `track` is the track's name, or `main` for the main checkout. From a track's clone every issue filed here carries `track:<name>`; from main it carries no `track:` label, so every track takes it.
- The state is the store's renderings and nothing re-measured: `.claude/harness.local.md` (`budget`, `phpt`, `witness`, `divergence`) and `cargo run -q -p xtask -- status --section week`, which says "no reading" until the manifest carries `[[week]]`. A rendering that is stale is what you have; running an instrument to refresh it is another session's act.
- The row is the `Days` cell of one row of §11's table, `1–2`, `3–7`, `8–14`, `15–21`, `22–30` or `31`, and its `Work` and `Exit bar` cells are the whole source of the batch. The design carries no numbers of its own; a ceiling or an interface it names lives in `seams.toml` and a lock.

## Labels

Four labels, each created when `gh label list --json name --jq '.[].name'` lacks it, and described so a person reads them cold:

- `frontier:map`: The map of one track's frontier; its tickets are its sub-issues
- `frontier:research`: A fact outside the tree that a decision waits on; resolved by `/research`
- `frontier:decision`: A decision the design of record does not make; resolved by `/grill-design`, a person's
- `track:<name>`: Only the <name> track works this ticket; a ticket with no track: label is every track's

`gh label create <name> --description "<text>" --color <hex>`; a label that exists is left as it is.

## The map

One issue, labelled `frontier:map` (and `track:<name>` from a clone), titled `Map: <track> at §11 days <row>`. Its body is the whole map at low resolution, loaded once per session; open tickets are not listed on it, they are its open sub-issues:

```markdown
## Destination

<the row's exit bar in one or two lines, naming the row; every session orients to it before choosing a ticket>

## Notes

<the row's Work cell as it bears on this track; the design sections and skills a session consults; standing preferences>

## Decisions so far

<!-- one line per closed ticket: [<title>](url), a one-line gist; the map indexes, the ticket holds the detail -->

## Not yet specified

<!-- the fog: in-scope work of the row not yet sharp enough to ticket; it graduates into tickets as closes make it sharp -->

## Out of scope

<!-- work ruled past the destination; closed, never graduates -->
```

The map indexes and never restates: a decision lives in its ticket, the map gists it and links. In everything a person reads, a ticket goes by its title with the link inside the name, never by a bare number.

## A ticket

One concern of the row, sized to one session, filed as a sub-issue of the map (`gh issue create --parent <map>`) with the body shape every ticket in this repository has, since `xtask ticket read` holds a ticket to the boxes under `## Acceptance criteria`:

```markdown
## What to build

<what and why, citing the design section or ADR>

## Acceptance criteria

- [ ] <one condition an instrument or a reading shows: a reading named and green, a lock line in the diff, a witness pinned, a file that exists>

## Blocked by

- <the blocking tickets by title, or "None, can start immediately">
```

Fog or ticket: a question you can state precisely now is a ticket, blocked or not; one you cannot is a line under Not yet specified, coarser than a ticket, not pre-sliced. Every ticket carries `ready-for-agent` where an agent may take it, `for-a-person` where the act is a person's (harness-design.md §11). A ticket that is a question rather than a build carries its `frontier:` type as well: `frontier:research` when a fact outside the tree decides it, `frontier:decision` when the design of record does not say and a person must; a decision the batch needs is such a ticket, never the planner's own. Build tickets carry no `frontier:` type.

Blocking is the tracker's own relation, wired in a second pass once the numbers exist: `gh issue edit <n> --add-blocked-by <a,b>` for each ticket another ticket of the batch or of the tracker blocks. `xtask track next` reads that relation and the `ready-for-agent` label to find the frontier; a heading in a body finds nothing.

## chart <days>

1. Read where you stand, the row named in the argument and the renderings. Without a row, or with one that is no `Days` cell of §11, stop and say so; chart files nothing on a guess.
2. Create the labels the batch needs that are missing.
3. File the map: Destination from the row's exit bar, Notes, Decisions so far empty, the fog of the row sketched under Not yet specified, Out of scope empty.
4. File the first three to five tickets of the row: the work the renderings show undone that is sharp enough to ticket now, each one concern with acceptance boxes `done` can hold, each a sub-issue of the map, typed and labelled as above. What is not sharp stays in the fog.
5. Wire blocking in the second pass, then write the `## Blocked by` list of each ticket from the relation as it now is.
6. Print what you filed: the map by title and number, each ticket by title, number, type and what blocks it. Then end; charting is one session's work and resolves no ticket.
