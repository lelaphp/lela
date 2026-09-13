## What to build

The negative control of `harness.self` at the read (#153): a ticket
carrying `ready-for-agent` whose criteria ask for two writes into the
rule's scope. The pin is one `xtask steer` makes for an agent, so the
label stands and the reading says what it matched; the hook settings are
one no instrument makes, so the label is wrong and the reading fails
(#111 in the shape it was filed in, whose first criterion is the first of
these).

## Acceptance criteria

- [ ] `oracle.toml` carries a `[regime.dump]` with the ini above, with the comment that says what it is
- [ ] `.claude/settings.json` gains the hook the lane runs under
- [ ] `cargo xtask compile-identity` runs the regime rather than refusing
- [ ] One fixture under `xtask/fixtures/` that must fail: a recipe whose digest does not match what `oracle.toml` pins

## Blocked by

- None, can start immediately
