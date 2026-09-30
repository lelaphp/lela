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
└── zlib
```
<!-- /computed in=68b59b8465a35132ea6d1116d0f052e79054db2512a974eed4d3bc01f14a3e9d out=64b0c19b6631eed11c5d53148f4c60985492c309d609343c051dc5de1fed1b60 -->

<!-- computed exec cmd="cargo xtask status --section bundle" inputs=../seams.toml name=bundle-order | do not edit; run computed -->

| crate | order | extensions | reading | message |
|---|---|---|---|---|
| php_bundle | topological | 33 | pass | 34 workspace dependencies within its edges |

<!-- /computed in=ac10430af46c0ba35491d0658c3fe6aadf2dbc986ad4f02e06a785531e320d79 out=7b24621d601049f54c12f4dc2c88a82c3b775d3e27a2985d288c258752def651 -->
