# The `fuzz` lane's fixture

`corpus/` is a corpus of one program, `disagreed.php`. The lane's tests
in `xtask/src/fuzz/tests.rs` run it under stand-in binaries, both
shell programs: the oracle's prints one line whatever it runs, and the
engine's prints another where the program carries the comment line
`// the engine is made to differ here`, and the oracle's line otherwise.

So the lane must fail on this corpus, naming the program minimised to
its opening tag and that comment line; with an engine that agrees, the
same corpus passes. The same tests hold that one seed draws the same
programs from it twice.
