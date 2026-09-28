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
├── bz2
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
├── phar
├── posix
├── random
├── reflection
├── session
├── sockets
├── spl
├── standard
├── tokenizer
├── uri
└── zlib
```
<!-- /computed in=d05d4ba9e83aa1b14b568d813d9f25d5fe349a1243db9fe099fd872e6dcc0cb7 out=7b7988f8f96a7284c3dae1db60e8877baa1b4858ebbb9e15bac0aefd3b404575 -->

<!-- computed exec cmd="cargo xtask status --section bundle" inputs=../seams.toml name=bundle-order | do not edit; run computed -->

| crate | order | extensions | reading | message |
|---|---|---|---|---|
| php_bundle | topological | 28 | pass | 29 workspace dependencies within its edges |

<!-- /computed in=5be7465f0c67de57cd2c24feadc6d9a5db71f21da0235db55873cb59121348df out=4f8283eed28703eace5321b25bbc58489ad945aa783a131e0086b0bc85ba9de8 -->
