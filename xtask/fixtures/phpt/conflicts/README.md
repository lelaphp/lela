Tests held apart as run-tests.php holds them (run-tests.php:1341-1372). Each
program sleeps and writes the span it ran over beside itself, so a test can
read what ran at once. `a-keyed/one.phpt` and `two.phpt` name the key `shared`
in `--CONFLICTS--` and never run at once; `free1.phpt` and `z-free.phpt` name
none and run beside each other, the negative control that shows the pool runs
anything at once at all. On a pool of eight the batches are two neighbours
each (run-tests.php:1537-1541, #1091), and the names put `one.phpt` and
`two.phpt` in two batches and `c-directory`'s `one.phpt` and `two.phpt` in two,
so the keys and not the batches hold them apart. `b-alone/all.phpt` names `all` and runs with nothing
beside it. `c-directory/CONFLICTS` gives `one.phpt` and `two.phpt` the key
`directory`, and `own.phpt`'s own section, `own`, stands in its place, so it
runs beside them. Each control pair, `free1.phpt` and `z-free.phpt`, `own.phpt`
and `c-directory/two.phpt`, leaves a `.here` file and waits, ten seconds at
most, for the other's, so the pair's overlap is made by the files and not won
against the load (#1105). Held by `xtask/src/phpt/tests.rs` and, for the keys alone,
`xtask/src/phpt/file.rs`.
