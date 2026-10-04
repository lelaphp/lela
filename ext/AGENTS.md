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
├── zip
└── zlib
```
<!-- /computed in=0bd03d4df2acee3f9ecb6b2bf9b917c7b48bec9e4886778d999faa7bae453bd3 out=1fcaa6c617de5a5f0399d34a71476fd4fe4e740c696af2ff9496a41ee9c279d1 -->

<!-- computed exec cmd="cargo xtask status --section bundle" inputs=../seams.toml name=bundle-order | do not edit; run computed -->

| crate | order | extensions | reading | message |
|---|---|---|---|---|
| php_bundle | topological | 58 | pass | 59 workspace dependencies within its edges |

<!-- /computed in=233dc9c877d47a64a834a9aa24dffbccc50d17d1455f0d0171492a462ad9c3d6 out=360fd3e48a6d45602b2bf777e5188c01b7d2d67612b8efe8cb406c0f3ceea156 -->
