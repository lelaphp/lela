# Extensions

One crate per extension under `ext/` (ADR-0004), laid down by `xtask new-ext <name>`. The template's rules:

- One dependency, `php_ext`; the engine is out of reach (`[[edge]]` `ext/* → php_ext only`).
- One entry, `extension()`; anything `pub` beyond it is what the php header a dependent extension includes declares `PHPAPI`, and nothing more. The edge to that dependent is a row of the design's list of surfaces (§4.10), a person's; the items are `ext/<name>/surface.lock`, which `api-check --write` rewrites and you land in the same diff as the code.
- State is on the value: request-scoped or immutable; no lock or cell (ADR-0005, `ext.no-locks`). Cross-request caching is `api::Cache`.
- Tests go through `Capture`; every native carries its witness in rustdoc, with `exit=` written by `xtask witness --accept` only.
- The phpt subset is every `.phpt` under `php-src/ext/<name>`, as run-tests.php walks the directory (`ext/intl/uchar/tests` is `ext/intl`'s), run with `xtask phpt --subset`.

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
├── ffi
├── fileinfo
├── filter
├── ftp
├── gd
├── gettext
├── gmp
├── hash
├── iconv
├── intl
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
├── readline
├── reflection
├── session
├── shmop
├── simplexml
├── soap
├── sockets
├── sodium
├── spl
├── sqlite3
├── standard
├── sysvmsg
├── sysvsem
├── sysvshm
├── tidy
├── tokenizer
├── uri
├── xml
├── xmlreader
├── xmlwriter
├── xsl
├── zend_test
├── zip
└── zlib
```
<!-- /computed in=073532650c3f2016b644e55d6199bb7d754ef49b4249dd271e1290d0193b545f out=1f6b1b2ad0d65a8a29ca4c6008bce6c0123d6b3a69b771b5e1a8310e039dcace -->

<!-- computed exec cmd="cargo xtask status --section bundle --tracked" inputs=../seams.toml,../xtask/src/status.rs name=bundle-order | do not edit; run computed -->

| crate | order | extensions | message |
|---|---|---|---|
| php_bundle | registration |  |  |

<!-- /computed in=ae8883527f38250200e8eb19904b034153628313fe86ed694f2b5e3764ca35a8 out=91983940b62ed840754cb91c6ed571841ceefb8b8c359d504e95bc1c50cdf1a5 -->
