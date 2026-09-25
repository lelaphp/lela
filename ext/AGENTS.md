# Extensions

One crate per extension under `ext/` (ADR-0004), laid down by `xtask new-ext <name>`. The template's rules:

- One dependency, `php_ext`; the engine is out of reach (`[[edge]]` `ext/* → php_ext only`).
- One entry, `extension()`; anything `pub` beyond it is printed by `api-check` and is a row of the design's list of surfaces (§4.10): what the php header a dependent extension includes declares `PHPAPI`, item by item, and nothing more.
- State is on the value: request-scoped or immutable; no lock or cell (ADR-0005, `ext.no-locks`). Cross-request caching is `api::Cache`.
- Tests go through `Capture`; every native carries its witness in rustdoc, with `exit=` written by `xtask witness --accept` only.
- The phpt subset is `php-src/ext/<name>/tests`, run with `xtask phpt --subset`.

<!-- computed tree src=. depth=1 dirs name=extensions | do not edit; run computed -->
```
.
├── ctype
├── curl
├── date
├── dom
├── filter
├── hash
├── iconv
├── json
├── libxml
├── mbstring
├── openssl
├── pcre
├── pdo
├── pdo_mysql
├── pdo_sqlite
├── random
├── reflection
├── session
├── spl
├── standard
└── tokenizer
```
<!-- /computed in=7b244187e1e8c15be170f38dac055862a57d16aa52f3d55ce9988238453a18d8 out=c6b813903caafe16c363ab6f06b8960e05e3c8720a6da40aeb624a82bcb0a691 -->

<!-- computed exec cmd="cargo xtask status --section bundle" inputs=../seams.toml name=bundle-order | do not edit; run computed -->

| crate | order | extensions | reading | message |
|---|---|---|---|---|
| php_bundle | topological | 21 | pass | 22 workspace dependencies within its edges |

<!-- /computed in=cb1d77455fd027a7d1392d8b2a3de72ebaeade982e08fa5f6d3b072e3a51c976 out=c203f9f4e30371022da07152bfbd18889f04dbc9832034bf335c43cebacf1d5e -->
