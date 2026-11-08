# The hostile lane's controls

`xtask hostile` (#4265) holds `data/hostile.toml`'s programs to the oracle.
These are its controls each way, read by `xtask/src/hostile/tests.rs`.

The corpus: `corpus.toml` is a corpus of two programs that decodes;
`corpus-id-twice.toml`, `corpus-unknown-form.toml` and
`corpus-misspelt-id.toml` must each be refused, naming what is wrong.

The verdict: `base.json` is a run kept at a base, in the shape a run is kept
at `target/harness/hostile-runs/<engine tree>.json`: `kept-met` met php
there, `kept-differs` differed, `kept-accepted` differed and a row accepted
it. Held to it, a program that met php there and differs now must fail, one
that differed there and differs now passes and is counted, and a fault fails
whatever the base held. `accepts.toml` is a registry whose accepted `trace:`
row covers `hostile:kept-accepted`, the one way a hostile difference is
accepted (Round 4, Q33); `witness-covers.toml` is a `witness:` row covering a
`hostile:` id, which the registry must refuse.
