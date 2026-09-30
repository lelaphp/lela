The debugger fixture of `xtask phpt` (#1304): `run.phpt` is a `--PHPDBG--`
test whose `SKIPIF` skips it outside the debugger and whose program prints
the SAPI and the words after it, which passes only where the runner hands it
to a phpdbg with its commands on stdin and its `--ARGS--` after `--`, and
runs its `SKIPIF` through a phpdbg too; a runner that skips it, or serves it
through the command line, fails `a_phpdbg_test_runs_through_the_debugger`.
