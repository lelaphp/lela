Two runs of one subset as `xtask phpt` keeps them, at
`target/harness/phpt-runs/<tree>/<scope>.json`: `base.json` at the merge base and
`head.json` at the head. One regression is planted: `planted.phpt` passed at
the base and fails at the head, so `phpt --against` fails its reading and
names it. The other tests are the controls: `fixed.phpt` newly passes,
`still.phpt` fails at both, `diverged.phpt` is met at both under a registry
row, and `skipped.phpt` fails at the head after a skip at the base, which met
nothing there and so is no regression. Held by `xtask/src/phpt/against/tests.rs`.

`newer.json` is a run a later xtask kept: a verdict, a miss's signature and a
field this one has not got. Every worktree's xtask reads every worktree's runs,
so it is read with the verdict and the miss left out and the field ignored, and
a file that is no run at all is passed over with a note, never refused.
