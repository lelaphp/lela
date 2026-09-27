A test that never ends, `a-never.phpt`, before two that sleep and write the
span they ran over. Under a limit of a few seconds and a pool of three, the
batches are two neighbours, as run-tests.php cuts them (run-tests.php:1537-1541):
`b-quick.phpt` waits for the first, its batch's, and `c-quick.phpt`, in a batch
of its own, is done before the first is past its limit; on a pool of one, the
negative control, it starts after it. Held by `xtask/src/phpt/tests.rs`.
