# Third-party notices

How to read this: component | where in this project | origin | licence | notice text (verbatim).

Where the last column names a file in `LICENSES/`, that file is the notice text, copied byte for byte from the source named beside it: the component's own licence file, an item of php-src's `README.REDIST.BINS`, or the comment that carries the notice in php-src's source, all at php-src's tag php-8.5.12. Where it names "the first comment" of a php-src file, the notice is a public-domain dedication that stays in that file, and `LICENSES/` holds no copy. php-src's own row names `LICENSES/php-src.txt`, which is not taken at that tag: it is the PHP License, version 4, copied byte for byte from php.net's `https://www.php.net/license/4_0.txt`. Paths under "origin" are php-src's. `NOTICE` says how the code derived from php-src itself is used.

## 1. php-src (derived)

| component | where in this project | origin | licence | notice text |
|---|---|---|---|---|
| php-src | `ops.toml`; every file that cites a php-src source as `Z/<path>`; `crates/php_heap/src/iterators.rs`, `object.rs` and `resource.rs`, `crates/php_syntax/src/grammar/expr.rs`, `ext/standard/src/html/tables.rs` and `sapi/cli/src/server/client.rs` (`php_cli_server_css`), which copy or transcribe it; `ext/lexbor/config.m4`, `ext/uri/config.m4`, `ext/mbstring/rare_cp_bitvec.h` and `ext/mbstring/unicode_data.h`, which are copies | php-src | BSD-3-Clause (PHP License 4 / Zend Engine License 3.0) | `NOTICE`; `LICENSES/php-src.txt` |

## 2. Vendored C, copies of php-src's

| component | where in this project | origin | licence | notice text |
|---|---|---|---|---|
| lexbor, with php-src's changes | `ext/lexbor/lexbor` | `ext/lexbor/lexbor` | Apache-2.0 | `LICENSES/lexbor.txt` and `LICENSES/lexbor-NOTICE.txt` (`ext/lexbor/LICENSE` and `ext/lexbor/NOTICE`) |
| timelib | `crates/timelib_sys/lib` | `ext/date/lib` | MIT | `LICENSES/timelib.txt` (`ext/date/lib/LICENSE.rst`) |
| libmbfl | `ext/mbstring/libmbfl`, its source; `ext/mbstring/build.rs` compiles it into the engine | `ext/mbstring/libmbfl` | LGPL-2.1-only | `LICENSES/libmbfl.txt` (`ext/mbstring/libmbfl/LICENSE`) |
| uriparser | `ext/uri/uriparser` | `ext/uri/uriparser` | BSD-3-Clause | `LICENSES/uriparser.txt` (`ext/uri/uriparser/COPYING.BSD-3-Clause`) |
| libmagic, and its compiled magic database | `ext/fileinfo/libmagic`; `ext/fileinfo/data_file.c`, the whole of php-src's generated file, which `libmagic/apprentice.c` includes and `ext/fileinfo/build.rs` compiles in. The generated file carries no notice of its own, and the crate's manifest takes it as libmagic's | `ext/fileinfo/libmagic`, `ext/fileinfo/data_file.c` | BSD-2-Clause-Darwin | `LICENSES/libmagic.txt` (`ext/fileinfo/libmagic/LICENSE`) |

`ext/mbstring/unicode_data.h` (§1) is php-src's file, generated from the Unicode Character Database by `ext/mbstring/ucgendat/ucgendat.php`, and `ext/mbstring/build.rs` reads its tables into Rust. The file carries no notice of its own. The OpenLDAP Public License that php-src lists under `README.REDIST.BINS` item 17 is the generator's, and this tree does not carry the generator; `LICENSES/ucgendat.txt` holds that item.

## 3. Ports (Rust translations)

