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
└── zlib
```
<!-- /computed in=eb280cfa0b55069df3f360dd7864d899a52af7e952e621a81006372a04ab1ddc out=f16c1b52ef44967b537e9a464a42577e0b21fbb4f41f7c8d7daf61019f36a1af -->

<!-- computed exec cmd="cargo xtask status --section bundle" inputs=../seams.toml name=bundle-order | do not edit; run computed -->

| crate | order | extensions | reading | message |
|---|---|---|---|---|
| php_bundle | topological | 27 | pass | 28 workspace dependencies within its edges |

<!-- /computed in=742592614a0ee3130718ccc4c9b93ab53d243db368cd4c37c74c0ca37cc2aa4b out=16d18bf279a32f7d346709fccb43b7ad934f95e0d9cd27212abb00a20a815dee -->
