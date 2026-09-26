Tests held apart as run-tests.php holds them (run-tests.php:1341-1372). Each
program sleeps and writes the span it ran over beside itself, so a test can
read what ran at once. `a-keyed/one.phpt` and `two.phpt` name the key `shared`
in `--CONFLICTS--` and never run at once; `free1.phpt` and `free2.phpt` name
none and run beside them, the negative control that shows the pool runs
anything at once at all. `b-alone/all.phpt` names `all` and runs with nothing
beside it. `c-directory/CONFLICTS` gives `one.phpt` and `two.phpt` the key
`directory`, and `own.phpt`'s own section, `own`, stands in its place, so it
runs beside them. Held by `xtask/src/phpt/tests.rs` and, for the keys alone,
`xtask/src/phpt/file.rs`.