| component | where in this project | origin | licence | notice text |
|---|---|---|---|---|
| strnatcmp, Martin Pool | `ext/standard/src/strnatcmp.rs`, an altered version | `ext/standard/strnatcmp.c` | Zlib | `LICENSES/strnatcmp.txt` (`README.REDIST.BINS` item 8) |
| scanf, from Tcl 8.3.0 | `ext/standard/src/scanf.rs` | `ext/standard/scanf.c` | TCL | `LICENSES/scanf.txt` (item 7) |
| FreeSec, David Burren | `ext/standard/src/crypt/freesec.rs` | `ext/standard/crypt_freesec.c` | BSD-3-Clause | `LICENSES/crypt-freesec.txt` (item 4) |
| crypt_blowfish, Solar Designer | `ext/standard/src/crypt/blowfish.rs` | `ext/standard/crypt_blowfish.c` | public domain, with a permissive licence in case that is void | `LICENSES/crypt-blowfish.txt` (item 5) |
| md5crypt, Poul-Henning Kamp | `ext/standard/src/crypt/md5_crypt.rs` | `ext/standard/php_crypt_r.c` (`php_md5_crypt_r`) | Beerware | `LICENSES/md5crypt.txt` (`php_crypt_r.c:17-31`) |
| SHA-crypt, Ulrich Drepper | `ext/standard/src/crypt/sha_crypt.rs` | `ext/standard/crypt_sha256.c`, `crypt_sha512.c` | public domain | the first comment of each file |
| MD5, Solar Designer | `ext/standard/src/md5.rs` | `ext/standard/md5.c`, `md5.h` | public domain | `md5.h`'s first comment |
| Mersenne Twister, Matsumoto, Nishimura and Wagner | `ext/random/src/engine_mt19937.rs` | `ext/random/engine_mt19937.c` | BSD-3-Clause | `LICENSES/mt19937.txt` (item 6) |
| uuencode, University of California, Berkeley | `ext/standard/src/uuencode.rs` | `ext/standard/uuencode.c` | BSD-4-Clause-UC | `LICENSES/uuencode.txt` (item 9) |
| xxHash, Yann Collet | `ext/hash/src/xxhash.rs` (XXH32 and XXH64) | `ext/hash/xxhash/xxhash.h` | BSD-2-Clause | `LICENSES/xxhash.txt` (item 19) |
| libavifinfo, Alliance for Open Media | `ext/standard/src/image/avif.rs` | `ext/standard/libavifinfo` | BSD-2-Clause | `LICENSES/libavifinfo.txt`, and the Alliance for Open Media Patent License 1.0 in `LICENSES/libavifinfo-PATENTS.txt` (`ext/standard/libavifinfo/LICENSE` and `PATENTS`) |
| MurmurHash3 (PMurHash), Austin Appleby and Shane Day | `ext/hash/src/murmur.rs` | `ext/hash/murmur/PMurHash.c`, `PMurHash128.c` | public domain | the first comment of each file |
| tar, Tim Kientzle (libarchive) | `ext/phar/src/tar.rs` (`phar_tar_octal`) | `ext/phar/tar.c` | BSD-2-Clause | `LICENSES/phar-tar.txt` (item 14) |
| libbcmath, GNU bc's number package | `ext/bcmath/src/num.rs` | `ext/bcmath/libbcmath` | LGPL-2.1-or-later | `LICENSES/libbcmath.txt` (`ext/bcmath/libbcmath/LICENSE`) |
| dtoa, David M. Gay | `crates/php_heap/src/numeric/dtoa.rs` | `Zend/zend_strtod.c` (`zend_dtoa`) | dtoa | `LICENSES/dtoa.txt` (`zend_strtod.c:1-18`) |
| std::sort, LLVM libc++ | `crates/php_heap/src/sort.rs` | `Zend/zend_sort.c` (`zend_sort`) | NCSA OR MIT | `LICENSES/libcxx-sort.txt` (`zend_sort.c:165-247`) |
| glob(3), University of California, Berkeley, by way of OpenBSD | `ext/standard/src/dir/php_glob.rs` | `main/php_glob.c` | BSD-3-Clause | `LICENSES/php-glob.txt` (`php_glob.c:1-31`) |
| http_parser, Ryan Dahl | `crates/php_proto/src/http.rs` (the request half) | `sapi/cli/php_http_parser.c` | MIT | `LICENSES/http-parser.txt` (`php_http_parser.c:1-20`) |
| ps_status, PostgreSQL | `sapi/cli/src/ffi/title.rs` | `sapi/cli/ps_title.c` | PostgreSQL | `LICENSES/ps-title.txt` (`ps_title.c:1-29`) |

