# The crosscheck negative control

`oracle.re` is what the pinned php prints on its first stream for `php -n
--re ctype` under the pin's recipe, trimmed as the recipe trims them; the
bytes a run compares and keeps are these with the load number standing as
the pin's `extension_number_token`. `engine.re` is the same rendering with one byte
changed, the `m` of `ctype_alnum` on line 4 standing as `n`.
`renumbered.re` is the same rendering with its load number changed, the
`#10` of line 1 standing as `#7`: the oracle's build order, which the
pin's `extension_number_token` row replaces in both streams (#317), so it
is a pass under the row and the same bytes without the row are not.

`oracle.ri` is the `--ri` column's counterpart (#273): what the pinned php
prints for `php -n --ri ctype`, `php_info_print_module` in text mode,
trimmed the same way; `engine.ri` is the same rendering with one byte
changed, the `e` of `enabled` on line 3 standing as `E`. A `--ri` rendering
carries no load number, so it has no renumbered counterpart.

The control that must fail is in `xtask/src/crosscheck/tests.rs`: the two
files through the lane's verdict are not a pass, and the difference it
prints names both lines; and in `xtask/tests/crosscheck.rs`, where a
directory holding `engine.re` as `ctype.re`, or `engine.ri` as
`ctype.ri`, stands for the engine's rendering through `run`. Byte-identity is the whole standard, so one byte
is the smallest difference there is and the one worth holding the lane to
(harness-design.md §14 item 4).
