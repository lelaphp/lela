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
├── exif
├── filter
├── ftp
├── hash
├── iconv
├── json
├── lexbor
├── libxml
├── mbstring
├── mysqli
├── mysqlnd
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
├── shmop
├── simplexml
├── sockets
├── spl
├── standard
├── sysvmsg
├── sysvsem
├── sysvshm
├── tokenizer
├── uri
├── xml
├── xmlreader
├── xmlwriter
└── zlib
```
<!-- /computed in=b45261de79ff108f9b5c23b55af2edc68ae62ef1e7883eb34ed34825185cec1e out=fac438bb5c6a64c75dca9c04280e72b9cac8eba4ce711f8ffa1ccb497b390f2e -->

<!-- computed exec cmd="cargo xtask status --section bundle" inputs=../seams.toml name=bundle-order | do not edit; run computed -->

| crate | order | extensions | reading | message |
|---|---|---|---|---|
| php_bundle | topological | 43 | pass | 44 workspace dependencies within its edges |

<!-- /computed -->
