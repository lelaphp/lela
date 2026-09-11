# Extensions

One crate per extension under `ext/` (ADR-0004), laid down by `xtask new-ext <name>`. The template's rules:

- One dependency, `php_ext`; the engine is out of reach (`[[edge]]` `ext/* → php_ext only`).
- One entry, `extension()`; anything `pub` beyond it is printed by `api-check` and needs a line in the design (§12).
- State is on the value: request-scoped or immutable; no lock or cell (ADR-0005, `ext.no-locks`). Cross-request caching is `api::Cache`.
- Tests go through `Capture`; every native carries its witness in rustdoc, with `exit=` written by `xtask witness --accept` only.
- The phpt subset is `php-src/ext/<name>/tests`, run with `xtask phpt --subset`.

<!-- computed tree src=. depth=1 dirs name=extensions | do not edit; run computed -->
```
.
```
<!-- /computed in=c1e0a1f9e38e0dc30b7a78368a133864692469a48041f89b856691efb382f206 out=b347635fd4f0f31acca43658a179aaf5ab4d5c8dfd566be799a195d1c895e9d9 -->

<!-- computed exec cmd="cargo xtask status --section bundle" inputs=../seams.toml name=bundle-order | do not edit; run computed -->

| crate | order | extensions | reading | message |
|---|---|---|---|---|
| php_bundle | topological | 0 | pass | 1 workspace dependency within its edges |

<!-- /computed in=08247f044e91bb4598e3fb3bbefa613b8ad00733f2a538218fc0ba5bc11ae590 out=73c6ceb4391eabe35f7aeea995374bc24079858354e60c9a3e15b1488d8d371d -->