## 4. Followed, derivation not checked file by file

These files follow a php-src function that carries a third-party notice, or a library's own structures. Whether each is derived from that code has not been checked file by file; the notice is carried in case it is.

| component | where in this project | origin | licence | notice text |
|---|---|---|---|---|
| snprintf, Todd C. Miller and the Apache Group | `ext/standard/src/formatted_print.rs` (`php_conv_fp`), `ext/standard/src/var/debug.rs` | `main/snprintf.c` | Apache-1.0, and an ISC-style notice | `LICENSES/snprintf.txt` (item 10) |
| tempnam, University of California | `crates/php_engine/src/api/request/temporary.rs` (`php_do_open_temporary_file`) | `main/php_open_temporary_file.c` | BSD-4-Clause-UC | `LICENSES/tempnam.txt` (`php_open_temporary_file.c:55-88`) |
| libzip, Dieter Baron and Thomas Klausner | `ext/zip/src`, libzip's archive and directory entries re-derived | libzip (`zipint.h`, `zip_dirent.c`) | BSD-3-Clause | `LICENSES/libzip.txt` (item 15, the notice php-src carries for `zip_dirent.c`) |

## 5. Linked, not vendored

This section lists every C library the engine and its hosts link that is not in this tree: the platform's, which a crate's build script finds or its `#[link]` names, and one built from the source a crates.io crate bundles. None is part of this source distribution, and each is under its own licence, which travels with the library. `.cargo/config.toml` makes the links to libpcre2-8 and libsqlite3 static, and `ext/tidy/build.rs` links tidy statically where it finds `libtidy.a`.

| library | linked by |
|---|---|
| Oniguruma | `ext/mbstring` |
| libgd | `ext/gd` |
| libxml2 | `ext/libxml`, `ext/xsl` |
| libxslt, libexslt | `ext/xsl` |
| libcurl | `ext/curl` |
| OpenSSL's libssl and libcrypto | `ext/openssl` |
| SQLite | `ext/pdo_sqlite` |
| zlib | `ext/zlib`, `ext/zip` |
| PCRE2 (libpcre2-8) | `ext/pcre`, `ext/fileinfo` |
| GMP | `ext/gmp` |
| libsodium | `ext/sodium` |
| ICU (icuuc, icui18n, icuio) | `ext/intl` |
| libintl | `ext/gettext` |
| libedit | `ext/readline` |
| libffi | `ext/ffi` |
| liblzma, libzstd | `ext/zip` |
| tidy-html5 | `ext/tidy` |
| libiconv | `ext/iconv` |
| libresolv | `ext/standard` |
| AWS-LC, built from the source the `aws-lc-sys` crate bundles; its licence, as that crate's manifest states it, is `ISC AND (Apache-2.0 OR ISC) AND Apache-2.0 AND MIT AND BSD-3-Clause AND (Apache-2.0 OR ISC OR MIT) AND (Apache-2.0 OR ISC OR MIT-0)`, with its texts in the crate's `LICENSE` and `aws-lc/LICENSE` | `sapi/makala`, through rustls's aws-lc-rs provider |

## 6. Rust crates

The crates this project builds with are the ones `Cargo.lock` lists, each under the licence its own manifest states; `cargo metadata --format-version 1` prints each package's `license`. The list changes with `Cargo.lock`, so it is not typed here.

## 7. Harness data

The notice in this section is taken at the nginx release that `oracle.toml`'s `[fastcgi]` key `nginx` pins, not at php-src's tag: a bump of that key moves `LICENSES/nginx.txt` with it.

| component | where in this project | origin | licence | notice text |
|---|---|---|---|---|
| nginx's `fastcgi_params`, Igor Sysoev and Nginx, Inc. | `oracle.toml`, `[fastcgi]`: `params`, the text of the file, and `script_filename`, the line `conf/fastcgi.conf` adds | nginx `conf/fastcgi_params` and `conf/fastcgi.conf` at the tag `release-1.30.5`, the release `[fastcgi]`'s `nginx` key pins | BSD-2-Clause | `LICENSES/nginx.txt` (nginx's `LICENSE` at `release-1.30.5`) |
