Tests that write where a run of them stands, as the corpus's tests do:
`beside.phpt` leaves a file beside itself, as `Zend/tests/offsets` leaves its
`.txt` files; `relative.phpt` leaves a file and a directory relative to the
root every program runs from, as the strays `bad.txt` and `ftp:` stand at the
pinned tree's root; `sections.phpt` leaves one from its `SKIPIF`, one from
its program through a `{PWD}` path in `--ENV--`, and one from its `CLEAN`.
Each also says the working directory is the tree `TEST_PHP_SRCDIR` names and
that it stands inside it. `stray.phpt` writes nothing: the test that holds it
lays an untracked `stray.php` beside it in the pinned tree, where its program
goes, which borked it while programs were written into that tree (#1183).
A runner that writes into the pinned tree leaves every one of these there,
and `xtask/tests/phpt.rs` fails it by listing the tree before and after.
