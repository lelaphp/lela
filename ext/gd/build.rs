//! libgd is the keg's (design §7.2, revision 75; #1573): the library the
//! oracle links, found through `pkg-config gdlib` and never vendored, its
//! declarations written in `src/ffi/`. The oracle links the shared
//! library, and neither the SDK's `/usr/lib`, which the SDK's libxml2
//! puts ahead of the kegs on the link line (#1510), nor the SDK ships a
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
    build.file("src/ffi/error.c").warnings(true);
    for dir in &library.include_paths {
        build.include(dir);
    }
    build.compile("php_gd_error");
}
