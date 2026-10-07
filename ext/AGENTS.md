# Extensions

One crate per extension under `ext/` (ADR-0004), laid down by `xtask new-ext <name>`. The template's rules:

- One dependency, `php_ext`; the engine is out of reach (`[[edge]]` `ext/* → php_ext only`).
- One entry, `extension()`; anything `pub` beyond it is printed by `api-check` and is a row of the design's list of surfaces (§4.10): what the php header a dependent extension includes declares `PHPAPI`, item by item, and nothing more.
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

<!-- computed exec cmd="cargo xtask status --section bundle" inputs=../seams.toml name=bundle-order | do not edit; run computed -->

| crate | order | extensions | reading | message |
|---|---|---|---|---|
| php_bundle | registration | 59 | pass | 60 workspace dependencies within its edges |

<!-- /computed in=aca438217a65d4d94f9c048c8b0ebb9ea41ca72615b868f769465cb4dea74b0a out=efacbf76c0f18b14a095721bb320f198897b8bce374beb7e52c69614edfee39a -->
