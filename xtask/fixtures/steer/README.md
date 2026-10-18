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
never the markers:

| `MERGE_HEAD` | proposal | the reading |
|---|---|---|
| `merged.toml` | `union.toml`: `HEAD`'s file and `MERGE_HEAD`'s row | written, judged against `HEAD`'s file, naming `divergence[trace:fixture-head]` and `divergence[trace:fixture-merged]` |
| `merged-reworded.toml` | `union.toml` | refused at `divergence[trace:fixture-base].reason on MERGE_HEAD` |
| `merged.toml` | `changed-reason.toml`: the union, rewording the base's row | refused at `divergence[trace:fixture-base].reason` |
| `merged.toml` | `neither.toml`: the union and a row neither side carries | refused at `divergence[trace:fixture-neither]` |

`xtask/tests/steer.rs` lays each merge in a scratch repository and runs
all four.
