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
