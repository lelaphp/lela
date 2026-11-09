# The workloads declaration's controls

`docs/perf/workloads.toml` declares every program under `docs/perf/workloads/`
(#4344). Each tree here is a workspace root holding a declaration and its
directory, read by `xtask/src/workloads.rs`'s tests, and each must be refused:
`stray/` holds `stray.php`, a program with no row, and `gone/` declares
`gone`, a row whose program is not there.
