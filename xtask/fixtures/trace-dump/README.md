# The trace-dump negative control

`loop.php` is one hot `for` loop. `oracle.lines` is what the pinned php
writes on its second stream for it under the recipe, `[regime.jit-inline]`
and the instrument's ini, the `---- TRACE` lines alone, its directory
standing as the recipe's `scratch_token`: the lines a run keeps as the
oracle's. `engine.lines` is what the engine built with the tier's feature
wrote for it under `jit.mode=force` when the fixture was laid: its loop
root on line 4, the body, where php roots a `for` loop at its condition on
line 3 (#3134), and no side trace from the loop's exit (#2964).

The control that must fail is in `xtask/tests/trace_dump.rs`: a binary that
writes `engine.lines` stands for the engine's through `run`, and the
reading is a fail naming both lines of the loop's root; the same binary
writing `oracle.lines` is a pass, and the lines the run keeps for the
oracle are `oracle.lines`.
