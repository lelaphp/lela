//! libxml2 is the platform's (design §7.2, #731): found through
//! `pkg-config libxml-2.0` and linked, never vendored, so the library the
//! engine reads is the one the oracle's build reads on the same machine.
//! The declarations in `src/ffi/sys.rs` are written to 2.9, which the
//! `pkg-config` holds the library to, but for the names a later release
//! exports in place of 2.9's: where the version `pkg-config` reports
//! exports `xmlXPathValuePush` and `xmlXPathValuePop`, the cfg
//! `libxml_xpath_value_push` says so ([`libxml2`]), and `sys.rs` links
//! those, as php's source resolves `valuePush` through the header it is
//! compiled against.
//!
//! `src/ffi/generic.c` is php's generic handler, which is C-variadic,
//! built by `cc` against libxml2's headers under the include directory
//! `pkg-config` names.

#[path = "build/libxml2.rs"]
mod libxml2;

fn main() {
    println!("cargo::rustc-check-cfg=cfg(libxml_xpath_value_push)");
    match pkg_config::Config::new()
        .atleast_version("2.9")
        .probe("libxml-2.0")
    {
        Ok(library) => {
            if libxml2::exports_xpath_value_push(&library.version) {
                println!("cargo::rustc-cfg=libxml_xpath_value_push");
            }
        }
        Err(error) => {
            println!("cargo:warning=pkg-config libxml-2.0: {error}");
            println!("cargo:rustc-link-lib=xml2");
        }
    }
    let mut build = cc::Build::new();
    build.file("src/ffi/generic.c").warnings(true);
    if let Ok(dir) = pkg_config::get_variable("libxml-2.0", "includedir") {
        build.include(std::path::Path::new(&dir).join("libxml2"));
    }
    build.compile("php_libxml_generic");
    println!("cargo:rerun-if-changed=src/ffi/generic.c");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build/libxml2.rs");
}
