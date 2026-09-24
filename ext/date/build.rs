//! timelib, compiled from the sources vendored at `lib/` (design §7.2,
//! §12's row on vendored C): `php-src/ext/date/lib` at the tag
//! `oracle.toml` pins, byte for byte, with its bundled timezone database
//! (`timezonedb.h`). The file list and the flags are `config0.m4`'s
//! `timelib_sources` and `PHP_TIMELIB_CFLAGS`; nothing is discovered on
//! the system. `HAVE_TIMELIB_CONFIG_H` is left undefined, so timelib
//! allocates with the C library's `malloc` and `free` where php's build
//! points it at `emalloc` and `efree`.

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
    build
        .include("lib")
        .define("HAVE_STDINT_H", "1")
        .define("HAVE_SYS_TYPES_H", "1")
        .define("HAVE_UNISTD_H", "1")
        .define("HAVE_STRING_H", "1")
        .define("HAVE_STRTOLL", "1")
        .define("HAVE_ATOLL", "1")
        .flag_if_supported("-fwrapv")
        .flag_if_supported("-Wno-implicit-fallthrough")
        .warnings(false);
    for source in SOURCES {
        build.file(format!("lib/{source}"));
    }
    build.compile("timelib");
    println!("cargo:rerun-if-changed=lib");
    println!("cargo:rerun-if-changed=build.rs");
}
