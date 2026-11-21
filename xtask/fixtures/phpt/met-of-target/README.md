A subset's target and an engine's run of it, as `xtask phpt` keeps them
(#1184): `control.json` is the pinned oracle's verdicts under the runner's
control, kept at `target/harness/phpt-runs/pin-<tag>-sha256-<hex>/<scope>.json`, and
`engine.json` an engine's run of the same tests. Two oracle misses are
planted: `oracle-miss.phpt`, which the oracle fails (as it runs past the
recipe's limit on `ext/openssl/tests/stream_server_reneg_limit.phpt`), and
`oracle-row.phpt`, which it misses under a registry row; with
`oracle-skip.phpt`, which it skips and the engine passes, they are outside
the target, and a reading that counts either miss in `target` fails. The
target is `kept.phpt`, met; `missed.phpt`, missed; `xfail.phpt`, met as the
oracle fails it; and `row.phpt`, met under a registry row though the engine
skipped it (#1161). `--against` over the planted miss, `diverged` at the
base and a fail past the limit at the head, is no regression once the target
is kept. The control keeps what the oracle's fail keeps, its run past the
limit, and the reason it skipped `oracle-skip.phpt`, "For Linux only", as
`ext/zend_test`'s target keeps it for `gh10370_1..4.phpt`, so a reading held
to the target names each test outside it with why (#2033). Held by the tests
of `xtask/src/phpt/target.rs`.
