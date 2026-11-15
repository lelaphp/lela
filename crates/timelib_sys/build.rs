//! timelib, compiled from the sources vendored at `lib/` (design §7.2,
//! §12's row on vendored C): `php-src/ext/date/lib` at the tag
//! `oracle.toml` pins, byte for byte, with its bundled timezone database
//! (`timezonedb.h`). The file list and the flags are `config0.m4`'s
//! `timelib_sources` and `PHP_TIMELIB_CFLAGS`; nothing is discovered on
//! the system. `HAVE_TIMELIB_CONFIG_H` is defined, and `include/` holds
//! the `timelib_config.h` php's `config0.m4` writes, so timelib allocates
//! with `emalloc`, `ecalloc`, `erealloc`, `estrdup`, `estrndup` and
//! `efree` as php's build does; `php_heap` defines them, and they reach
//! the request's heap under its lend (ADR-0014). It is optimised as php
//! is, and floating-point expressions are not contracted into fused
//! multiply-adds, so `astro.c`'s sunrise hours round as the oracle's
//! build rounds them, to the last bit.

const SOURCES: [&str; 10] = [
    "astro.c",
    "dow.c",
    "parse_date.c",
    "parse_tz.c",
    "parse_posix.c",
    "timelib.c",
    "tm2unixtime.c",
    "unixtime2tm.c",
    "parse_iso_intervals.c",
    "interval.c",
];

fn main() {
    let mut build = cc::Build::new();
    // php compiles C and C++ with -DNDEBUG unless it is built with
    // --enable-debug or --enable-debug-assertions (Z/configure.ac:840), the
    // two builds that set ZEND_DEBUG, as debug assertions are this build's:
    // assert() is live exactly where Rust's debug_assertions is.
    if std::env::var_os("CARGO_CFG_DEBUG_ASSERTIONS").is_none() {
        build.define("NDEBUG", None);
    }
    build
        .include("include")
        .include("lib")
        .define("HAVE_TIMELIB_CONFIG_H", "1")
        .define("HAVE_STDINT_H", "1")
        .define("HAVE_SYS_TYPES_H", "1")
        .define("HAVE_UNISTD_H", "1")
        .define("HAVE_STRING_H", "1")
        .define("HAVE_STRTOLL", "1")
        .define("HAVE_ATOLL", "1")
        .flag_if_supported("-fwrapv")
        .flag_if_supported("-Wno-implicit-fallthrough")
        .opt_level(2)
        .flag_if_supported("-ffp-contract=off")
        .warnings(false);
    for source in SOURCES {
        build.file(format!("lib/{source}"));
    }
    build.compile("timelib");
    println!("cargo:rerun-if-changed=lib");
    println!("cargo:rerun-if-changed=include");
    println!("cargo:rerun-if-changed=build.rs");
}
