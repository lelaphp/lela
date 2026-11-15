//! libgd is the keg's (design §7.2, revision 75): the library the
//! oracle links, found through `pkg-config gdlib` and never vendored, its
//! declarations written in `src/ffi/`. The oracle links the shared
//! library, and neither the SDK's `/usr/lib`, which the SDK's libxml2
//! puts ahead of the kegs on the link line, nor the SDK ships a
//! libgd, so `-l gd` finds the keg's whatever the order, as libgmp's
//! does. A host `pkg-config` finds no libgd on fails the build.
//!
//! `src/ffi/error.c` is php's `php_gd_error_method`, which libgd calls
//! with a `va_list` Rust cannot take; it is built by `cc` against the
//! keg's `gd.h`.

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed=src/ffi/error.c");
    println!("cargo::rerun-if-env-changed=PKG_CONFIG_PATH");
    let library = match pkg_config::Config::new()
        .atleast_version("2.3")
        .probe("gdlib")
    {
        Ok(library) => library,
        Err(error) => {
            println!(
                "cargo::error=pkg-config gdlib: {error}; install the libgd the oracle links, Homebrew's `gd` keg on a Mac (design §7.2)"
            );
            return;
        }
    };
    let mut build = cc::Build::new();
    // php compiles C and C++ with -DNDEBUG unless it is built with
    // --enable-debug or --enable-debug-assertions (Z/configure.ac:840), the
    // two builds that set ZEND_DEBUG, as debug assertions are this build's:
    // assert() is live exactly where Rust's debug_assertions is.
    if std::env::var_os("CARGO_CFG_DEBUG_ASSERTIONS").is_none() {
        build.define("NDEBUG", None);
    }
    build.file("src/ffi/error.c").warnings(true);
    for dir in &library.include_paths {
        build.include(dir);
    }
    build.compile("php_gd_error");
}
