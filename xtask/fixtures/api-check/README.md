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
| `ceiling`   | `api.lock` over `[budget] api_items`; the crate's `Cargo.toml` alone, its items laid down by the test at one past the ceiling the manifest carries (#338) | fail |
| `edge`      | a crate dependency outside its `[[edge]]` rows  | fail |
| `deny-edge` | a `use` path across a `deny = true` module edge | fail |
| `bundle`    | a bundle order that is not topological          | fail |
| `harness-lock` | a public item of the harness with no line in `xtask.lock`, under the rows #55 proposes | fail |
| `ops-lock`  | a hand-edited line of `ops.lock`, the lock rendered from `ops.toml` | fail |
| `same-name` | two `pub` types of one name in two modules, each listing its own methods alone (#79); read by the unit test beside `Inventory::assoc`, not by `hold` | pass |
