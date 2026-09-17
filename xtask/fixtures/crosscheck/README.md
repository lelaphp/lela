# The crosscheck negative control

`oracle.re` is what the pinned php prints on its first stream for `php -n
--re ctype` under the pin's recipe: the bytes a run compares, trimmed as
the recipe trims them. `engine.re` is the same rendering with one byte
changed, the `m` of `ctype_alnum` on line 4 standing as `n`.

The control that must fail is in `xtask/src/crosscheck/tests.rs`: the two
files through the lane's verdict are not a pass, and the difference it
prints names both lines; and in `xtask/tests/crosscheck.rs`, where a
directory holding `engine.re` as `ctype.re` stands for the engine's
rendering through `run`. Byte-identity is the whole standard, so one byte
is the smallest difference there is and the one worth holding the lane to
(harness-design.md §14 item 4).
