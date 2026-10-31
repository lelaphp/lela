//! The libraries libzip links beside zlib, and the C this crate compiles
//! against their headers.
//!
//! liblzma and libzstd are the libraries the oracle's libzip loads
//! (design §7.2; #3529): found through `pkg-config liblzma` and
//! `pkg-config libzstd`, which on an Apple silicon Mac are Homebrew's
//! `xz` and `zstd` kegs, never vendored. A miss fails the build rather
//! than falling back to a library the oracle does not load. liblzma is
//! linked from the keg's archive, held inside this crate's rlib: the SDK
//! ships a `liblzma.tbd`, and its libxml2 puts `-L /usr/lib` ahead of the
//! kegs on the link line (#1510), where `-l lzma` would find the SDK's
//! library. The SDK ships no libzstd, so `-l zstd` finds the keg's
//! whatever the order. A host whose liblzma has no archive links its
//! shared library.
//!
//! The C is one file: `ZLIB_VERSION` of the `zlib.h` this crate's deflate
//! compiles against, the string libzip's `deflateInit2` macro hands
//! `deflateInit2_` (#2426), under the include directories libz-sys hands
//! a dependent as `DEP_Z_INCLUDE` where it names any, else the
//! compiler's own, which for the platform's zlib is the SDK's or the
//! system's header; and the sizes of `lzma_stream` and
//! `lzma_options_lzma`, which `src/ffi/lzma.rs` lays out and its test
//! holds to them. ext/zlib reads the same zlib header for its own
//! constants (its `build.rs`), as php's ext/zlib and libzip are each
//! compiled against it; no extension reads the other's.

use std::env;
use std::fs;
use std::path::PathBuf;

/// The constants, the version NUL-terminated as the macro expands to a C
/// string.
const SOURCE: &str = "#include <stddef.h>\n#include <zlib.h>\n#include <lzma.h>\n\
const char php_zip_zlib_version[] = ZLIB_VERSION;\n\
const size_t php_zip_lzma_stream_size = sizeof(lzma_stream);\n\
const size_t php_zip_lzma_options_size = sizeof(lzma_options_lzma);\n";

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=DEP_Z_INCLUDE");
    let lzma = match pkg_config::Config::new()
        .atleast_version("5.2")
        .statik(true)
        .probe("liblzma")
    {
        Ok(library) => library,
        Err(error) => {
            println!(
                "cargo::error=pkg-config liblzma: {error}; install the liblzma the oracle's libzip loads, Homebrew's `xz` keg on a Mac (design §7.2)"
            );
            return;
        }
    };
    if let Err(error) = pkg_config::Config::new()
        .atleast_version("1.4")
        .probe("libzstd")
    {
        println!(
            "cargo::error=pkg-config libzstd: {error}; install the libzstd the oracle's libzip loads, Homebrew's `zstd` keg on a Mac (design §7.2)"
        );
        return;
    }
    let Some(out) = env::var_os("OUT_DIR").map(PathBuf::from) else {
        println!("cargo::error=OUT_DIR is unset");
        return;
    };
    let source = out.join("headers.c");
    if let Err(error) = fs::write(&source, SOURCE) {
        println!("cargo::error=headers.c: {error}");
        return;
    }
    let mut build = cc::Build::new();
    build.file(&source);
    let includes = env::var("DEP_Z_INCLUDE").unwrap_or_default();
    for include in includes.split(',').filter(|dir| !dir.is_empty()) {
        build.include(include);
    }
    for include in &lzma.include_paths {
        build.include(include);
    }
    for (name, value) in &lzma.defines {
        build.define(name, value.as_deref());
    }
    if let Err(error) = build.try_compile("php_zip_headers") {
        println!(
            "cargo::error=compiling ZLIB_VERSION from <zlib.h> and liblzma's sizes from <lzma.h>: {error}"
        );
    }
}
