# The negative controls of `xtask steer`

`tracked.toml` is what the instrument is handed as the file on disk: a
slice of `seams.toml`'s shape, one `[budget]` table and two `[[rule]]`
rows beside two `[[crate]]` rows. Every other file here is a proposal
against it, and each one must be refused with nothing written
(harness-design.md §4.4, §11, ledger 51):

| proposal | what it does | the path the refusal names |
|---|---|---|
| `changed-value.toml` | raises a ceiling the row already carries | `crate[ext/standard].budget` |
| `dropped-key.toml` | drops a key the row already carries | `crate[ext/standard].budget` |
| `dropped-row.toml` | drops a row the file already carries | `rule[lanes.nightly] dropped` |
| `moved-row.toml` | moves a row past another while adding a key | `rule[lanes.nightly] moved` |

`added-key.toml` is the control beside them: the one addition the
widening admits, `exempt` on a `[[rule]]` row the file already carries,
which is written and named in the reading. `xtask/tests/steer.rs` runs
all five.

## A conflicted merge (#2880)

`merge/` holds a merge of `docs/divergence.toml` whose two sides each
append a row after the same last one, so git stops on the conflict and
the working file holds its markers: `base.toml` is the merge base,
`head.toml` the base with `HEAD`'s row, `merged.toml` the base with
`MERGE_HEAD`'s, and `merged-reworded.toml` that side rewording the
base's reason as well. While the merge stands, steer judges a proposal
against the blobs git keeps for the conflict, `HEAD`'s file among them,
never the markers. A side's own change over the base is part of the
union, as it stands on that side (#3633):

| `MERGE_HEAD` | proposal | the reading |
|---|---|---|
| `merged.toml` | `union.toml`: `HEAD`'s file and `MERGE_HEAD`'s row | written, judged against `HEAD`'s file, naming `divergence[trace:fixture-head]` and `divergence[trace:fixture-merged]` |
| `merged-reworded.toml` | `union-reworded.toml`: the union, with `MERGE_HEAD`'s reason | written, naming `divergence[trace:fixture-merged]` and `divergence[trace:fixture-base].reason` |
| `merged-reworded.toml` | `union.toml`: the union, with the base's reason back | refused at `divergence[trace:fixture-base].reason` |
| `merged-reworded.toml` | `changed-reason.toml`: the union, rewording that reason again | refused at `divergence[trace:fixture-base].reason` |
| `merged.toml` | `changed-reason.toml`: the union, rewording the base's row | refused at `divergence[trace:fixture-base].reason` |
| `merged.toml` | `neither.toml`: the union and a row neither side carries | refused at `divergence[trace:fixture-neither]` |

`xtask/tests/steer.rs` lays each merge in a scratch repository and runs
all six.

## A clean merge (#4441)

`merge/head-reworded.toml` is the base with `HEAD`'s reason reworded, a
person's commit, and nothing appended; merged with `merged.toml`, the
branch's appended row, git takes the two with no conflict, and
`merge/clean.toml` is the text it stages. Judged against that working
copy a proposal of it would add nothing, so steer judges it against the
three commits' files as it judges a conflict, and holds the proposal to
the text `git merge-file` gives of them as well:

| `HEAD` | `MERGE_HEAD` | proposal | the reading |
|---|---|---|---|
| `head-reworded.toml` | `merged.toml` | a copy of the staged file, `clean.toml` | written, naming `divergence[trace:fixture-base].reason` as `HEAD`'s and `divergence[trace:fixture-merged]` as brought; pre-commit then admits the agent's merge |
| `head-reworded.toml` | `merged.toml` | `merged.toml`: `HEAD`'s reason put back | refused at `divergence[trace:fixture-base].reason` |
| `head-reworded.toml` | `merged.toml` | `head-reworded.toml`: the branch's row dropped | refused at `divergence[trace:fixture-merged] dropped` |
| `head-reworded.toml` | `merged.toml` | `clean.toml` with the branch's row's `since` moved: a change neither side made | refused at `divergence[trace:fixture-merged].since`, and as not the text git's three-way merge gives |
| `head-reworded.toml` | `merged.toml` | `clean.toml` and a comment: the union in another text | refused as not the text git's three-way merge gives |
| `head-reworded.toml` and `head.toml`'s row | `merged.toml`, merged with `-X theirs` | a copy of the staged file | refused at `divergence[trace:fixture-head] dropped`, and as a merge a strategy option took |

`xtask/tests/steer.rs` lays each merge in a scratch repository and runs
all six, the first through the `steer` and `permit --diff` binaries.

## An `[[owed]]` row (#3636)

`owed/tracked.toml` is a slice of `seams.toml`'s `[[owed]]` rows, each
named by its `instrument`. `owed/held-at.toml` is the control: `held_at`
added to both rows, two additions, written. The negative controls beside
it are refused with nothing written:

| proposal | what it does | the path the refusal names |
|---|---|---|
| `owed/changed-when.toml` | changes a `when` the row already carries | `owed[crosscheck].when` |
| `owed/second-row.toml` | adds a second row for an instrument already owed | `owed[phpt] twice` |
