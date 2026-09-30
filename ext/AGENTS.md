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
├── bcmath
├── bz2
├── calendar
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
├── opcache
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
├── simplexml
├── sockets
├── spl
├── standard
├── tokenizer
├── uri
├── xmlwriter
└── zlib
```
<!-- /computed in=978ac3bea842a974de4458c8f5d24dc5cd87be54a07af683618e14bd477ba031 out=48a228532882dd0d200122775dbcdcc0c1015a7fbd6dec601dee30a7e5ba35fa -->

<!-- computed exec cmd="cargo xtask status --section bundle" inputs=../seams.toml name=bundle-order | do not edit; run computed -->

| crate | order | extensions | reading | message |
|---|---|---|---|---|
| php_bundle | topological | 33 | pass | 34 workspace dependencies within its edges |

<!-- /computed in=1e2f6b9651dd49e901bba934c5ce249f87c60d5964347951aa3e7d56942dcd07 out=7b24621d601049f54c12f4dc2c88a82c3b775d3e27a2985d288c258752def651 -->
