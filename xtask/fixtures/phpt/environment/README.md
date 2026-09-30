Tests of the environment and the streams a test's programs get, which are
run-tests.php's at the pinned tag (#1186). `stdin-pipe.phpt` and
`stdin-section.phpt` find a pipe on stdin, empty or holding the `--STDIN--`
section, where `fseek` fails as it fails on a pipe; `skipif-stdin.phpt`
skips unless its `SKIPIF` finds a pipe too; `stdout-pipe.phpt` finds a pipe
on stdout, which `php://stdout` cannot seek (as `bug45303.phpt` asks);
`environment.phpt` finds `SKIP_ONLINE_TESTS` and `SKIP_IO_CAPTURE_TESTS`
set, and neither its empty `--ENV--` value nor `TZ` and `QUERY_STRING`,
which the runner empties for every test, in its environment, since php's
`proc_open` leaves an empty value out. `xtask/tests/phpt.rs` runs them under
the pinned php and every one passes; run again through a binary that hands
its program `/dev/null` on stdin and a file on stdout, as the runner did
before, the three stream tests fail.
