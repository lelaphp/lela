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
├── dba
├── dom
├── exif
├── fileinfo
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
├── soap
├── sockets
├── spl
├── sqlite3
├── standard
├── sysvmsg
├── sysvsem
├── sysvshm
├── tokenizer
├── uri
├── xml
├── xmlreader
├── xmlwriter
├── zip
└── zlib
```
<!-- /computed in=39e6364ca6ca915336a81f169a0e8e31937b844509cd2ba267dc1a711edf6b91 out=e9cce2463611ad873b6aba06904039dafc31959d2bb7d14b2c4e81e9b79bf70b -->

<!-- computed exec cmd="cargo xtask status --section bundle" inputs=../seams.toml name=bundle-order | do not edit; run computed -->

| crate | order | extensions | reading | message |
|---|---|---|---|---|
| php_bundle | topological | 48 | pass | 49 workspace dependencies within its edges |

<!-- /computed in=89c686ba402dd4027328a9bde0f279ffb429f4d9c23c72629d89a08b0dacba07 out=b0d8e67b88b1fc2f9de8e644d712fc8a6bbaeaff89f1078952497bf609f6cc46 -->
