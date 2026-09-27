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
├── lexbor
├── libxml
├── mbstring
├── openssl
├── pcntl
├── pcre
├── pdo
├── pdo_mysql
├── pdo_sqlite
├── posix
├── random
├── reflection
├── session
├── spl
├── standard
├── tokenizer
└── zlib
```
<!-- /computed in=5291ec8bb981a305672f5335e63fa1959e5610233bd94b624ace06f957f19b74 out=cb14a5d5f955ffd39fb186a2af27cf66014759dd2dec3fa214eb592837b68f26 -->

<!-- computed exec cmd="cargo xtask status --section bundle" inputs=../seams.toml name=bundle-order | do not edit; run computed -->

| crate | order | extensions | reading | message |
|---|---|---|---|---|
| php_bundle | topological | 22 | pass | 23 workspace dependencies within its edges |

<!-- /computed in=dc9103db0b7a056364495f98e5a4b322f1aa27a883fb7ea15e387502af3c7daf out=c83977b4b1366584458d3ba6148cab21edcf9c1818959dd24c8a18f968a67a23 -->
