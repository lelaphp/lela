---
status: accepted
date: 2026-09-05
supersedes: third-engine-design.r3.md §4.1 (`php_ext` as one crate; `php_ext::enabled()`), §4.2 (the `php_ext → php_engine` edge), §7.1 (`php_ext::enabled()` returns the static list)
---

# Extensions are one crate each under `ext/`, with `php_ext` as the facade below them and `php_bundle` as the enabled set above them

Revision 3 kept all nineteen ported extensions in one crate of 105k lines. That shape rebuilds everything on any native edit, hides one extension's reach into another behind module privacy, and leaves a helper two extensions need with no home but a shared module that grows without an owner, which is how Mahout's `mahout_runtime` became a dependency of all nineteen of its extension crates. The third engine takes php-src's layout instead: one crate per extension at `ext/<name>/`, named `php_ext_<name>`, so the directory joins directly to `php-src/ext/<name>/tests` and its stub files. Below every extension sits `php_ext`, a page of re-exports: `php_engine::api` and the three attributes from `php_macros`, nothing defined. The macros expand to paths in its private module, so an extension has exactly one workspace dependency and cannot name `php_engine` at all; the "api only" rule becomes crate visibility rather than a lint over the bodies. Above every extension sits `php_bundle`, one cargo feature per non-default extension and an `enabled()` list in module-startup order, which is php-src's `internal_functions.c`. Cargo forces the two crates to be two: the crate every extension depends on cannot also depend on every extension. Neither is deep; their depth is the engine's, and each passes the deletion test because what they hold would otherwise reappear in nineteen manifests or in four hosts.

## Considered options

- **One crate, one module per extension (revision 3).** Rejected for the reasons above; the cross-extension edges that php-src states with `ZEND_MOD_REQUIRED` are invisible in it.
- **Extension crates depending on `php_engine` and `php_macros` directly, with `xtask api-check` scanning `use` paths.** Rejected: the macros would expand to `::php_engine::` paths, so the engine's host face is one path away from every native body, and the rule lives in a scanner instead of the type system.
- **Moving `api` out of `php_engine` into `php_ext`.** Rejected: ADR-0002 makes every entry take the engine's concrete `Request`, and the marshalling traits need `Zval` and the kernels. `php_ext` re-exports the api; it does not define it.
- **One crate that both re-exports `api` and lists the enabled extensions.** Impossible: the extensions depend on it and it would depend on them.
- **A shared helper crate under `ext/` for what several extensions need.** Rejected: it is the grab-bag this decision exists to prevent. A helper is engine policy and enters `api`, or belongs to the extension that owns it in php-src, which exports it and becomes a declared dependency.
- **The bundle owning lifecycle, ordering or linking.** Rejected: the linker needs the linked tables and the interner, which are engine internals; moving them out widens `api`. The bundle is data.

## Consequences

- Workspace members are `crates/*`, `ext/*`, `xtask`. A new extension is a directory, one manifest line in `php_bundle` and one entry in its list.
- An extension may depend on another when its php-src counterpart includes that extension's headers (the pdo drivers on `pdo`; `standard` on `random`). Each such `pub` surface is deliberate and small, and `api-check` prints anything beyond `extension()`.
- `api.lock` is generated from `php_ext`'s public items, which are the whole crate.
- `unsafe` is permitted in `php_heap` and `ext/*/src/ffi/` only. *Amended 2026-09-07 by ADR-0006: `php_jit::table::code` and `php_jit::enter` are the third and fourth homes; `xtask unsafe-audit` names the four.*
- Extension tests link the engine with that crate and its dependencies and the `Capture` sink (ADR-0002); they never dev-depend on `php_bundle`. An engine with no extensions must serve a Zend-only script, and that is a witness in `php_engine`.
- A future reader will want to fold the facade back into `php_engine` "since it only re-exports". The engine cannot re-export the attributes, because `php_macros` depends on it; the facade is the only place above both. This file is the answer.
