A test that never ends, `a-never.phpt`, before two that sleep and write the
span they ran over. Under a limit of a few seconds and a pool of three, the two
are done before the first is past its limit; on a pool of one, the negative
control, they start after it. Held by `xtask/src/phpt/tests.rs`.
