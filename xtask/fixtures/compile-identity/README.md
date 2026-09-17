# The compile-identity negative control

`oracle.dump` is what the pinned php prints on its second stream compiling
`identity.php`, the program in `corpus/identity.phpt`, under the pin's
recipe and its `dump` regime: the bytes a run compares, trimmed as the
recipe trims them, with the directory the program was run from standing
as the recipe's scratch token, since a program runs by its absolute path
(#331) and php prints that path into each block. `engine.dump` is the same dump with one line changed,
the `MUL` of opline `0002` taking `int(3)` where php compiled `int(2)`.

The control that must fail is in `xtask/src/compile_identity/tests.rs`: the
two files through the lane's verdict are not a pass, and the difference it
prints names both lines. Byte-identity is the whole standard, so one line
is the smallest difference there is and the one worth holding the lane to
(harness-design.md §12 item 4).
