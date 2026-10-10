//! `ZLIB_VERSION` of the `zlib.h` this crate's deflate compiles against,
//! the string libzip's `deflateInit2` macro hands `deflateInit2_` (#2426).
//! libzip is C compiled against the header, so the version is compiled
//! here as C: one constant initialised by the macro, under the include
//! directories libz-sys hands a dependent as `DEP_Z_INCLUDE` where it
//! names any, else the compiler's own, which for the platform's zlib is
//! the SDK's or the system's header. ext/zlib reads the same header for
//! its own constants (its `build.rs`), as php's ext/zlib and libzip are
//! each compiled against it; no extension reads the other's.

use std::env;
use std::fs;
use std::path::PathBuf;

/// The one constant, NUL-terminated as the macro expands to a C string.
const SOURCE: &str = "#include <zlib.h>\nconst char php_zip_zlib_version[] = ZLIB_VERSION;\n";

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=DEP_Z_INCLUDE");
    let Some(out) = env::var_os("OUT_DIR").map(PathBuf::from) else {
        println!("cargo::error=OUT_DIR is unset");
        return;
    };
    let source = out.join("zlib_version.c");
    if let Err(error) = fs::write(&source, SOURCE) {
        println!("cargo::error=zlib_version.c: {error}");
        return;
    }
    let mut build = cc::Build::new();
    build.file(&source);
    let includes = env::var("DEP_Z_INCLUDE").unwrap_or_default();
    for include in includes.split(',').filter(|dir| !dir.is_empty()) {
        build.include(include);
    }
    if let Err(error) = build.try_compile("php_zip_zlib_version") {
        println!("cargo::error=compiling ZLIB_VERSION from <zlib.h>: {error}");
    }
}
