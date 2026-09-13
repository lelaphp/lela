# zend_try_compile_special_func census, php-src tag php-8.5.9

Which names of `zend_try_compile_special_func_ex` (`Zend/zend_compile.c`) the phpt corpus reaches, read for #184: the names are `crates/php_compile/src/special.rs`'s table, php's order kept, and the counts are `scripts/special-funcs-census.sh` over the pinned tree named in `oracle.toml`. The per-commit corpus is design §6.1's (`Zend/tests`, `tests`, `sapi/cli`), the whole is every `.phpt` at the tag; `sites` counts `name(` in `--FILE--` sections, `files` the tests with at least one, `written` the sites whose first argument opens with a quote or a digit, an upper bound on the calls the constant shape can take. `clone(` counts the keyword written with parentheses beside the 8.5 function. Rerun the script to regenerate; the numbers below are the run of 2026-09-13.

| name | per-commit sites | files | written | whole sites | files | written |
|---|---|---|---|---|---|---|
| `strlen` | 99 | 62 | 21 | 874 | 449 | 59 |
| `is_null` | 11 | 11 | 0 | 67 | 51 | 0 |
| `is_bool` | 2 | 2 | 0 | 34 | 27 | 0 |
| `is_long` | 0 | 0 | 0 | 17 | 7 | 0 |
| `is_int` | 8 | 3 | 1 | 89 | 40 | 4 |
| `is_integer` | 0 | 0 | 0 | 13 | 7 | 0 |
| `is_float` | 11 | 6 | 4 | 26 | 11 | 4 |
| `is_double` | 8 | 1 | 0 | 20 | 3 | 0 |
| `is_string` | 5 | 4 | 1 | 94 | 68 | 1 |
| `is_array` | 12 | 9 | 0 | 232 | 126 | 0 |
| `is_object` | 8 | 6 | 0 | 174 | 94 | 0 |
| `is_resource` | 4 | 4 | 0 | 49 | 42 | 0 |
| `is_scalar` | 1 | 1 | 0 | 14 | 5 | 3 |
| `boolval` | 1 | 1 | 0 | 14 | 3 | 5 |
| `intval` | 5 | 2 | 4 | 43 | 18 | 5 |
| `floatval` | 0 | 0 | 0 | 12 | 3 | 0 |
| `doubleval` | 0 | 0 | 0 | 12 | 3 | 0 |
| `strval` | 11 | 3 | 1 | 61 | 17 | 5 |
| `defined` | 33 | 19 | 31 | 75 | 43 | 72 |
| `chr` | 50 | 14 | 44 | 932 | 229 | 576 |
| `ord` | 7 | 6 | 2 | 122 | 42 | 39 |
| `call_user_func_array` | 72 | 32 | 32 | 113 | 52 | 53 |
| `call_user_func` | 173 | 75 | 69 | 204 | 99 | 81 |
| `in_array` | 12 | 7 | 4 | 161 | 67 | 91 |
| `count` | 126 | 77 | 0 | 1004 | 579 | 3 |
| `sizeof` | 0 | 0 | 0 | 96 | 47 | 0 |
| `get_class` | 168 | 93 | 1 | 576 | 304 | 1 |
| `get_called_class` | 14 | 13 | 0 | 14 | 13 | 0 |
| `gettype` | 37 | 15 | 0 | 950 | 237 | 0 |
| `func_num_args` | 17 | 9 | 0 | 18 | 10 | 0 |
| `func_get_args` | 31 | 19 | 0 | 87 | 44 | 0 |
| `array_slice` | 23 | 11 | 0 | 111 | 30 | 0 |
| `array_key_exists` | 30 | 20 | 9 | 84 | 48 | 22 |
| `sprintf` | 16 | 12 | 15 | 760 | 292 | 505 |
| `clone` | 57 | 24 | 0 | 60 | 27 | 0 |

names reached by the per-commit corpus: 30 of 35; not reached: is_long, is_integer, floatval, doubleval, sizeof
