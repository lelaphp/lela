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
└── zlib
```
<!-- /computed in=ed86da7cfcc8a738e5ac468fcb8c9f9043480a769103d9d4d40a962d8de6c261 out=ba4d7005d32dc13dac780192dbf962fd53f3b6869645922ead1eec98b1faf83d -->

<!-- computed exec cmd="cargo xtask status --section bundle" inputs=../seams.toml name=bundle-order | do not edit; run computed -->

| crate | order | extensions | reading | message |
|---|---|---|---|---|
| php_bundle | topological | 44 | pass | 45 workspace dependencies within its edges |

<!-- /computed in=e7ef0967f21253a313b5245121d7aec6a3b5ed55cede00c60e61515ad68f3f86 out=a04f2c82e3fe55eb892aa5368b7ea76b7393f4d2fc36eee889a7d2b6b7974bc7 -->
