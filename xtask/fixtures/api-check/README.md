# api-check fixtures

One workspace per rule `xtask api-check` evaluates, judged by the tracked
`seams.toml` (harness-design.md §4.1: one negative-control fixture per rule).
Crates are found the way the instrument finds them everywhere: a `[[crate]]`
row of the manifest whose directory holds a `Cargo.toml`. Nothing here is
built; the instrument reads manifests and sources.

| fixture     | rule                                            | must |
|-------------|-------------------------------------------------|------|
| `clean`     | every rule, as the positive control             | pass |
| `lock-line` | a public item with no line in the seam's lock   | fail |
| `lock-order` | a lock whose every item has its line, in an order `--write` would rewrite (#403) | fail |
| `ceiling`   | `api.lock` over `[budget] api_items`, and its `__private` lines over `api_private_items`; the crate's `Cargo.toml` alone, its items laid down by the test at one past the ceiling the manifest carries, one cap at a time (#338, #953) | fail |
| `include`   | a type defined in a module that `include!`s its file from `OUT_DIR`, which the instrument cannot read, with a `pub fn` in an `impl` of another file and no line for it in the lock (#2025): the impl is joined to the type by name; beside it a glob through the including module, which answers for no name, before a glob that defines `Plain` | fail |
| `edge`      | a crate dependency outside its `[[edge]]` rows  | fail |
| `harness-edge` | a crate no `[[edge]]` row names depending on a workspace crate: the harness linking `php_bundle`, which the tracked manifest forbids since #2213 | fail |
| `host-edge` | a host under `sapi/` depending on `php_engine` past its facade (ADR-0008), beside one within its edges, both linked for the pinned php's macOS (#3356) | fail |
| `deny-edge` | a `use` path across a `deny = true` module edge | fail |
| `bundle`    | a bundle that lists an extension before one it depends on, behind no feature: the order written is php's and free under `order = "registration"` (#2255) | pass |
| `registration` | under `order = "registration"` (#1778, #1915), which the test sets on the tracked manifest's `php_bundle` row: a bundle feature that does not switch on the feature of an extension its extension declares `Required`, beside one that does, and a Cargo link and an `Optional` row that require nothing; the test also lays it down in a scratch workspace with its bundle's `src/lib.rs` replaced, for a required extension registered nowhere, a registration behind a `cfg` the instrument does not read, one extension registered twice, a link with no declaration, and, with an extension's `src/lib.rs` replaced, a declaration the instrument does not read | fail |
| `harness-lock` | a public item of the harness with no line in `xtask.lock`, under the rows #55 proposes | fail |
| `ops-lock`  | a hand-edited line of `ops.lock`, the lock rendered from `ops.toml` | fail |
| `tier-face` | a sectioned lock (#1369): php_sapi importing a `tier::*` item `tier.lock`'s `[face]` does not list, which `sapi.lock` does not hold either | fail |
| `tier-kernels` | `tier.lock`'s `[kernels]`, generated from `php_jit`'s `kernels!` lists (#2957): an import of `ops::add_slow`, which no list names, listed under `[kernels]` by hand, beside `ops::sub_slow`, which the list names | fail |
| `tier-module` | an import lock's module line (#3033): php_jit writing `use php_engine::tier;` and calling `tier::unlisted()`, a renamed module's `t::renamed()`, `php_engine::tier::in_place()` written in place and a `use` inside a function, which `tier.lock`'s `[face]` lists `tier` and not; beside `tier::Flow::Exit`, cut to the listed `tier::Flow`, and an inline `mod tier` of its own, which reaches nothing of php_engine | fail |
| `layer-face` | a seam that is one module of its producer (#1959), under a `layer` seam the test adds to the tracked manifest: `php_engine::layer`'s free function with no line in `layer.lock`, and `php_ext`, outside the seam's consumers, re-exporting it beside `php_streams`' own use | fail |
| `same-name` | two `pub` types of one name in two modules, each listing its own methods alone (#79); read by the unit test beside `Inventory::assoc`, not by `hold` | pass |
| `raise-by-name` | a raise naming its class by a lookup of the linked table in `ext/ctype`, beside `ext/json` raising by its handle (#3403, #3656): the raising crate's own reading fails | fail |
| `lints`     | a crate whose `Cargo.toml` lacks `[lints] workspace = true`, so the root manifest's `[workspace.lints]` do not reach it (#2111), beside one that carries it; every other fixture's crates carry the line | fail |
| `surface-edge` | an `ext/*` crate's library dependency on another that no row of the fixture's design §4.10 names with both crates (#2020, #2251): ext/zip linking ext/pcre beside ext/spl's link on the `pcre` → `spl` row, and a dev-dependency on no row, which links nothing and is not held | fail |
| `surface-lock` | an extension's `pub` item beyond `extension()` with no line in its `surface.lock`, and a line naming no item (#2251); `clean` carries the lock of its one export | fail |
| `binding` | a crate's binding to a native library held to the keg the pinned php loads it from (#3458): `.cargo/config.toml` binds ext/openssl's openssl-sys to openssl@4 in the fixture's `brew/Cellar/`, and `api_check::binding`'s test lays two stand-in php builds beside it, Mach-O files it writes since `permit --diff` reads every file as text, 1.0 loading libssl from openssl@4, which holds, and 2.0 from openssl@5, which fails naming both | fail |
| `mirrors` | a surface item's `#[mirrors]` naming what the pinned header does not declare `PHPAPI`, `PDO_API` or `ZEND_API`, `php_random.h`'s `static inline` `php_random_default_engine`, and one naming a header the tree lacks, beside one naming `php_random_default_algo`, which holds (#2353); the test hands the fixture's `php-src/` as the pinned tree | fail |
