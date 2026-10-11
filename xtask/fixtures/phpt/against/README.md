Two runs of one subset as `xtask phpt` keeps them, at
`target/harness/phpt-runs/<tree>/<scope>.json`: `base.json` at the merge base and
`head.json` at the head. One regression is planted: `planted.phpt` passed at
the base and fails at the head, so `phpt --against` fails its reading and
names it. The other tests are the controls: `fixed.phpt` newly passes,
`still.phpt` fails at both, `diverged.phpt` is met at both under a registry
row, and `skipped.phpt` fails at the head after a skip at the base, which met
nothing there and so is no regression. Held by `xtask/src/phpt/against/tests.rs`.
