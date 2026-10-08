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
| `edge`      | a crate dependency outside its `[[edge]]` rows  | fail |
| `harness-edge` | a crate no `[[edge]]` row names depending on a workspace crate: the harness linking `php_bundle`, which the tracked manifest forbids since #2213 | fail |
| `host-edge` | a host under `sapi/` depending on `php_engine` past its facade (ADR-0008), beside one within its edges | fail |
| `deny-edge` | a `use` path across a `deny = true` module edge | fail |
| `bundle`    | a bundle that lists an extension before one it depends on, behind no feature: the order written is php's and free under `order = "registration"` (#2255) | pass |
| `registration` | under `order = "registration"` (#1778, #1882), which the test sets on the tracked manifest's `php_bundle` row: a bundle feature that does not switch on the feature of an extension its extension links, beside one that does and a dev-dependency that requires nothing; the test also lays it down in a scratch workspace with its bundle's `src/lib.rs` replaced, for a required extension registered nowhere, a registration behind a `cfg` the instrument does not read, and one extension registered twice | fail |
| `harness-lock` | a public item of the harness with no line in `xtask.lock`, under the rows #55 proposes | fail |
| `ops-lock`  | a hand-edited line of `ops.lock`, the lock rendered from `ops.toml` | fail |
| `tier-face` | a sectioned lock (#1369): php_sapi importing a `tier::*` item `tier.lock`'s `[face]` does not list, which `sapi.lock` does not hold either | fail |
| `same-name` | two `pub` types of one name in two modules, each listing its own methods alone (#79); read by the unit test beside `Inventory::assoc`, not by `hold` | pass |
| `surfaces` | an `ext/*` export on no row of the fixture's own design §4.10 list of surfaces (#1781, #1826): a member a `` `Regex`, with `compile` `` row does not write, and a function no row writes; `clean` carries a design listing its one export | fail |
